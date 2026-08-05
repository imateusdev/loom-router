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

    pub async fn save_provider(&self, mut provider: crate::config::Provider) -> anyhow::Result<()> {
        let mut cfg = self.config.write().await;
        // The UI never receives the real key back, so an empty key on save
        // means "keep the existing one" — never overwrite with empty.
        let empty_key = provider
            .api_key
            .as_deref()
            .map(str::is_empty)
            .unwrap_or(true);
        if empty_key {
            if let Some(existing) = cfg.providers.get(&provider.id) {
                provider.api_key = existing.api_key.clone();
            }
        }
        provider.has_key = provider
            .api_key
            .as_deref()
            .map(|k| !k.is_empty())
            .unwrap_or(false);
        cfg.providers.insert(provider.id.clone(), provider);
        drop(cfg);
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
        let running = self.server.read().await.is_some();
        self.status_with(running).await
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
        let cfg = self.config.read().await.clone();
        // `codex::status` probes the CLI with a blocking `codex --version`;
        // keep it off the async executor (it runs on every screen open).
        tokio::task::spawn_blocking(move || codex::status(&cfg))
            .await
            .unwrap_or_else(|e| {
                // JoinError only happens if the probe panicked; report a
                // degraded status instead of panicking the command.
                tracing::warn!("codex status probe failed: {e}");
                codex::status(&crate::config::AppConfig::default())
            })
    }

    pub async fn codex_apply(&self) -> anyhow::Result<()> {
        let cfg = self.config.read().await.clone();
        codex::apply(&cfg, cfg.port)?;
        self.config.write().await.codex_integration = true;
        self.persist().await
    }

    pub async fn codex_remove(&self) -> anyhow::Result<()> {
        codex::remove()?;
        self.config.write().await.codex_integration = false;
        self.persist().await
    }

    /// Route Codex side/auxiliary calls (thread titles, probes) to a
    /// cheap/free "provider/model" slug. Persisted only; the proxy reads it
    /// live from the shared config.
    pub async fn set_side_call_fallback(&self, model: Option<String>) -> anyhow::Result<()> {
        self.config.write().await.side_call_fallback = model;
        self.persist().await
    }

    /// Toggle native slug mode (see codex.rs module docs). The merged
    /// catalog changes shape (bare slugs, no OpenAI-auth requirement), so
    /// re-apply the integration when it is active; a failed re-apply is
    /// logged by `maybe_auto_apply` and never blocks saving the preference.
    pub async fn set_native_slug_mode(&self, enabled: bool) -> anyhow::Result<()> {
        self.config.write().await.native_slug_mode = enabled;
        self.persist().await?;
        self.maybe_auto_apply().await;
        Ok(())
    }

    /// Fetch balance/quota for every enabled provider (best effort per
    /// provider; failures are reported inline, never fatal). Providers are
    /// probed concurrently so N slow providers don't serialize into N ×
    /// timeout of wall-clock latency.
    pub async fn provider_balances(&self) -> Vec<ProviderBalance> {
        let cfg = self.config.read().await.clone();
        let probes = cfg
            .providers
            .values()
            .filter(|p| p.enabled)
            .map(fetch_balance);
        futures::future::join_all(probes).await
    }
}

/// Shared HTTP client for provider probes: one connection pool and TLS
/// session cache for the whole app instead of rebuilding both per call.
fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .expect("failed to build shared HTTP client")
    })
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
            .and_then(|v| {
                v.as_str()
                    .map(str::to_string)
                    .or_else(|| Some(v.to_string()))
            })
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
    use crate::proxy::ProviderFamily;
    let base = p.base_url.trim_end_matches('/').to_string();
    let mut result = ProviderBalance {
        provider_id: p.id.clone(),
        ok: false,
        bars: Vec::new(),
        balance_text: None,
        error: None,
    };
    let client = http_client();
    let get = |url: String| {
        // Protocol-correct auth (Anthropic: x-api-key + anthropic-version;
        // others: Authorization bearer) shared with the proxy.
        let mut req = crate::proxy::apply_provider_auth(client.get(&url), p)
            // Per-probe timeout tighter than the client default.
            .timeout(std::time::Duration::from_secs(10));
        if let Some(ua) = &p.user_agent {
            req = req.header("user-agent", ua);
        }
        req
    };

    match crate::proxy::family_of(p) {
        // Kimi Code quota: weekly allowance + rolling 5-hour window. Only
        // the Coding Plan endpoint exposes /usages; other Kimi-family
        // endpoints fall through to the credential-health probe.
        ProviderFamily::Kimi if base.contains("api.kimi.com/coding") => {
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
        }
        ProviderFamily::OpenRouter => match get(format!("{base}/credits")).send().await {
            Ok(res) if res.status().is_success() => {
                result.ok = true;
                if let Ok(body) = res.json::<serde_json::Value>().await {
                    let credits = body
                        .pointer("/data/total_credits")
                        .and_then(serde_json::Value::as_f64)
                        .unwrap_or(0.0);
                    let used = body
                        .pointer("/data/total_usage")
                        .and_then(serde_json::Value::as_f64)
                        .unwrap_or(0.0);
                    result.balance_text = Some(format!("${:.2}", credits - used));
                }
            }
            Ok(res) => result.error = Some(format!("credits returned {}", res.status())),
            Err(e) => result.error = Some(e.to_string()),
        },
        ProviderFamily::DeepSeek => {
            let root = base.trim_end_matches("/v1");
            match get(format!("{root}/user/balance")).send().await {
                Ok(res) if res.status().is_success() => {
                    result.ok = true;
                    if let Ok(body) = res.json::<serde_json::Value>().await {
                        if let Some(info) = body["balance_infos"].as_array().and_then(|a| a.first())
                        {
                            let amount = info["total_balance"].as_str().unwrap_or("?");
                            let currency = info["currency"].as_str().unwrap_or("");
                            result.balance_text = Some(format!("{amount} {currency}"));
                        }
                    }
                }
                Ok(res) => result.error = Some(format!("balance returned {}", res.status())),
                Err(e) => result.error = Some(e.to_string()),
            }
        }
        _ => {
            // No known balance endpoint: report credential health only,
            // reusing the model-catalog probe.
            match list_models(p).await {
                Ok(_) => result.ok = true,
                Err(e) => result.error = Some(e.to_string()),
            }
        }
    }
    result
}

/// Fetch a provider's live model catalog (also validates the API key).
pub async fn list_models(p: &crate::config::Provider) -> anyhow::Result<Vec<String>> {
    let url = format!("{}/models", p.base_url.trim_end_matches('/'));
    let client = http_client();
    // Protocol-correct auth shared with the proxy (Anthropic gets
    // x-api-key + anthropic-version; everything else a bearer token).
    let mut req = crate::proxy::apply_provider_auth(client.get(&url), p);
    if let Some(ua) = &p.user_agent {
        req = req.header("user-agent", ua);
    }
    let res = req
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("request failed: {e}"))?;
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
