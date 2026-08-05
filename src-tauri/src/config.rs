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
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|e| {
                tracing::warn!("invalid config at {}: {e}; starting fresh", path.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let dir = config_dir();
        #[cfg(unix)]
        {
            // Create ~/.loomrouter as 0700 (credentials live here) and
            // tighten it when it already exists with looser permissions.
            use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&dir)?;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        #[cfg(not(unix))]
        {
            // Windows: ACL hardening is best-effort only (see
            // `restrict_permissions`); the file stays under the user's
            // profile directory.
            std::fs::create_dir_all(&dir)?;
        }
        let path = config_path();
        let tmp = dir.join("config.json.tmp");
        let json = serde_json::to_string_pretty(self)?;
        #[cfg(unix)]
        {
            // Create the temp file with 0600 BEFORE any content is written,
            // so the API keys are never world-readable — not even during the
            // window between write and rename.
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(json.as_bytes())?;
        }
        #[cfg(not(unix))]
        {
            std::fs::write(&tmp, &json)?;
        }
        // The rename preserves the temp file's 0600 mode on Unix;
        // `restrict_permissions` is kept as a backstop.
        std::fs::rename(&tmp, &path)?;
        crate::config::restrict_permissions(&path);
        Ok(())
    }
}

/// Best-effort file permission hardening for credential-bearing files.
pub(crate) fn restrict_permissions(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(windows)]
    {
        // Tightening ACLs on Windows requires a DACL edit; deferred to a
        // later milestone. The file already lives under the user's profile.
        let _ = path;
    }
}
