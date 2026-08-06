//! LoomRouter configuration: providers, credentials, enabled models.
//!
//! Stored at `~/.loomrouter/config.json`. API keys never leave the machine.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum ProviderProtocol {
    /// OpenAI-compatible Chat Completions (`/v1/chat/completions`)
    #[default]
    OpenAI,
    /// Anthropic Messages API (`/v1/messages`)
    Anthropic,
    /// OpenAI Responses API (`/v1/responses`) — e.g. OpenCode Zen's
    /// GPT/Grok models, which are not served as chat completions.
    Responses,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    /// Stable slug, e.g. "deepseek", "openrouter".
    pub id: String,
    /// Human label shown in the UI and the agent's model picker.
    pub name: String,
    pub protocol: ProviderProtocol,
    /// Base URL up to (and including) `/v1` or equivalent.
    pub base_url: String,
    /// API key. Stored only locally, never logged and never sent to the
    /// webview (see the sanitized `get_config` command).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Whether an API key is stored for this provider. The backend fills
    /// this in whenever a config is handed to the frontend, so the UI can
    /// show "key saved" without ever seeing the key itself.
    #[serde(default)]
    pub has_key: bool,
    /// Optional context window override (tokens) used when publishing this
    /// provider's models to agent catalogs. When absent, a conservative
    /// default is used (see `codex::routed_model`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    /// Custom User-Agent for providers that gate by client identity
    /// (e.g. Kimi For Coding only allows whitelisted coding agents).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    /// Models the user enabled for the agent picker, in display order.
    #[serde(default)]
    pub models: Vec<ProviderModel>,
    /// Whether this provider is active at all.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderModel {
    /// Upstream model id, e.g. "deepseek-v4-pro".
    pub id: String,
    /// Optional friendly label for the picker; defaults to `id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Proxy listen port on 127.0.0.1.
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub providers: BTreeMap<String, Provider>,
    /// Whether the Codex integration is active. When true, any config
    /// change (provider saved/deleted, model toggled, server start)
    /// re-applies the integration automatically.
    #[serde(default)]
    pub codex_integration: bool,
    /// Optional "provider/model" slug used to route Codex side/auxiliary
    /// calls (thread titles, probes) to a cheap/free provider.
    #[serde(default)]
    pub side_call_fallback: Option<String>,
    /// Republish external models under native slugs so Codex works without
    /// an OpenAI login (see codex.rs).
    #[serde(default)]
    pub native_slug_mode: bool,
    /// Whether the first-run walkthrough has been finished.
    ///
    /// Deliberately three-state. `None` means "never answered", which is
    /// only true for a genuinely fresh install: `load()` backfills it to
    /// `Some(true)` whenever a config file already exists, so upgrading
    /// users are never sent back through onboarding. A plain `bool` could
    /// not express that — `false` is also what gets persisted mid-walkthrough
    /// (activating the Codex integration saves the config), and that must
    /// not be mistaken for a legacy install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onboarding_completed: Option<bool>,
}

fn default_port() -> u16 {
    4180
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            port: default_port(),
            providers: BTreeMap::new(),
            codex_integration: false,
            side_call_fallback: None,
            native_slug_mode: false,
            onboarding_completed: None,
        }
    }
}

pub fn config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".loomrouter")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

impl AppConfig {
    pub fn load() -> Self {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(raw) => {
                let mut cfg: Self = serde_json::from_str(&raw).unwrap_or_else(|e| {
                    tracing::warn!("invalid config at {}: {e}; starting fresh", path.display());
                    Self::default()
                });
                // A config written before the walkthrough existed has no
                // answer recorded. Its owner is plainly not a first-run
                // user, so mark it done rather than interrupting them.
                if cfg.onboarding_completed.is_none() {
                    cfg.onboarding_completed = Some(true);
                }
                cfg
            }
            // No config file at all: a genuinely fresh install, so the
            // answer stays unset and the walkthrough runs.
            Err(_) => Self::default(),
        }
    }

    /// Mark the first-run walkthrough as finished (or skipped).
    pub fn complete_onboarding(&mut self) {
        self.onboarding_completed = Some(true);
    }

    /// Persist the config. API keys live in this file, so the write goes
    /// through `secure_fs`: owner-only directory, owner-only file, created
    /// with its mode before any content is written, replaced atomically.
    pub fn save(&self) -> anyhow::Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        crate::secure_fs::write_private(&config_path(), json.as_bytes())?;
        Ok(())
    }
}
