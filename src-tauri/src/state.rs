//! Shared application state: config, proxy server lifecycle, integrations.

use crate::codex;
use crate::config::AppConfig;
use crate::stats::{SharedStats, Stats};
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
    pub stats: SharedStats,
    server: RwLock<Option<ServerHandle>>,
}

struct ServerHandle {
    shutdown: oneshot::Sender<()>,
}

impl AppState {
    pub fn load() -> Self {
        Self {
            config: Arc::new(RwLock::new(AppConfig::load())),
            stats: Arc::new(RwLock::new(Stats::load())),
            server: RwLock::new(None),
        }
    }

    async fn persist(&self) -> anyhow::Result<()> {
        self.config.read().await.save()
    }

    /// Re-apply the Codex integration after a config change, but only when
    /// the user enabled it. Failures are logged, never fatal.
    async fn maybe_auto_apply(&self) {
        let cfg = self.config.read().await.clone();
        if !cfg.codex_integration {
            return;
        }
        if let Err(e) = codex::apply(&cfg, cfg.port) {
            tracing::warn!("auto-apply of Codex integration failed: {e}");
        } else {
            tracing::info!("Codex integration auto-applied after config change");
        }
    }

    pub async fn save_provider(&self, provider: crate::config::Provider) -> anyhow::Result<()> {
        self.config
            .write()
            .await
            .providers
            .insert(provider.id.clone(), provider);
        self.persist().await?;
        self.maybe_auto_apply().await;
        Ok(())
    }

