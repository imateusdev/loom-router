//! Shared application state: config, proxy server lifecycle, integrations.

mod lifecycle;
mod model_discovery;

use crate::codex;
use crate::config::AppConfig;
#[cfg(test)]
use crate::stats::RequestEntry;
use crate::stats::{SharedStats, Stats};
use serde::Serialize;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tokio::sync::{oneshot, RwLock};

pub use model_discovery::{list_models, list_models_detailed};

pub type SharedConfig = Arc<RwLock<AppConfig>>;

#[derive(Debug, Clone, Serialize)]
pub struct ServerStatus {
    pub running: bool,
    pub port: u16,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct SetupValidation {
    pub started_at: Option<u64>,
    pub first_ok_request_at: Option<u64>,
    pub failed_attempt: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SetupStatus {
    pub ready: bool,
    pub missing: Vec<String>,
    pub validation: SetupValidation,
    pub codex_active: bool,
}

const WIZARD_STEPS: [&str; 6] = [
    "welcome", "codex", "detect", "provider", "validate", "finish",
];

pub struct AppState {
    pub config: SharedConfig,
    pub stats: SharedStats,
    pub key_pools: crate::keypool::KeyPools,
    server: Arc<RwLock<Option<ServerHandle>>>,
    pub(crate) wake: crate::wake_lock::WakeController,
    /// Serializes the combined power toggle. Without it, two fast clicks on
    /// the tray item interleave start/stop with apply/remove and settle on a
    /// state neither click asked for.
    power: tokio::sync::Mutex<()>,
    /// Detection is read-only, but confirmed imports must re-read and dedupe
    /// as one operation so two quick clicks cannot overwrite credentials.
    tool_import: tokio::sync::Mutex<()>,
    /// Context windows learned during model discovery, per provider, for
    /// models not yet in the config (they only get a `ProviderModel` entry
    /// when the user enables one — see toggle_model).
    model_contexts:
        RwLock<std::collections::HashMap<String, std::collections::HashMap<String, u32>>>,

    /// Cached native Codex model slugs, populated once at startup and invalidated
    /// after every `codex::apply` so the next UI read picks up new CLI models.
    native_slugs_cache: RwLock<Option<Vec<String>>>,
    /// Cached models.dev catalog and when it was fetched. The file is
    /// several MB and changes rarely, so one fetch per session window is
    /// plenty.
    models_dev: RwLock<Option<(std::time::Instant, serde_json::Value)>>,
    /// Test-only destination for configuration writes. Command integration
    /// tests must prove persistence without touching the user's real config.
    #[cfg(test)]
    test_config_path: Option<std::path::PathBuf>,
    #[cfg(test)]
    test_opencode_path: Option<std::path::PathBuf>,
    #[cfg(test)]
    test_persist_count: std::sync::atomic::AtomicUsize,
}

struct ServerHandle {
    id: u64,
    shutdown: oneshot::Sender<()>,
}

static NEXT_SERVER_ID: AtomicU64 = AtomicU64::new(1);

impl AppState {
    pub fn load() -> Self {
        let config = AppConfig::load();
        let wake = crate::wake_lock::WakeController::new(config.sleep_prevention);
        Self {
            config: Arc::new(RwLock::new(config)),
            stats: Arc::new(RwLock::new(Stats::load())),
            key_pools: crate::keypool::KeyPools::new(),
            server: Arc::new(RwLock::new(None)),
            wake,
            power: tokio::sync::Mutex::new(()),
            tool_import: tokio::sync::Mutex::new(()),
            model_contexts: RwLock::new(std::collections::HashMap::new()),
            native_slugs_cache: RwLock::new(None),
            models_dev: RwLock::new(None),
            #[cfg(test)]
            test_config_path: None,
            #[cfg(test)]
            test_opencode_path: None,
            #[cfg(test)]
            test_persist_count: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    async fn persist(&self) -> anyhow::Result<()> {
        #[cfg(test)]
        if let Some(path) = &self.test_config_path {
            self.test_persist_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let config = self.config.read().await;
            let json = serde_json::to_string_pretty(&*config)?;
            crate::secure_fs::write_private(path, json.as_bytes())?;
            return Ok(());
        }
        self.config.read().await.save()
    }

    #[cfg(test)]
    pub(crate) fn for_test(config: AppConfig, config_path: std::path::PathBuf) -> Self {
        Self {
            config: Arc::new(RwLock::new(config)),
            stats: Arc::new(RwLock::new(Stats::load())),
            key_pools: crate::keypool::KeyPools::new(),
            server: Arc::new(RwLock::new(None)),
            wake: crate::wake_lock::WakeController::disabled(),
            power: tokio::sync::Mutex::new(()),
            tool_import: tokio::sync::Mutex::new(()),
            model_contexts: RwLock::new(std::collections::HashMap::new()),
            native_slugs_cache: RwLock::new(None),
            models_dev: RwLock::new(None),
            test_config_path: Some(config_path),
            test_opencode_path: None,
            test_persist_count: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_test_opencode_path(mut self, path: std::path::PathBuf) -> Self {
        self.test_opencode_path = Some(path);
        self
    }
}

fn codex_is_active(cfg: &AppConfig, status: &codex::CodexStatus) -> bool {
    cfg.codex_integration && status.integration_enabled && status.managed_block_present
}

fn derive_setup_status(
    cfg: &AppConfig,
    codex_active: bool,
    claude_logged_in: bool,
    validation: &SetupValidation,
) -> SetupStatus {
    let mut missing = Vec::new();
    if !codex_active {
        missing.push("codex_integration".to_string());
    }

    let credentialed: Vec<_> = cfg
        .providers
        .values()
        .filter(|provider| {
            provider.enabled
                && (provider.has_key
                    || provider
                        .api_key
                        .as_deref()
                        .is_some_and(|key| !key.is_empty())
                    || (provider.id == crate::providers::CLAUDE_CODE_PROVIDER_ID
                        && claude_logged_in))
        })
        .collect();
    let provider_ready = credentialed
        .iter()
        .any(|provider| provider.models.iter().any(|model| model.enabled));
    if credentialed.is_empty() {
        missing.push("provider".to_string());
    } else if !provider_ready {
        missing.push("enabled_model".to_string());
    }

    SetupStatus {
        ready: codex_active && provider_ready,
        missing,
        validation: validation.clone(),
        codex_active,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderBalance {
    pub provider_id: String,
    /// Key this row describes; None for claude-code and keyless providers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_name: Option<String>,
    /// Credentials work (endpoint reachable and authorized).
    pub ok: bool,
    pub bars: Vec<QuotaBar>,
    pub balance_text: Option<String>,
    pub error: Option<String>,
}

/// Kimi resetTime can omit seconds and the trailing Z; normalize it to a
/// browser-parseable ISO UTC string instead of forwarding a partial timestamp.
fn reset_at_iso(raw: &str) -> Option<String> {
    let parsed = chrono::DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%SZ")
                .or_else(|_| chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%MZ"))
                .or_else(|_| chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S"))
                .or_else(|_| chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M"))
                .map(|dt| dt.and_utc())
        })
        .ok()?;
    Some(parsed.format("%Y-%m-%dT%H:%M:%SZ").to_string())
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
    let reset_at = detail
        .get("resetTime")
        .and_then(serde_json::Value::as_str)
        .and_then(reset_at_iso);
    Some(QuotaBar {
        label: label.to_string(),
        percent: (remaining / limit * 100.0).clamp(0.0, 100.0),
        detail: format!("{} / {} left", remaining as u64, limit as u64),
        reset_at,
    })
}

fn zai_quota_bar(limit: &serde_json::Value) -> Option<QuotaBar> {
    let unit = limit["unit"].as_i64()?;
    let number = limit["number"].as_i64()?;
    let usage = limit["usage"].as_f64()?;
    let remaining = limit["remaining"].as_f64()?;
    if usage <= 0.0 {
        return None;
    }
    let label = match (unit, number) {
        (3, 5) => "5-hour window".to_string(),
        (6, 1) => "Weekly quota".to_string(),
        _ => format!("{number} {unit}-unit window"),
    };
    let reset_at = limit["nextResetTime"]
        .as_i64()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|time| time.format("%Y-%m-%dT%H:%M:%SZ").to_string());
    Some(QuotaBar {
        label,
        percent: (remaining / usage * 100.0).clamp(0.0, 100.0),
        detail: format!("{} / {} credits left", remaining as u64, usage as u64),
        reset_at,
    })
}

async fn fetch_balance(
    p: &crate::config::Provider,
    key: Option<&crate::config::ProviderKey>,
) -> ProviderBalance {
    use crate::proxy::ProviderFamily;
    let mut provider = p.clone();
    if let Some(key) = key {
        provider.api_key = key.api_key.clone();
        provider.has_key = key.has_key;
    }
    let base = p.base_url.trim_end_matches('/').to_string();
    let mut result = ProviderBalance {
        provider_id: p.id.clone(),
        key_id: key.map(|key| key.id.clone()),
        key_name: key.map(|key| key.name.clone()),
        ok: false,
        bars: Vec::new(),
        balance_text: None,
        error: None,
    };
    // claude-code has no remote quota/balance endpoint: report the local
    // CLI's subscription login instead (the credential health of the plan).
    if p.id == crate::providers::CLAUDE_CODE_PROVIDER_ID {
        let status = crate::claude_cli::auth_status().await;
        result.ok = status.logged_in;
        if status.logged_in {
            result.balance_text = status.plan.or(status.subscription_type).or(status.email);
        } else {
            result.error = status.error;
        }
        return result;
    }
    let client = http_client();
    let get = |url: String| {
        // Protocol-correct auth (Anthropic: x-api-key + anthropic-version;
        // others: Authorization bearer) shared with the proxy. A balance
        // probe belongs to no model, so the provider's own dialect applies.
        let mut req = crate::proxy::apply_provider_auth(client.get(&url), &provider, None)
            // Per-probe timeout tighter than the client default.
            .timeout(std::time::Duration::from_secs(10));
        if let Some(ua) = &provider.user_agent {
            req = req.header("user-agent", ua);
        }
        req
    };

    if p.id == "zai-coding" {
        // The quota endpoint is host-relative under /api/monitor, not under
        // the provider base path, and its percentage field is consumed, not
        // remaining.
        let origin = base
            .split_once("/api/")
            .map(|(origin, _)| origin)
            .unwrap_or(&base);
        match get(format!("{origin}/api/monitor/usage/quota/limit"))
            .send()
            .await
        {
            Ok(res) if res.status().is_success() => {
                result.ok = true;
                if let Ok(body) = res.json::<serde_json::Value>().await {
                    if let Some(limits) = body["data"]["limits"].as_array() {
                        result.bars = limits
                            .iter()
                            .filter(|limit| limit["type"].as_str() == Some("CREDIT_LIMIT"))
                            .filter_map(zai_quota_bar)
                            .collect();
                    }
                }
            }
            Ok(res) => result.error = Some(format!("quota returned {}", res.status())),
            Err(e) => result.error = Some(e.to_string()),
        }
        return result;
    }

    match crate::proxy::family_of(&provider) {
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
            match list_models(&provider).await {
                Ok(_) => result.ok = true,
                Err(e) => result.error = Some(e.to_string()),
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        Provider, ProviderKey, ProviderModel, ProviderProtocol, SleepPreventionMode,
    };

    #[tokio::test]
    async fn sleep_prevention_mode_is_persisted_and_reaches_the_wake_thread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        // Nothing is acquired while the mode is Never, so the first event the
        // backend reports is proof that `set_mode` crossed the channel.
        let (wake, events) = crate::wake_lock::recording_controller(SleepPreventionMode::Never);
        wake.set_proxy_running(true);
        let mut state = AppState::for_test(AppConfig::default(), path.clone());
        state.wake = wake;

        state
            .set_sleep_prevention(SleepPreventionMode::Always)
            .await
            .unwrap();

        assert_eq!(
            state.config.read().await.sleep_prevention,
            SleepPreventionMode::Always
        );
        let persisted: AppConfig =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(persisted.sleep_prevention, SleepPreventionMode::Always);
        assert!(events
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap());
    }

    #[tokio::test]
    async fn server_stop_keeps_wake_lock_until_graceful_drain_finishes() {
        let (wake, events) = crate::wake_lock::recording_controller(SleepPreventionMode::Always);
        wake.set_proxy_running(true);
        assert!(events
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap());

        let (shutdown, _) = tokio::sync::oneshot::channel::<()>();
        let mut state = state_with_config(AppConfig::default());
        state.wake = wake;
        state.server = Arc::new(RwLock::new(Some(ServerHandle { id: 1, shutdown })));

        state.server_stop().await.unwrap();

        assert!(events
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err());
    }

    fn state_with_config(config: AppConfig) -> AppState {
        AppState {
            config: Arc::new(RwLock::new(config)),
            stats: Arc::new(RwLock::new(Stats::load())),
            key_pools: crate::keypool::KeyPools::new(),
            server: Arc::new(RwLock::new(None)),
            wake: crate::wake_lock::WakeController::disabled(),
            power: tokio::sync::Mutex::new(()),
            tool_import: tokio::sync::Mutex::new(()),
            model_contexts: RwLock::new(std::collections::HashMap::new()),
            native_slugs_cache: RwLock::new(None),
            models_dev: RwLock::new(None),
            test_config_path: None,
            test_opencode_path: None,
            test_persist_count: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn provider(id: &str, enabled: bool, has_key: bool, model_enabled: bool) -> Provider {
        Provider {
            id: id.into(),
            name: id.into(),
            protocol: ProviderProtocol::OpenAI,
            base_url: "https://example.test/v1".into(),
            api_key: has_key.then(|| "secret".into()),
            keys: vec![],
            rotation_enabled: false,
            has_key,
            context_window: None,
            user_agent: None,
            prompt_cache: None,
            models: vec![ProviderModel {
                id: "model".into(),
                label: None,
                context_window: None,
                protocol: None,
                fast_mode: false,
                enabled: model_enabled,
                supports_vision: false,
            }],
            enabled,
        }
    }

    fn key(id: &str, name: &str, value: Option<&str>) -> ProviderKey {
        ProviderKey {
            id: id.to_string(),
            name: name.to_string(),
            enabled: true,
            api_key: value.map(str::to_string),
            has_key: value.is_some(),
        }
    }

    fn keyed_provider(keys: Vec<ProviderKey>) -> Provider {
        let has_key = keys.iter().any(|key| {
            key.api_key
                .as_deref()
                .is_some_and(|value| !value.is_empty())
        });
        Provider {
            id: "test".into(),
            name: "Test".into(),
            protocol: ProviderProtocol::OpenAI,
            base_url: "https://example.test/v1".into(),
            api_key: None,
            keys,
            rotation_enabled: false,
            has_key,
            context_window: None,
            user_agent: None,
            prompt_cache: None,
            models: vec![ProviderModel {
                id: "model".into(),
                label: None,
                context_window: None,
                protocol: None,
                fast_mode: false,
                enabled: true,
                supports_vision: false,
            }],
            enabled: true,
        }
    }

    fn configured(provider: Provider) -> AppConfig {
        let mut config = AppConfig {
            codex_integration: true,
            ..AppConfig::default()
        };
        config.providers.insert(provider.id.clone(), provider);
        config
    }

    fn request(ts: u64, status: &str) -> RequestEntry {
        RequestEntry {
            ts,
            provider: "provider".into(),
            model: "model".into(),
            key_id: None,
            transport: "http".into(),
            kind: "request".into(),
            status: status.into(),
            error: (status == "error").then(|| "failed".into()),
            latency_ms: None,
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
            visual_assistance: None,
            finish_reason: None,
            cost_usd: None,
        }
    }

    fn codex_status(managed_block_present: bool) -> codex::CodexStatus {
        codex::CodexStatus {
            codex_home: String::new(),
            config_exists: true,
            config_parseable: true,
            managed_block_present,
            managed_block_orphaned: false,
            native_catalog_present: managed_block_present,
            merged_catalog_present: managed_block_present,
            merged_model_count: usize::from(managed_block_present),
            codex_cli_available: true,
            codex_config_loads: true,
            codex_config_error: None,
            integration_enabled: true,
            session: codex::CodexSessionStatus {
                path: String::new(),
                present: false,
                usable: false,
                has_account_id: false,
                expired: false,
                expires_in_hours: None,
                age_hours: None,
            },
        }
    }

    #[test]
    fn ut_085_ready_with_codex_keyed_provider_and_enabled_model() {
        let status = derive_setup_status(
            &configured(provider("ready", true, true, true)),
            true,
            false,
            &SetupValidation::default(),
        );
        assert!(status.ready);
        assert!(status.missing.is_empty());
    }

    #[test]
    fn ut_086_inactive_codex_is_missing() {
        let status = derive_setup_status(
            &configured(provider("ready", true, true, true)),
            false,
            false,
            &SetupValidation::default(),
        );
        assert!(!status.ready);
        assert_eq!(status.missing, ["codex_integration"]);
    }

    #[test]
    fn ut_087_disabled_provider_or_model_needs_another_ready_provider() {
        let disabled = configured(provider("disabled", false, true, true));
        assert_eq!(
            derive_setup_status(&disabled, true, false, &SetupValidation::default()).missing,
            ["provider"]
        );
        let no_model = configured(provider("no-model", true, true, false));
        assert_eq!(
            derive_setup_status(&no_model, true, false, &SetupValidation::default()).missing,
            ["enabled_model"]
        );
    }

    #[test]
    fn ut_088_next_derivation_reflects_external_config_changes() {
        let mut config = configured(provider("provider", true, true, true));
        assert!(derive_setup_status(&config, true, false, &SetupValidation::default()).ready);
        config.providers.get_mut("provider").unwrap().enabled = false;
        assert!(!derive_setup_status(&config, true, false, &SetupValidation::default()).ready);
    }

    #[test]
    fn ut_089_zero_providers_is_missing_provider() {
        let config = AppConfig {
            codex_integration: true,
            ..AppConfig::default()
        };
        assert_eq!(
            derive_setup_status(&config, true, false, &SetupValidation::default()).missing,
            ["provider"]
        );
    }

    #[test]
    fn ut_090_key_with_disabled_models_is_missing_enabled_model() {
        let config = configured(provider("provider", true, true, false));
        assert_eq!(
            derive_setup_status(&config, true, false, &SetupValidation::default()).missing,
            ["enabled_model"]
        );
    }

    #[test]
    fn ut_091_enabled_model_without_key_is_missing_provider() {
        let config = configured(provider("provider", true, false, true));
        assert_eq!(
            derive_setup_status(&config, true, false, &SetupValidation::default()).missing,
            ["provider"]
        );
    }

    #[test]
    fn ut_092_one_ready_provider_is_enough() {
        let mut config = configured(provider("ready", true, true, true));
        let disabled = provider("disabled", false, false, false);
        config.providers.insert(disabled.id.clone(), disabled);
        assert!(derive_setup_status(&config, true, false, &SetupValidation::default()).ready);
    }

    #[test]
    fn ut_093_status_after_save_uses_settled_backend_state() {
        let mut config = configured(provider("provider", true, true, false));
        assert!(!derive_setup_status(&config, true, false, &SetupValidation::default()).ready);
        config.providers.get_mut("provider").unwrap().models[0].enabled = true;
        assert!(derive_setup_status(&config, true, false, &SetupValidation::default()).ready);
    }

    #[test]
    fn claude_cli_login_counts_as_the_local_provider_credential() {
        let config = configured(provider("claude-code", true, false, true));
        assert!(derive_setup_status(&config, true, true, &SetupValidation::default()).ready);
        assert_eq!(
            derive_setup_status(&config, true, false, &SetupValidation::default()).missing,
            ["provider"]
        );
    }

    #[tokio::test]
    async fn it_001_step_persistence_completion_and_boundary_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let state = AppState::for_test(AppConfig::default(), path.clone());

        state.set_onboarding_step("provider").await.unwrap();
        let saved: AppConfig =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.onboarding_step.as_deref(), Some("provider"));
        assert_eq!(saved.validation_started_at, None);

        state.set_onboarding_step("validate").await.unwrap();
        let first_boundary = state.config.read().await.validation_started_at.unwrap();
        state.set_onboarding_step("validate").await.unwrap();
        assert_eq!(
            state.config.read().await.validation_started_at,
            Some(first_boundary)
        );

        state.complete_onboarding().await.unwrap();
        let saved: AppConfig =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(saved.onboarding_completed, Some(true));
        assert_eq!(saved.onboarding_step, None);
    }

    #[tokio::test]
    async fn invalid_wizard_step_is_not_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let state = AppState::for_test(AppConfig::default(), path.clone());

        assert!(state.set_onboarding_step("unknown").await.is_err());
        assert!(!path.exists());
        assert_eq!(state.config.read().await.onboarding_step, None);
    }

    #[tokio::test]
    async fn native_model_context_override_is_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let state = AppState::for_test(AppConfig::default(), path.clone());

        state
            .set_native_model_context_override("gpt-5.6-sol", 1_000_000)
            .await
            .unwrap();

        let saved: AppConfig =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(
            saved.native_model_context_overrides.get("gpt-5.6-sol"),
            Some(&1_000_000)
        );
    }

    #[tokio::test]
    async fn invalid_native_model_context_override_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let state = AppState::for_test(AppConfig::default(), path.clone());

        assert!(state
            .set_native_model_context_override("provider/model", 1_000_000)
            .await
            .is_err());
        assert!(state
            .set_native_model_context_override(" gpt-5.6-sol ", 1_000_000)
            .await
            .is_err());
        assert!(state
            .set_native_model_context_override("gpt-5.6-sol", 2_000_001)
            .await
            .is_err());
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn native_model_context_override_can_return_to_catalog_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let mut config = AppConfig::default();
        config
            .native_model_context_overrides
            .insert("gpt-5.6-sol".into(), 1_000_000);
        let state = AppState::for_test(config, path.clone());

        state
            .clear_native_model_context_override("gpt-5.6-sol")
            .await
            .unwrap();

        let saved: AppConfig =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert!(!saved
            .native_model_context_overrides
            .contains_key("gpt-5.6-sol"));
    }

    #[test]
    fn it_002_managed_codex_status_controls_readiness() {
        let config = configured(provider("provider", true, true, true));
        let applied = codex_is_active(&config, &codex_status(true));
        assert!(derive_setup_status(&config, applied, false, &SetupValidation::default()).ready);
        let removed = codex_is_active(&config, &codex_status(false));
        assert!(!derive_setup_status(&config, removed, false, &SetupValidation::default()).ready);
    }

    #[test]
    fn it_006_readiness_recalculates_after_provider_changes() {
        let mut config = configured(provider("provider", true, true, true));
        config.providers.get_mut("provider").unwrap().enabled = false;
        assert_eq!(
            derive_setup_status(&config, true, false, &SetupValidation::default()).missing,
            ["provider"]
        );
        let provider = config.providers.get_mut("provider").unwrap();
        provider.enabled = true;
        provider.models[0].enabled = false;
        assert_eq!(
            derive_setup_status(&config, true, false, &SetupValidation::default()).missing,
            ["enabled_model"]
        );
        let provider = config.providers.get_mut("provider").unwrap();
        provider.models[0].enabled = true;
        provider.api_key = None;
        provider.has_key = false;
        assert_eq!(
            derive_setup_status(&config, true, false, &SetupValidation::default()).missing,
            ["provider"]
        );
        config.providers.get_mut("provider").unwrap().has_key = true;
        assert!(derive_setup_status(&config, true, false, &SetupValidation::default()).ready);
    }

    #[test]
    fn it_007_validation_uses_only_post_boundary_stats() {
        let mut config = configured(provider("provider", true, true, true));
        config.validation_started_at = Some(100);
        let stats = Stats::in_memory();
        stats.record_entry(request(99, "error"));
        stats.record_entry(request(101, "error"));
        stats.record_entry(request(103, "ok"));
        stats.record_entry(request(102, "ok"));

        let (first_ok_request_at, failed_attempt) = stats.validation_since(100, None);
        let status = derive_setup_status(
            &config,
            true,
            false,
            &SetupValidation {
                started_at: Some(100),
                first_ok_request_at,
                failed_attempt,
            },
        );
        assert_eq!(status.validation.first_ok_request_at, Some(102));
        assert!(status.validation.failed_attempt);
    }

    #[test]
    fn it_008_validation_boundary_excludes_same_second_pre_wizard_success() {
        let mut config = configured(provider("provider", true, true, true));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        config.validation_started_at = Some(now);
        config.validation_started_request_id = Some(1);
        let stats = Stats::in_memory();
        stats.record_entry(request(now, "ok"));
        stats.record_entry(request(now, "error"));

        let (first_ok_request_at, failed_attempt) = stats.validation_since(now, Some(1));
        assert_eq!(first_ok_request_at, None);
        assert!(failed_attempt);
    }

    #[test]
    fn it_009_validation_aggregate_outlives_recent_window() {
        let mut config = configured(provider("provider", true, true, true));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        config.validation_started_at = Some(now - 10);
        let stats = Stats::in_memory();
        stats.record_entry(request(now - 5, "ok"));
        for _ in 0..101 {
            stats.record_entry(request(now - 4, "error"));
        }

        let (first_ok_request_at, failed_attempt) = stats.validation_since(now - 10, None);
        assert_eq!(first_ok_request_at, Some(now - 5));
        assert!(failed_attempt);
        assert!(stats
            .recent(100)
            .iter()
            .all(|entry| entry.status == "error"));
    }

    #[tokio::test]
    async fn set_model_vision_rejects_unknown_provider_and_model() {
        // Creating an entry here would silently turn a typo into a persisted
        // model selection, unlike the explicit model toggle workflow.
        let state = state_with_config(AppConfig::default());
        let err = state
            .set_model_vision("unknown", "anything", true)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "unknown provider 'unknown'");

        let mut config = AppConfig::default();
        let provider = Provider::from_preset(
            crate::providers::PRESETS
                .iter()
                .find(|preset| preset.id == "deepseek")
                .unwrap(),
        );
        config.providers.insert(provider.id.clone(), provider);
        let state = state_with_config(config);
        let err = state
            .set_model_vision("deepseek", "unknown", true)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "unknown model 'unknown'");
    }

    fn opencode_fixture(dir: &tempfile::TempDir, key: &str) -> std::path::PathBuf {
        let path = dir.path().join("opencode.json");
        std::fs::write(
            &path,
            format!(
                r#"{{// JSONC fixture
                  "provider": {{"zen": {{"options": {{
                    "baseURL": "https://opencode.ai/zen/v1",
                    "apiKey": "{key}",
                  }}}}}},
                }}"#
            ),
        )
        .unwrap();
        path
    }

    #[tokio::test]
    async fn ut_024_detection_without_confirmation_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let opencode = opencode_fixture(&dir, "source-secret");
        let output = dir.path().join("loomrouter.json");
        let state = AppState::for_test(AppConfig::default(), output.clone())
            .with_test_opencode_path(opencode);

        let detection = state.detect_tools().await;
        assert!(detection.opencode.gateways[0].importable);
        assert!(!output.exists());
    }

