//! Shared application state: config, proxy server lifecycle, integrations.

use crate::codex;
use crate::config::AppConfig;
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::{oneshot, RwLock};

pub type SharedConfig = Arc<RwLock<AppConfig>>;

#[derive(Debug, Clone, Serialize)]
pub struct ServerStatus {
    pub running: bool,
    pub port: u16,
    pub url: Option<String>,
}

pub struct AppState {
    pub config: SharedConfig,
    server: RwLock<Option<ServerHandle>>,
}

struct ServerHandle {
    shutdown: oneshot::Sender<()>,
}

impl AppState {
    pub fn load() -> Self {
        Self {
            config: Arc::new(RwLock::new(AppConfig::load())),
            server: RwLock::new(None),
        }
    }

    async fn persist(&self) -> anyhow::Result<()> {
        self.config.read().await.save()
    }

    pub async fn save_provider(&self, provider: crate::config::Provider) -> anyhow::Result<()> {
        self.config
            .write()
            .await
            .providers
            .insert(provider.id.clone(), provider);
        self.persist().await
    }

    pub async fn delete_provider(&self, id: &str) -> anyhow::Result<()> {
        self.config.write().await.providers.remove(id);
        self.persist().await
    }

    pub async fn toggle_model(
        &self,
        provider_id: &str,
        model: &str,
        enabled: bool,
    ) -> anyhow::Result<()> {
        let mut cfg = self.config.write().await;
        let provider = cfg
            .providers
            .get_mut(provider_id)
            .ok_or_else(|| anyhow::anyhow!("unknown provider '{provider_id}'"))?;
        if let Some(m) = provider.models.iter_mut().find(|m| m.id == model) {
            m.enabled = enabled;
        } else {
            provider.models.push(crate::config::ProviderModel {
                id: model.to_string(),
                label: None,
                enabled,
            });
        }
        drop(cfg);
        self.persist().await
    }

    /// Live model discovery: GET {base_url}/models (OpenAI-compatible).
    pub async fn discover_models(&self, provider_id: &str) -> anyhow::Result<Vec<String>> {
        let cfg = self.config.read().await;
        let p = cfg
            .providers
            .get(provider_id)
            .ok_or_else(|| anyhow::anyhow!("unknown provider '{provider_id}'"))?;
        list_models(p).await
    }

    pub fn server_status(&self) -> ServerStatus {
        let port = self.config.blocking_read().port;
        let running = self.server.blocking_read().is_some();
        ServerStatus {
            running,
            port,
            url: running.then(|| format!("http://127.0.0.1:{port}/v1")),
        }
    }

    pub async fn server_start(&self) -> anyhow::Result<ServerStatus> {
        let mut guard = self.server.write().await;
        if guard.is_some() {
            return Ok(self.status_with(true));
        }
        let port = self.config.read().await.port;
        let app = crate::proxy::router(self.config.clone());
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });
        *guard = Some(ServerHandle { shutdown: tx });
        tracing::info!(port, "proxy listening on 127.0.0.1");
        Ok(self.status_with(true))
    }

    pub async fn server_stop(&self) -> anyhow::Result<ServerStatus> {
        let mut guard = self.server.write().await;
        if let Some(handle) = guard.take() {
            let _ = handle.shutdown.send(());
        }
        Ok(self.status_with(false))
    }

    fn status_with(&self, running: bool) -> ServerStatus {
        let port = self.config.blocking_read().port;
        ServerStatus {
            running,
            port,
            url: running.then(|| format!("http://127.0.0.1:{port}/v1")),
        }
    }

    pub fn codex_status(&self) -> codex::CodexStatus {
        codex::status(&self.config.blocking_read())
    }

    pub async fn codex_apply(&self) -> anyhow::Result<()> {
        let cfg = self.config.read().await.clone();
        codex::apply(&cfg, cfg.port)
    }

    pub fn codex_remove(&self) -> anyhow::Result<()> {
        codex::remove()
    }
}

/// Fetch a provider's live model catalog (also validates the API key).
pub async fn list_models(p: &crate::config::Provider) -> anyhow::Result<Vec<String>> {
    use crate::config::ProviderProtocol;
    let url = format!("{}/models", p.base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let mut req = client.get(&url);
    if let Some(ua) = &p.user_agent {
        req = req.header("user-agent", ua);
    }
    match p.protocol {
        ProviderProtocol::OpenAI => {
            if let Some(key) = &p.api_key {
                req = req.bearer_auth(key);
            }
        }
        ProviderProtocol::Anthropic => {
            // Anthropic has /v1/models with x-api-key auth.
            if let Some(key) = &p.api_key {
                req = req
                    .header("x-api-key", key)
                    .header("anthropic-version", "2023-06-01");
            }
        }
    }
    let res = req.send().await.map_err(|e| anyhow::anyhow!("request failed: {e}"))?;
    let status = res.status();
    let body: serde_json::Value = res
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("invalid JSON from provider: {e}"))?;
    if !status.is_success() {
        let msg = body
            .pointer("/error/message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown error");
        anyhow::bail!("provider returned {status}: {msg}");
    }
    let ids = body
        .get("data")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    m.get("id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                })
                .collect()
            })
        .unwrap_or_default();
    Ok(ids)
}