    pub async fn delete_provider(&self, id: &str) -> anyhow::Result<()> {
        self.config.write().await.providers.remove(id);
        self.persist().await?;
        self.maybe_auto_apply().await;
        Ok(())
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
        self.persist().await?;
        self.maybe_auto_apply().await;
        Ok(())
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

    pub async fn server_status(&self) -> ServerStatus {
        let port = self.config.read().await.port;
        let running = self.server.read().await.is_some();
        ServerStatus {
            running,
            port,
            url: running.then(|| format!("http://127.0.0.1:{port}/v1")),
        }
    }

    pub async fn server_start(&self) -> anyhow::Result<ServerStatus> {
        let mut guard = self.server.write().await;
        if guard.is_some() {
            return Ok(self.status_with(true).await);
        }
        let port = self.config.read().await.port;
        let app = crate::proxy::router(self.config.clone(), self.stats.clone());
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
        drop(guard);
        self.maybe_auto_apply().await;
        Ok(self.status_with(true).await)
    }

    pub async fn server_stop(&self) -> anyhow::Result<ServerStatus> {
        let mut guard = self.server.write().await;
        if let Some(handle) = guard.take() {
            let _ = handle.shutdown.send(());
        }
        Ok(self.status_with(false).await)
    }

    async fn status_with(&self, running: bool) -> ServerStatus {
        let port = self.config.read().await.port;
        ServerStatus {
            running,
            port,
            url: running.then(|| format!("http://127.0.0.1:{port}/v1")),
        }
    }

    pub async fn codex_status(&self) -> codex::CodexStatus {
        codex::status(&self.config.read().await.clone())
    }

    pub async fn codex_apply(&self) -> anyhow::Result<()> {
        let cfg = {
            let mut guard = self.config.write().await;
            guard.codex_integration = true;
            guard.clone()
        };
        codex::apply(&cfg, cfg.port)?;
        self.persist().await
    }

    pub async fn codex_remove(&self) -> anyhow::Result<()> {
        codex::remove()?;
        self.config.write().await.codex_integration = false;
        self.persist().await
    }

    /// Fetch balance/quota for every enabled provider (best effort per
    /// provider; failures are reported inline, never fatal).
    pub async fn provider_balances(&self) -> Vec<ProviderBalance> {
        let cfg = self.config.read().await.clone();
        let mut out = Vec::new();
        for p in cfg.providers.values().filter(|p| p.enabled) {
            out.push(fetch_balance(p).await);
        }
        out
    }
}

/// One quota bar on the Overview card (e.g. "Weekly quota  52%").
#[derive(Debug, Clone, Serialize)]
pub struct QuotaBar {
    pub label: String,
    /// 0..100 remaining.
    pub percent: f64,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderBalance {
    pub provider_id: String,
    /// Credentials work (endpoint reachable and authorized).
    pub ok: bool,
    pub bars: Vec<QuotaBar>,
    pub balance_text: Option<String>,
    pub error: Option<String>,
}

fn quota_bar(label: &str, detail: &serde_json::Value) -> Option<QuotaBar> {
    let parse = |k: &str| {
        detail
            .get(k)
            .and_then(|v| v.as_str().map(str::to_string).or_else(|| Some(v.to_string())))
            .and_then(|s| s.trim_matches('"').parse::<f64>().ok())
    };
    let limit = parse("limit")?;
    let remaining = parse("remaining").unwrap_or(limit - parse("used").unwrap_or(0.0));
    if limit <= 0.0 {
        return None;
    }
    let reset = detail
        .get("resetTime")
        .and_then(serde_json::Value::as_str)
        .map(|s| s.chars().take(16).collect::<String>())
        .unwrap_or_default();
    Some(QuotaBar {
        label: label.to_string(),
        percent: (remaining / limit * 100.0).clamp(0.0, 100.0),
        detail: format!(
            "{} / {} left{}",
            remaining as u64,
            limit as u64,
            if reset.is_empty() {
                String::new()
            } else {
                format!(" · resets {reset}")
            }
        ),
    })
}

async fn fetch_balance(p: &crate::config::Provider) -> ProviderBalance {
    let base = p.base_url.trim_end_matches('/').to_string();
    let mut result = ProviderBalance {
        provider_id: p.id.clone(),
        ok: false,
        bars: Vec::new(),
        balance_text: None,
        error: None,
    };
    let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    else {
        result.error = Some("http client init failed".into());
        return result;
    };
    let get = |url: String| {
        let mut req = client.get(&url);
        if let Some(key) = &p.api_key {
            req = req.bearer_auth(key);
        }
        if let Some(ua) = &p.user_agent {
            req = req.header("user-agent", ua);
        }
        req
    };

    if base.contains("api.kimi.com/coding") {
        // Kimi Code quota: weekly allowance + rolling 5-hour window.
        match get(format!("{base}/usages")).send().await {
            Ok(res) if res.status().is_success() => {
                result.ok = true;
                if let Ok(body) = res.json::<serde_json::Value>().await {
                    if let Some(bar) = quota_bar("Weekly quota", &body["usage"]) {
                        result.bars.push(bar);
                    }
                    if let Some(window) = body["limits"].as_array().and_then(|a| a.first()) {
                        if let Some(bar) = quota_bar("5-hour window", &window["detail"]) {
                            result.bars.push(bar);
                        }
                    }
                }
            }
            Ok(res) => result.error = Some(format!("usages returned {}", res.status())),
            Err(e) => result.error = Some(e.to_string()),
        }
    } else if base.contains("openrouter") {
        match get(format!("{base}/credits")).send().await {
            Ok(res) if res.status().is_success() => {
                result.ok = true;
                if let Ok(body) = res.json::<serde_json::Value>().await {
                    let credits = body.pointer("/data/total_credits").and_then(serde_json::Value::as_f64).unwrap_or(0.0);
                    let used = body.pointer("/data/total_usage").and_then(serde_json::Value::as_f64).unwrap_or(0.0);
                    result.balance_text = Some(format!("${:.2}", credits - used));
                }
            }
            Ok(res) => result.error = Some(format!("credits returned {}", res.status())),
            Err(e) => result.error = Some(e.to_string()),
        }
    } else if base.contains("deepseek") {
        let root = base.trim_end_matches("/v1");
        match get(format!("{root}/user/balance")).send().await {
            Ok(res) if res.status().is_success() => {
                result.ok = true;
                if let Ok(body) = res.json::<serde_json::Value>().await {
                    if let Some(info) = body["balance_infos"].as_array().and_then(|a| a.first()) {
                        let amount = info["total_balance"].as_str().unwrap_or("?");
                        let currency = info["currency"].as_str().unwrap_or("");
                        result.balance_text = Some(format!("{amount} {currency}"));
                    }
                }
            }
            Ok(res) => result.error = Some(format!("balance returned {}", res.status())),
            Err(e) => result.error = Some(e.to_string()),
        }
    } else {
        // No known balance endpoint: report credential health only,
        // reusing the model-catalog probe.
        match list_models(p).await {
            Ok(_) => result.ok = true,
            Err(e) => result.error = Some(e.to_string()),
        }
    }
    result
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