    #[tokio::test]
    async fn ut_026_existing_provider_key_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let opencode = opencode_fixture(&dir, "new-secret");
        let mut config = AppConfig::default();
        let existing = {
            let mut provider = keyed_provider(vec![key("existing", "Principal", Some("secret"))]);
            provider.id = "opencode-zen".to_string();
            provider
        };
        config.providers.insert(existing.id.clone(), existing);
        let state = AppState::for_test(config, dir.path().join("loomrouter.json"))
            .with_test_opencode_path(opencode);

        state.import_opencode_gateway("opencode-zen").await.unwrap();
        assert_eq!(
            state.config.read().await.providers["opencode-zen"].keys[0]
                .api_key
                .as_deref(),
            Some("secret")
        );
    }

    #[tokio::test]
    async fn ut_027_double_click_settles_to_one_provider() {
        let dir = tempfile::tempdir().unwrap();
        let opencode = opencode_fixture(&dir, "source-secret");
        let state = Arc::new(
            AppState::for_test(AppConfig::default(), dir.path().join("loomrouter.json"))
                .with_test_opencode_path(opencode),
        );

        let (first, second) = tokio::join!(
            state.import_opencode_gateway("opencode-zen"),
            state.import_opencode_gateway("opencode-zen")
        );
        first.unwrap();
        second.unwrap();
        assert_eq!(state.config.read().await.providers.len(), 1);
        assert_eq!(
            state
                .test_persist_count
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    #[tokio::test]
    async fn it_003_and_it_004_import_is_secret_safe_and_feeds_readiness() {
        let dir = tempfile::tempdir().unwrap();
        let opencode = opencode_fixture(&dir, "source-secret");
        let state = AppState::for_test(AppConfig::default(), dir.path().join("loomrouter.json"))
            .with_test_opencode_path(opencode);

        let detection = state.detect_tools().await;
        assert!(!serde_json::to_string(&detection)
            .unwrap()
            .contains("source-secret"));
        state.import_opencode_gateway("opencode-zen").await.unwrap();

        let mut config = state.config.read().await.clone();
        config.codex_integration = true;
        config.validation_started_at = Some(10);
        let status = derive_setup_status(
            &config,
            true,
            false,
            &SetupValidation {
                started_at: Some(10),
                first_ok_request_at: Some(11),
                failed_attempt: false,
            },
        );
        assert!(status.ready);
        assert_eq!(status.validation.first_ok_request_at, Some(11));
    }

    #[tokio::test]
    async fn startup_catalog_repair_regenerates_an_enabled_integration() {
        // This catches a regression where startup notices the integration but
        // leaves its generated model catalog stale. A newly shipped native
        // model then cannot reach Codex's picker until the user clicks Apply.
        let config = AppConfig {
            codex_integration: true,
            port: 4242,
            ..AppConfig::default()
        };
        let state = state_with_config(config);
        let regenerated = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = regenerated.clone();

        let repaired = state
            .repair_codex_integration_with(move |config, port| {
                assert!(config.codex_integration);
                assert_eq!(port, 4242);
                observed.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            })
            .await
            .unwrap();

        assert!(repaired);
        assert!(regenerated.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn startup_catalog_repair_leaves_disabled_integration_untouched() {
        let state = state_with_config(AppConfig::default());
        let repaired = state
            .repair_codex_integration_with(|_, _| -> anyhow::Result<()> {
                panic!("disabled integrations must not rewrite Codex configuration")
            })
            .await
            .unwrap();

        assert!(!repaired);
    }

    #[tokio::test]
    async fn periodic_catalog_refresh_invalidates_picker_cache_when_models_change() {
        // This catches a regression where the background refresh writes a
        // newly released native model but the picker keeps serving an old
        // in-memory slug list until LoomRouter restarts.
        let config = AppConfig {
            codex_integration: true,
            port: 4242,
            ..AppConfig::default()
        };
        let state = state_with_config(config);
        *state.native_slugs_cache.write().await = Some(vec!["gpt-5.6-sol".into()]);

        let changed = state
            .refresh_codex_integration_if_changed_with(|config, port| {
                assert!(config.codex_integration);
                assert_eq!(port, 4242);
                Ok(true)
            })
            .await
            .unwrap();

        assert!(changed);
        assert!(state.native_slugs_cache.read().await.is_none());
    }
    #[tokio::test]
    async fn codex_native_models_uses_populated_cache_when_server_is_present() {
        // The cache is already populated, so this path must return before the
        // CLI probe branch in codex_native_models runs.
        let (shutdown, _) = tokio::sync::oneshot::channel::<()>();
        let state = AppState {
            config: Arc::new(RwLock::new(AppConfig::default())),
            stats: Arc::new(RwLock::new(Stats::load())),
            key_pools: crate::keypool::KeyPools::new(),
            server: Arc::new(RwLock::new(Some(ServerHandle { id: 1, shutdown }))),
            wake: crate::wake_lock::WakeController::disabled(),
            power: tokio::sync::Mutex::new(()),
            tool_import: tokio::sync::Mutex::new(()),
            model_contexts: RwLock::new(std::collections::HashMap::new()),
            native_slugs_cache: RwLock::new(Some(vec![
                "gpt-5.6-sol".into(),
                "claude-opus-5".into(),
            ])),
            models_dev: RwLock::new(None),
            test_config_path: None,
            test_opencode_path: None,
            test_persist_count: std::sync::atomic::AtomicUsize::new(0),
        };

        assert_eq!(
            state.codex_native_models().await,
            vec!["gpt-5.6-sol".to_string(), "claude-opus-5".to_string()]
        );
    }

    #[tokio::test]
    async fn it_001_add_key_persists_and_sanitizes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let state = AppState::for_test(AppConfig::default(), path.clone());

        state
            .save_provider(keyed_provider(vec![key("", "Primary", Some("sk-1"))]))
            .await
            .unwrap();

        let saved: AppConfig =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let provider = &saved.providers["test"];
        assert_eq!(provider.keys.len(), 1);
        assert!(!provider.keys[0].id.is_empty());
        assert_eq!(provider.keys[0].name, "Primary");
        assert_eq!(provider.keys[0].api_key.as_deref(), Some("sk-1"));

        let mut sanitized = saved;
        sanitized.sanitize_for_frontend();
        assert_eq!(
            sanitized.providers["test"].keys[0].api_key.as_deref(),
            Some("")
        );
        assert!(sanitized.providers["test"].has_key);
    }

    #[tokio::test]
    async fn ut_006_empty_key_value_preserves_the_stored_value() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::for_test(AppConfig::default(), dir.path().join("config.json"));
        state
            .save_provider(keyed_provider(vec![key("key-1", "Main", Some("secret"))]))
            .await
            .unwrap();

        state
            .save_provider(keyed_provider(vec![key("key-1", "Main", None)]))
            .await
            .unwrap();

        let provider = &state.config.read().await.providers["test"];
        assert_eq!(provider.keys[0].id, "key-1");
        assert_eq!(provider.keys[0].api_key.as_deref(), Some("secret"));
        assert!(provider.has_key);
    }

    #[tokio::test]
    async fn ut_007_save_rejects_blank_and_duplicate_key_names() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::for_test(AppConfig::default(), dir.path().join("config.json"));

        assert!(state
            .save_provider(keyed_provider(vec![key("a", "  ", Some("x"))]))
            .await
            .is_err());
        assert!(state
            .save_provider(keyed_provider(vec![
                key("a", "Same", Some("x")),
                key("b", "Same", Some("y")),
            ]))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn it_002_rename_keeps_the_key_id() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::for_test(AppConfig::default(), dir.path().join("config.json"));
        state
            .save_provider(keyed_provider(vec![key("key-1", "Old", Some("secret"))]))
            .await
            .unwrap();

        state
            .save_provider(keyed_provider(vec![key("key-1", "New", None)]))
            .await
            .unwrap();

        let provider = &state.config.read().await.providers["test"];
        assert_eq!(provider.keys[0].id, "key-1");
        assert_eq!(provider.keys[0].name, "New");
        assert_eq!(provider.keys[0].api_key.as_deref(), Some("secret"));
    }

    #[tokio::test]
    async fn it_003_delete_last_key_keeps_the_provider() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let state = AppState::for_test(AppConfig::default(), path.clone());
        state
            .save_provider(keyed_provider(vec![key("only", "Only", Some("secret"))]))
            .await
            .unwrap();

        state.save_provider(keyed_provider(vec![])).await.unwrap();

        let saved: AppConfig =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(saved.providers.contains_key("test"));
        assert!(saved.providers["test"].keys.is_empty());
        assert!(!saved.providers["test"].has_key);
    }

    #[tokio::test]
    async fn it_004_reorder_persists_the_new_primary_order() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::for_test(AppConfig::default(), dir.path().join("config.json"));
        state
            .save_provider(keyed_provider(vec![
                key("a", "A", Some("1")),
                key("b", "B", Some("2")),
            ]))
            .await
            .unwrap();

        state
            .save_provider(keyed_provider(vec![
                key("b", "B", None),
                key("a", "A", None),
            ]))
            .await
            .unwrap();

        let provider = &state.config.read().await.providers["test"];
        assert_eq!(provider.keys[0].id, "b");
        assert_eq!(provider.keys[1].id, "a");
        assert_eq!(provider.keys[0].api_key.as_deref(), Some("2"));
    }

    #[tokio::test]
    async fn it_014_two_keys_sanitize_without_losing_has_key() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::for_test(AppConfig::default(), dir.path().join("config.json"));
        state
            .save_provider(keyed_provider(vec![
                key("a", "A", Some("1")),
                key("b", "B", Some("2")),
            ]))
            .await
            .unwrap();

        let mut config = state.config.read().await.clone();
        config.sanitize_for_frontend();
        let provider = &config.providers["test"];
        assert!(provider
            .keys
            .iter()
            .all(|key| key.api_key.as_deref() == Some("")));
        assert!(provider.has_key);
    }

    #[tokio::test]
    async fn ut_094_duplicate_key_ids_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::for_test(AppConfig::default(), dir.path().join("config.json"));

        assert!(state
            .save_provider(keyed_provider(vec![
                key("same", "A", Some("1")),
                key("same", "B", Some("2")),
            ]))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn deleting_key_removes_stored_value() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::for_test(AppConfig::default(), dir.path().join("config.json"));
        state
            .save_provider(keyed_provider(vec![key("only", "Only", Some("secret"))]))
            .await
            .unwrap();

        state.save_provider(keyed_provider(vec![])).await.unwrap();

        let provider = &state.config.read().await.providers["test"];
        assert!(provider.keys.is_empty());
        assert!(!provider.has_key);
        assert!(!serde_json::to_string(&*state.config.read().await)
            .unwrap()
            .contains("secret"));
    }

    #[tokio::test]
    async fn set_provider_rotation_persists_the_setting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let state = AppState::for_test(AppConfig::default(), path.clone());
        state
            .save_provider(keyed_provider(vec![key("a", "A", Some("1"))]))
            .await
            .unwrap();

        state.set_provider_rotation("test", true).await.unwrap();

        let saved: AppConfig =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(saved.providers["test"].rotation_enabled);
    }

    #[derive(Clone)]
    struct BalanceProbeServer {
        totals: std::collections::HashMap<String, f64>,
        in_flight: Arc<std::sync::atomic::AtomicUsize>,
        max_in_flight: Arc<std::sync::atomic::AtomicUsize>,
        delay_ms: u64,
    }

    async fn balance_probe_handler(
        axum::extract::Extension(server): axum::extract::Extension<BalanceProbeServer>,
        headers: axum::http::HeaderMap,
    ) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
        use std::sync::atomic::Ordering;
        if server.delay_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(server.delay_ms));
        }
        let current = server.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        server.max_in_flight.fetch_max(current, Ordering::SeqCst);
        let auth = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split_whitespace().last())
            .unwrap_or_default();
        let total = server.totals.get(auth).copied().unwrap_or(10.0);
        server.in_flight.fetch_sub(1, Ordering::SeqCst);
        (
            axum::http::StatusCode::OK,
            axum::Json(serde_json::json!({
                "data": {"total_credits": total, "total_usage": 0.0}
            })),
        )
    }

    async fn zai_balance_probe_handler(
        axum::extract::Extension(server): axum::extract::Extension<BalanceProbeServer>,
        headers: axum::http::HeaderMap,
    ) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
        let auth = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split_whitespace().last())
            .unwrap_or_default();
        if !server.totals.contains_key(auth) {
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                axum::Json(serde_json::json!({})),
            );
        }
        (
            axum::http::StatusCode::OK,
            axum::Json(serde_json::json!({
                "code": 200,
                "success": true,
                "data": {
                    "level": "lite",
                    "limits": [
                        {
                            "type": "CREDIT_LIMIT",
                            "unit": 3,
                            "number": 5,
                            "usage": 2000,
                            "currentValue": 57,
                            "remaining": 1942,
                            "percentage": 2,
                            "nextResetTime": 1786588392963_i64
                        },
                        {
                            "type": "CREDIT_LIMIT",
                            "unit": 6,
                            "number": 1,
                            "usage": 10000,
                            "currentValue": 57,
                            "remaining": 9942,
                            "percentage": 1,
                            "nextResetTime": 1787175085997_i64
                        },
                        {
                            "type": "TOKENS_LIMIT",
                            "unit": 1,
                            "number": 1,
                            "usage": 10000,
                            "currentValue": 0,
                            "remaining": 10000,
                            "percentage": 0,
                            "nextResetTime": 1787175085997_i64
                        }
                    ]
                }
            })),
        )
    }

    async fn spawn_balance_server(server: BalanceProbeServer) -> String {
        use axum::routing::get;
        let app = axum::Router::new()
            .route("/openrouter/v1/credits", get(balance_probe_handler))
            .route(
                "/api/monitor/usage/quota/limit",
                get(zai_balance_probe_handler),
            )
            .layer(axum::Extension(server));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    fn balance_provider_with_id(id: &str, base_url: String, keys: Vec<ProviderKey>) -> AppState {
        let mut provider = keyed_provider(keys);
        provider.id = id.into();
        provider.base_url = base_url;
        let mut config = AppConfig::default();
        config.providers.insert(provider.id.clone(), provider);
        let dir = tempfile::tempdir().unwrap();
        let mut state = AppState::for_test(config, dir.path().join("config.json"));
        state.stats = Arc::new(RwLock::new(Stats::in_memory()));
        state
    }

    fn balance_provider(base_url: String, keys: Vec<ProviderKey>) -> AppState {
        balance_provider_with_id("test", base_url, keys)
    }

    #[tokio::test]
    async fn it_012_per_key_balances_return_rows_with_correct_key_ids() {
        let url = spawn_balance_server(BalanceProbeServer {
            totals: std::collections::HashMap::from([
                ("secret-a".to_string(), 100.0),
                ("secret-b".to_string(), 50.0),
            ]),
            in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            max_in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            delay_ms: 0,
        })
        .await;
        let state = balance_provider(
            format!("{url}/openrouter/v1"),
            vec![
                key("key-a", "Alpha", Some("secret-a")),
                key("key-b", "Beta", Some("secret-b")),
            ],
        );

        let balances = state.provider_balances().await;

        assert_eq!(balances.len(), 2);
        assert_eq!(balances[0].key_id.as_deref(), Some("key-a"));
        assert_eq!(balances[0].key_name.as_deref(), Some("Alpha"));
        assert_eq!(balances[0].balance_text.as_deref(), Some("$100.00"));
        assert_eq!(balances[1].key_id.as_deref(), Some("key-b"));
        assert_eq!(balances[1].balance_text.as_deref(), Some("$50.00"));
    }

    #[tokio::test]
    async fn it_013_zai_balance_returns_credit_windows() {
        let url = spawn_balance_server(BalanceProbeServer {
            totals: std::collections::HashMap::from([("zai-secret".to_string(), 0.0)]),
            in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            max_in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            delay_ms: 0,
        })
        .await;
        let state = balance_provider_with_id(
            "zai-coding",
            format!("{url}/api/coding/paas/v4"),
            vec![key("key-a", "Alpha", Some("zai-secret"))],
        );

        let balances = state.provider_balances().await;

        assert_eq!(balances.len(), 1);
        let balance = &balances[0];
        assert!(balance.ok);
        assert_eq!(balance.error, None);
        assert_eq!(balance.bars.len(), 2);
        assert_eq!(balance.bars[0].label, "5-hour window");
        assert_eq!(balance.bars[1].label, "Weekly quota");
        assert!((balance.bars[0].percent - 97.1).abs() < 0.01);
        assert!((balance.bars[1].percent - 99.42).abs() < 0.01);
        assert_eq!(balance.bars[0].detail, "1942 / 2000 credits left");
        assert_eq!(balance.bars[1].detail, "9942 / 10000 credits left");
        assert_eq!(
            balance.bars[0].reset_at.as_deref(),
            Some("2026-08-13T02:33:12Z")
        );
        assert_eq!(
            balance.bars[1].reset_at.as_deref(),
            Some("2026-08-19T21:31:25Z")
        );
    }

    #[test]
    fn quota_bar_normalizes_partial_reset_time() {
        let bar = quota_bar(
            "Weekly quota",
            &serde_json::json!({
                "limit": 100,
                "remaining": 67,
                "resetTime": "2026-08-13T02:33Z",
            }),
        )
        .unwrap();

        assert_eq!(bar.detail, "67 / 100 left");
        assert_eq!(bar.reset_at.as_deref(), Some("2026-08-13T02:33:00Z"));
    }

    #[tokio::test]
    async fn ut_076_provider_balances_returns_one_row_per_api_key() {
        let url = spawn_balance_server(BalanceProbeServer {
            totals: std::collections::HashMap::new(),
            in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            max_in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            delay_ms: 0,
        })
        .await;
        let state = balance_provider(
            format!("{url}/openrouter/v1"),
            vec![
                key("key-a", "Alpha", Some("secret-a")),
                key("key-b", "Beta", Some("secret-b")),
            ],
        );

        let balances = state.provider_balances().await;

        assert_eq!(balances.len(), 2);
        assert_eq!(balances[0].key_id.as_deref(), Some("key-a"));
        assert_eq!(balances[1].key_id.as_deref(), Some("key-b"));
    }

    #[tokio::test]
    async fn ut_096_disabled_keys_are_not_probed_for_balance() {
        let url = spawn_balance_server(BalanceProbeServer {
            totals: std::collections::HashMap::from([("secret-a".to_string(), 100.0)]),
            in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            max_in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            delay_ms: 0,
        })
        .await;
        let mut disabled = key("key-b", "Beta", Some("secret-b"));
        disabled.enabled = false;
        let state = balance_provider(
            format!("{url}/openrouter/v1"),
            vec![key("key-a", "Alpha", Some("secret-a")), disabled],
        );

        let balances = state.provider_balances().await;

        assert_eq!(balances.len(), 1);
        assert_eq!(balances[0].key_id.as_deref(), Some("key-a"));
    }

    #[tokio::test]
    async fn ut_077_missing_balance_endpoint_is_unavailable_not_zero() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route(
                    "/openrouter/v1/credits",
                    axum::routing::get(|| async {
                        (
                            axum::http::StatusCode::NOT_FOUND,
                            axum::Json(serde_json::json!({})),
                        )
                    }),
                ),
            )
            .await
            .unwrap();
        });
        let state = balance_provider(
            format!("http://{addr}/openrouter/v1"),
            vec![key("key-a", "Alpha", Some("secret-a"))],
        );

        let balances = state.provider_balances().await;

        assert_eq!(balances.len(), 1);
        assert!(balances[0].bars.is_empty());
        assert!(balances[0].balance_text.is_none());
        assert!(balances[0].error.is_some());
    }

    #[tokio::test]
    async fn ut_078_slow_balance_does_not_block_local_usage() {
        let url = spawn_balance_server(BalanceProbeServer {
            totals: std::collections::HashMap::new(),
            in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            max_in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            delay_ms: 50,
        })
        .await;
        let state = balance_provider(
            format!("{url}/openrouter/v1"),
            vec![key("key-a", "Alpha", Some("secret-a"))],
        );
        let usage = serde_json::json!({"input_tokens": 1, "output_tokens": 1});
        state
            .stats
            .read()
            .await
            .record_entry(crate::stats::RequestEntry::ok_with_key(
                "test", "model", "http", None, "key-a", &usage,
            ));
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let (balances, summary) = tokio::join!(state.provider_balances(), async {
            let stats = state.stats.read().await;
            stats.summarize(86_400)
        },);

        assert_eq!(balances.len(), 1);
        assert_eq!(summary.requests, 1);
        assert_eq!(summary.per_key[0].key_id, "key-a");
    }

    #[tokio::test]
    async fn ut_079_balance_probes_are_bounded() {
        use std::sync::atomic::Ordering;
        let in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let max_in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let url = spawn_balance_server(BalanceProbeServer {
            totals: std::collections::HashMap::new(),
            in_flight: in_flight.clone(),
            max_in_flight: max_in_flight.clone(),
            delay_ms: 30,
        })
        .await;
        let keys: Vec<_> = (0..6)
            .map(|index| {
                key(
                    &format!("key-{index}"),
                    &format!("Key {index}"),
                    Some("secret"),
                )
            })
            .collect();
        let state = balance_provider(format!("{url}/openrouter/v1"), keys);

        let balances = state.provider_balances().await;

        assert_eq!(balances.len(), 6);
        assert!(
            max_in_flight.load(Ordering::SeqCst) <= 4,
            "max concurrent probes was {}",
            max_in_flight.load(Ordering::SeqCst)
        );
    }
}
