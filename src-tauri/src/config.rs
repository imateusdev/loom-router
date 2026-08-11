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
    /// OpenAI Responses API (`/v1/responses`) - e.g. OpenCode Zen's
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
    /// Ordered named API keys. The first enabled key is the primary key
    /// when rotation is off.
    #[serde(default)]
    pub keys: Vec<ProviderKey>,
    /// Opt-in round-robin rotation across enabled keys.
    #[serde(default)]
    pub rotation_enabled: bool,
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

fn new_key_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderKey {
    /// Stable identity used by routing, stats and balance probes. Never
    /// changes on rename or reorder.
    pub id: String,
    /// User-visible name, unique within the provider.
    pub name: String,
    /// Disabled keys are skipped by routing and rotation.
    #[serde(default)]
    pub enabled: bool,
    /// Stored only locally. The backend returns this as empty to the webview.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Whether a key value exists locally. Filled by the backend for reads.
    #[serde(default)]
    pub has_key: bool,
}

impl Provider {
    /// Migrate a legacy single key into the ordered key registry. Returns
    /// true when the provider changed and the loaded config must be saved.
    pub fn migrate_provider_keys(&mut self) -> bool {
        let mut migrated = false;
        if self.api_key.is_some() {
            let legacy = self.api_key.take().unwrap_or_default();
            let existing = self
                .keys
                .iter_mut()
                .find(|key| key.api_key.as_deref() == Some(legacy.as_str()));
            match existing {
                Some(key) => {
                    key.api_key = Some(legacy);
                    key.has_key = true;
                }
                None if !legacy.is_empty() => {
                    self.keys.push(ProviderKey {
                        id: new_key_id(),
                        name: "Principal".to_string(),
                        enabled: true,
                        api_key: Some(legacy),
                        has_key: true,
                    });
                }
                None => {}
            }
            migrated = true;
        }
        let has_key = self
            .keys
            .iter()
            .any(|key| key.api_key.as_deref().is_some_and(|key| !key.is_empty()));
        if self.has_key != has_key {
            self.has_key = has_key;
            migrated = true;
        }
        migrated
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderModel {
    /// Upstream model id, e.g. "deepseek-v4-pro".
    pub id: String,
    /// Optional friendly label for the picker; defaults to `id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Real context window (tokens), filled from the provider's model
    /// catalog or the models.dev enrichment during discovery. None means
    /// nothing is known and `codex::context_window_for` falls back to the
    /// family heuristic / conservative default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    /// Wire dialect this one model is served in, when it differs from the
    /// provider's. One gateway can speak several: OpenCode serves Kimi/GLM
    /// as Chat Completions, Claude/Qwen as Anthropic Messages and GPT/Grok
    /// as Responses, all behind a single URL and key. `None` means "whatever
    /// the provider speaks", which is every ordinary endpoint and every
    /// model discovered before anyone said otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<ProviderProtocol>,
    /// Whether this model participates in Claude Code fast mode (the
    /// subscription `speed: "fast"` tier). Only Opus models on the
    /// claude-code provider carry `true`; the flag is meaningless elsewhere.
    #[serde(default)]
    pub fast_mode: bool,
    #[serde(default)]
    pub enabled: bool,
    /// Whether this model accepts image input for direct routing or visual assistance.
    #[serde(default)]
    pub supports_vision: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct VisualAssistanceConfig {
    /// Global opt-in for routing image-derived assistance requests.
    #[serde(default)]
    pub enabled: bool,
    /// Preferred `provider/model` slug for visual assistance.
    #[serde(default)]
    pub assistant_model: Option<String>,
    /// Ordered `provider/model` slugs to try after the primary model.
    #[serde(default)]
    pub fallback_models: Vec<String>,
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
    /// Global visual-assistance policy and its ordered model routing.
    #[serde(default)]
    pub visual_assistance: VisualAssistanceConfig,
    /// Republish external models under native slugs so Codex works without
    /// an OpenAI login (see codex.rs).
    #[serde(default)]
    pub native_slug_mode: bool,
    /// Model Codex starts new sessions with, in the canonical
    /// `provider/model` form (independent of `native_slug_mode`, which only
    /// decides the *published* slug). Materialized as the root `model` key
    /// of `~/.codex/config.toml`; `None` leaves that key to the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_model: Option<String>,
    /// The root `model` Codex had before LoomRouter first displaced it.
    /// Kept so that clearing the selection (or turning LoomRouter off)
    /// gives the user their own model back instead of silently dropping it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_model_backup: Option<String>,
    /// Whether the first-run walkthrough has been finished.
    ///
    /// Deliberately three-state. `None` means "never answered", which is
    /// only true for a genuinely fresh install: `load()` backfills it to
    /// `Some(true)` whenever a config file already exists, so upgrading
    /// users are never sent back through onboarding. A plain `bool` could
    /// not express that - `false` is also what gets persisted mid-walkthrough
    /// (activating the Codex integration saves the config), and that must
    /// not be mistaken for a legacy install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onboarding_completed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onboarding_step: Option<String>,
    /// Unix seconds marking the first visit to the validation step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation_started_at: Option<u64>,
    /// Newest request row id when validation started, so a request finished
    /// in the same second but before the wizard was opened is not accepted
    /// as the first post-boundary success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation_started_request_id: Option<i64>,
    /// Set by `load()` when it rewrote provider ids on the way in, so
    /// startup knows to persist the result and re-apply the Codex
    /// integration - Codex's own config still names the old provider, and a
    /// slug whose provider no longer exists routes nowhere. Never stored:
    /// it describes this load, not the config.
    #[serde(skip)]
    pub migrated: bool,
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
            visual_assistance: VisualAssistanceConfig::default(),
            native_slug_mode: false,
            active_model: None,
            codex_model_backup: None,
            onboarding_completed: None,
            onboarding_step: None,
            validation_started_at: None,
            validation_started_request_id: None,
            migrated: false,
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

pub fn is_gpt_model_id(model_id: &str) -> bool {
    model_id.trim().to_ascii_lowercase().starts_with("gpt-")
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
                cfg.merge_opencode_dialect_providers();
                cfg.migrate_provider_keys();
                cfg.repair_known_opencode_dialects();
                cfg.prune_external_gpt_models();
                cfg.normalize_claude_display_name();
                cfg
            }
            // No config file at all: a genuinely fresh install, so the
            // answer stays unset and the walkthrough runs.
            Err(_) => Self::default(),
        }
    }

    /// Older configs saved the claude-code preset with "(subscription)" in
    /// the display name. Normalize it on read so the UI never shows the
    /// stale label again.
    fn normalize_claude_display_name(&mut self) {
        if let Some(claude) = self
            .providers
            .get_mut(crate::providers::CLAUDE_CODE_PROVIDER_ID)
        {
            claude.name = "Claude Code".to_string();
        }
    }

    /// Fold the old per-dialect OpenCode providers into one per gateway.
    ///
    /// Each gateway used to be three providers - `opencode-go-chat`,
    /// `-claude`, `-responses` - because the dialect lived on the provider
    /// and one URL serves three. The dialect is a per-model field now, so
    /// the three collapse into `opencode-go`, each model keeping the dialect
    /// its old provider implied.
    ///
    /// Merging renames the provider, and models are addressed as
    /// `provider/model` everywhere - the picker, `active_model`, the side-call
    /// fallback, Codex's own config. So every reference is rewritten with the
    /// providers. The key, the enabled set and the learned context windows
    /// survive; a provider the user has since renamed or repointed keeps its
    /// own base URL by being merged only when it still matches the gateway.
    fn merge_opencode_dialect_providers(&mut self) {
        for (merged_id, base_url) in [
            ("opencode-zen", "https://opencode.ai/zen/v1"),
            ("opencode-go", "https://opencode.ai/zen/go/v1"),
        ] {
            let parts: Vec<String> = ["chat", "claude", "responses"]
                .iter()
                .map(|dialect| format!("{merged_id}-{dialect}"))
                .filter(|id| {
                    self.providers
                        .get(id)
                        .is_some_and(|p| p.base_url.trim_end_matches('/') == base_url)
                })
                .collect();
            if parts.is_empty() {
                continue;
            }
            let mut merged: Option<Provider> = self.providers.remove(merged_id);
            for part_id in &parts {
                let Some(part) = self.providers.remove(part_id) else {
                    continue;
                };
                // Every model of a single-dialect provider spoke that
                // provider's dialect, whether or not it said so.
                let models = part.models.iter().map(|m| ProviderModel {
                    protocol: m.protocol.clone().or_else(|| Some(part.protocol.clone())),
                    ..m.clone()
                });
                match merged.as_mut() {
                    // Same model id on two dialects of one gateway is the
                    // same upstream model reached two ways; first wins.
                    Some(target) => {
                        for model in models {
                            if !target.models.iter().any(|m| m.id == model.id) {
                                target.models.push(model);
                            }
                        }
                        if target.api_key.is_none() {
                            target.api_key = part.api_key.clone();
                            target.has_key = part.has_key;
                            if target.keys.is_empty() {
                                target.keys = part.keys.clone();
                            }
                        }
                        target.enabled |= part.enabled;
                    }
                    None => {
                        let mut target = part.clone();
                        target.id = merged_id.to_string();
                        target.name = match merged_id {
                            "opencode-zen" => "OpenCode Zen".to_string(),
                            _ => "OpenCode Go".to_string(),
                        };
                        // Chat Completions is what the rest of the catalog
                        // speaks and what an untagged model gets.
                        target.protocol = ProviderProtocol::OpenAI;
                        target.models = models.collect();
                        merged = Some(target);
                    }
                }
                self.rewrite_provider_slugs(part_id, merged_id);
            }
            if let Some(merged) = merged {
                self.providers.insert(merged_id.to_string(), merged);
                self.migrated = true;
            }
        }
    }

    /// Convert legacy single-key providers to the ordered key registry.
    /// The legacy field is migration input only and is cleared once handled.
    pub fn migrate_provider_keys(&mut self) {
        for provider in self.providers.values_mut() {
            if provider.migrate_provider_keys() {
                self.migrated = true;
            }
        }
    }

    /// Webview-safe copy of the config: every stored key value is replaced
    /// with an empty string and `has_key` describes whether a value exists.
    pub fn sanitize_for_frontend(&mut self) {
        for provider in self.providers.values_mut() {
            provider.has_key = provider
                .api_key
                .as_deref()
                .is_some_and(|key| !key.is_empty())
                || provider
                    .keys
                    .iter()
                    .any(|key| key.api_key.as_deref().is_some_and(|key| !key.is_empty()));
            for key in &mut provider.keys {
                key.api_key = Some(String::new());
            }
            provider.api_key = Some(String::new());
        }
    }

    /// Restore the endpoint split for models whose gateway wire is known and
    /// fixed. Older dialect probes could persist Anthropic for the flash tier,
    /// even though this gateway only serves it through Responses.
    fn repair_known_opencode_dialects(&mut self) {
        let mut repaired = false;
        for (provider_id, base_url) in [
            ("opencode-zen", "https://opencode.ai/zen/v1"),
            ("opencode-go", "https://opencode.ai/zen/go/v1"),
        ] {
            let Some(provider) = self.providers.get_mut(provider_id) else {
                continue;
            };
            if provider.base_url.trim_end_matches('/') != base_url {
                continue;
            }
            let Some(model) = provider
                .models
                .iter_mut()
                .find(|model| model.id == "deepseek-v4-flash")
            else {
                continue;
            };
            if model.protocol != Some(ProviderProtocol::Responses) {
                model.protocol = Some(ProviderProtocol::Responses);
                repaired = true;
            }
        }
        if repaired {
            self.migrated = true;
        }
    }

    /// GPT models are supplied by Codex's native catalog. Keeping mirrored
    /// upstream entries would let an external provider shadow that catalog.
    fn prune_external_gpt_models(&mut self) {
        let mut removed = false;
        for provider in self.providers.values_mut() {
            let before = provider.models.len();
            provider.models.retain(|model| !is_gpt_model_id(&model.id));
            removed |= provider.models.len() != before;
        }
        if !removed {
            return;
        }

        let is_removed_slug = |slug: &str| {
            slug.rsplit_once('/')
                .is_some_and(|(_, model)| is_gpt_model_id(model))
        };
        if self.active_model.as_deref().is_some_and(is_removed_slug) {
            self.active_model = None;
        }
        if self
            .side_call_fallback
            .as_deref()
            .is_some_and(is_removed_slug)
        {
            self.side_call_fallback = None;
        }
        if self
            .visual_assistance
            .assistant_model
            .as_deref()
            .is_some_and(is_removed_slug)
        {
            self.visual_assistance.assistant_model = None;
        }
        self.visual_assistance
            .fallback_models
            .retain(|slug| !is_removed_slug(slug));
        self.migrated = true;
    }

    /// Repoint every `provider/model` reference from one provider id to
    /// another. Codex's own config is rewritten separately, when the
    /// integration is next applied.
    fn rewrite_provider_slugs(&mut self, from: &str, to: &str) {
        for slug in [&mut self.active_model, &mut self.side_call_fallback]
            .into_iter()
            .flatten()
        {
            if let Some(model) = slug.strip_prefix(&format!("{from}/")) {
                *slug = format!("{to}/{model}");
            }
        }
        if let Some(slug) = self.visual_assistance.assistant_model.as_mut() {
            if let Some(model) = slug.strip_prefix(&format!("{from}/")) {
                *slug = format!("{to}/{model}");
            }
        }
        for slug in &mut self.visual_assistance.fallback_models {
            if let Some(model) = slug.strip_prefix(&format!("{from}/")) {
                *slug = format!("{to}/{model}");
            }
        }
    }

    /// Mark the first-run walkthrough as finished (or skipped).
    pub fn complete_onboarding(&mut self) {
        self.onboarding_completed = Some(true);
        self.onboarding_step = None;
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ut_079_legacy_onboarding_fields_default_without_replaying_completed_setup() {
        let config: AppConfig = serde_json::from_value(json!({
            "onboarding_completed": true
        }))
        .unwrap();

        assert_eq!(config.onboarding_completed, Some(true));
        assert_eq!(config.onboarding_step, None);
        assert_eq!(config.validation_started_at, None);
    }

    #[test]
    fn claude_display_name_is_normalized_on_load() {
        let mut config = AppConfig::default();
        config.providers.insert(
            crate::providers::CLAUDE_CODE_PROVIDER_ID.to_string(),
            Provider {
                id: crate::providers::CLAUDE_CODE_PROVIDER_ID.to_string(),
                name: "Claude Code (subscription)".to_string(),
                protocol: ProviderProtocol::Anthropic,
                base_url: "local".to_string(),
                api_key: None,
                keys: vec![],
                rotation_enabled: false,
                has_key: false,
                context_window: None,
                user_agent: None,
                models: vec![],
                enabled: true,
            },
        );

        config.normalize_claude_display_name();

        assert_eq!(
            config.providers[crate::providers::CLAUDE_CODE_PROVIDER_ID].name,
            "Claude Code"
        );
    }

    #[test]
    fn ut_075_complete_onboarding_clears_the_resume_step() {
        let mut config = AppConfig {
            onboarding_completed: Some(false),
            onboarding_step: Some("validate".into()),
            ..AppConfig::default()
        };

        config.complete_onboarding();

        assert_eq!(config.onboarding_completed, Some(true));
        assert_eq!(config.onboarding_step, None);
    }

    #[test]
    fn legacy_config_defaults_visual_assistance_and_model_vision_to_disabled() {
        // Removing either serde default would make existing user configs fail
        // to load or accidentally opt into image handling after an upgrade.
        let legacy = json!({
            "port": 4180,
            "providers": {
                "deepseek": {
                    "id": "deepseek",
                    "name": "DeepSeek",
                    "protocol": "openai",
                    "base_url": "https://api.deepseek.com/v1",
                    "models": [{ "id": "deepseek-chat", "enabled": true }]
                }
            }
        });

        let config: AppConfig = serde_json::from_value(legacy).unwrap();
        let saved = serde_json::to_value(config).unwrap();

        assert_eq!(saved["visual_assistance"]["enabled"], false);
        assert_eq!(
            saved["visual_assistance"]["assistant_model"],
            serde_json::Value::Null
        );
        assert_eq!(saved["visual_assistance"]["fallback_models"], json!([]));
        assert_eq!(
            saved["providers"]["deepseek"]["models"][0]["supports_vision"],
            false
        );
    }

    #[test]
    fn visual_assistance_round_trips_primary_and_ordered_fallbacks() {
        // A router depends on fallback order; serializing through an unordered
        // representation would change which visual model receives a request.
        let config: AppConfig = serde_json::from_value(json!({
            "visual_assistance": {
                "enabled": true,
                "assistant_model": "openrouter/google/gemini-2.5-pro",
                "fallback_models": [
                    "anthropic/claude-sonnet-4-5",
                    "openrouter/qwen/qwen3-vl-235b-a22b-instruct"
                ]
            }
        }))
        .unwrap();

        let saved = serde_json::to_value(config).unwrap();
        assert_eq!(saved["visual_assistance"]["enabled"], true);
        assert_eq!(
            saved["visual_assistance"]["assistant_model"],
            "openrouter/google/gemini-2.5-pro"
        );
        assert_eq!(
            saved["visual_assistance"]["fallback_models"],
            json!([
                "anthropic/claude-sonnet-4-5",
                "openrouter/qwen/qwen3-vl-235b-a22b-instruct"
            ])
        );
    }

    fn opencode_part(id: &str, protocol: ProviderProtocol, models: &[&str]) -> Provider {
        Provider {
            id: id.to_string(),
            name: id.to_string(),
            protocol,
            base_url: "https://opencode.ai/zen/go/v1".into(),
            api_key: Some("k".into()),
            keys: vec![],
            rotation_enabled: false,
            has_key: true,
            context_window: None,
            user_agent: None,
            models: models
                .iter()
                .map(|m| ProviderModel {
                    id: (*m).to_string(),
                    label: None,
                    context_window: Some(1_000_000),
                    protocol: None,
                    fast_mode: false,
                    enabled: true,
                    supports_vision: false,
                })
                .collect(),
            enabled: true,
        }
    }

    fn go_config() -> AppConfig {
        let mut cfg = AppConfig::default();
        for (id, protocol, models) in [
            (
                "opencode-go-chat",
                ProviderProtocol::OpenAI,
                &["kimi-k3"][..],
            ),
            (
                "opencode-go-claude",
                ProviderProtocol::Anthropic,
                &["qwen3.8-max"][..],
            ),
            (
                "opencode-go-responses",
                ProviderProtocol::Responses,
                &["gpt-5.6-luna"][..],
            ),
        ] {
            cfg.providers
                .insert(id.to_string(), opencode_part(id, protocol, models));
        }
        cfg
    }

    #[test]
    fn merging_opencode_keeps_each_model_in_its_own_dialect() {
        // The three per-dialect providers were the only record of which wire
        // each model is served on. Folding them into one has to move that
        // knowledge onto the models, or every non-Chat model 400s.
        let mut cfg = go_config();
        cfg.merge_opencode_dialect_providers();

        assert!(cfg.providers.contains_key("opencode-go"));
        assert_eq!(cfg.providers.len(), 1, "{:?}", cfg.providers.keys());
        let merged = &cfg.providers["opencode-go"];
        assert_eq!(merged.name, "OpenCode Go");
        assert_eq!(merged.protocol, ProviderProtocol::OpenAI);
        assert_eq!(merged.api_key.as_deref(), Some("k"));

        let dialect = |id: &str| {
            merged
                .models
                .iter()
                .find(|m| m.id == id)
                .unwrap_or_else(|| panic!("{id} missing from {:?}", merged.models))
                .protocol
                .clone()
        };
        assert_eq!(dialect("kimi-k3"), Some(ProviderProtocol::OpenAI));
        assert_eq!(dialect("qwen3.8-max"), Some(ProviderProtocol::Anthropic));
        assert_eq!(dialect("gpt-5.6-luna"), Some(ProviderProtocol::Responses));
        // Learned context windows are not re-learned for free: discovery
        // would have to run again on a provider that had already resolved.
        assert!(merged.models.iter().all(|m| m.context_window.is_some()));
    }

    #[test]
    fn repairs_the_known_opencode_flash_protocol() {
        let mut cfg = AppConfig::default();
        let mut provider = opencode_part(
            "opencode-go",
            ProviderProtocol::OpenAI,
            &["deepseek-v4-flash"],
        );
        provider.models[0].protocol = Some(ProviderProtocol::Anthropic);
        cfg.providers.insert(provider.id.clone(), provider);

        cfg.repair_known_opencode_dialects();

        assert_eq!(
            cfg.providers["opencode-go"].models[0].protocol,
            Some(ProviderProtocol::Responses)
        );
        assert!(cfg.migrated);
    }

    #[test]
    fn merging_opencode_repoints_the_saved_model_slugs() {
        // Models are addressed as `provider/model`, so a merge that renamed
        // the provider without rewriting these would leave the picker's
        // selection pointing at a provider that no longer exists.
        let mut cfg = go_config();
        cfg.active_model = Some("opencode-go-claude/qwen3.8-max".into());
        cfg.side_call_fallback = Some("opencode-go-chat/kimi-k3".into());
        cfg.visual_assistance = VisualAssistanceConfig {
            enabled: true,
            assistant_model: Some("opencode-go-responses/gpt-5.6-luna".into()),
            fallback_models: vec![
                "opencode-go-claude/qwen3.8-max".into(),
                "other-provider/unchanged".into(),
                "opencode-go-chat/kimi-k3".into(),
            ],
        };
        cfg.merge_opencode_dialect_providers();

        assert_eq!(cfg.active_model.as_deref(), Some("opencode-go/qwen3.8-max"));
        assert_eq!(
            cfg.side_call_fallback.as_deref(),
            Some("opencode-go/kimi-k3")
        );
        assert_eq!(
            cfg.visual_assistance.assistant_model.as_deref(),
            Some("opencode-go/gpt-5.6-luna")
        );
        assert_eq!(
            cfg.visual_assistance.fallback_models,
            vec![
                "opencode-go/qwen3.8-max",
                "other-provider/unchanged",
                "opencode-go/kimi-k3",
            ]
        );
        assert!(cfg.migrated, "startup must know to persist and re-apply");
    }

    #[test]
    fn merging_leaves_a_repointed_provider_alone() {
        // Same id, different endpoint: the user pointed it somewhere else.
        // Merging it into the gateway would send their key to a URL they did
        // not choose.
        let mut cfg = AppConfig::default();
        let mut moved = opencode_part("opencode-go-chat", ProviderProtocol::OpenAI, &["k"]);
        moved.base_url = "https://my-proxy.internal/v1".into();
        cfg.providers.insert(moved.id.clone(), moved);
        cfg.merge_opencode_dialect_providers();

        assert!(cfg.providers.contains_key("opencode-go-chat"));
        assert!(!cfg.providers.contains_key("opencode-go"));
        assert!(!cfg.migrated);
    }

    #[test]
    fn pruning_external_gpt_models_removes_them_and_their_saved_references() {
        let mut cfg = AppConfig::default();
        cfg.providers.insert(
            "external".into(),
            opencode_part(
                "external",
                ProviderProtocol::OpenAI,
                &["gpt-5.6", "kimi-k3"],
            ),
        );
        cfg.active_model = Some("external/gpt-5.6".into());
        cfg.side_call_fallback = Some("external/gpt-5.6".into());
        cfg.visual_assistance.assistant_model = Some("external/gpt-5.6".into());
        cfg.visual_assistance.fallback_models =
            vec!["external/gpt-5.6".into(), "external/kimi-k3".into()];

        cfg.prune_external_gpt_models();

        assert_eq!(cfg.providers["external"].models.len(), 1);
        assert_eq!(cfg.providers["external"].models[0].id, "kimi-k3");
        assert!(cfg.active_model.is_none());
        assert!(cfg.side_call_fallback.is_none());
        assert!(cfg.visual_assistance.assistant_model.is_none());
        assert_eq!(cfg.visual_assistance.fallback_models, ["external/kimi-k3"]);
        assert!(cfg.migrated);
    }

    #[test]
    fn a_config_without_opencode_is_untouched() {
        let mut cfg = AppConfig::default();
        cfg.providers.insert(
            "deepseek".into(),
            Provider {
                id: "deepseek".into(),
                name: "DeepSeek".into(),
                protocol: ProviderProtocol::OpenAI,
                base_url: "https://api.deepseek.com/v1".into(),
                api_key: None,
                keys: vec![],
                rotation_enabled: false,
                has_key: false,
                context_window: None,
                user_agent: None,
                models: Vec::new(),
                enabled: true,
            },
        );
        cfg.merge_opencode_dialect_providers();
        assert_eq!(cfg.providers.len(), 1);
        assert!(cfg.providers.contains_key("deepseek"));
        assert!(!cfg.migrated);
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

    fn legacy_provider(api_key: Option<&str>) -> Provider {
        Provider {
            id: "deepseek".into(),
            name: "DeepSeek".into(),
            protocol: ProviderProtocol::OpenAI,
            base_url: "https://api.deepseek.com/v1".into(),
            api_key: api_key.map(str::to_string),
            keys: vec![],
            rotation_enabled: false,
            has_key: api_key.is_some(),
            context_window: None,
            user_agent: None,
            models: vec![],
            enabled: true,
        }
    }

    #[test]
    fn ut_001_legacy_api_key_migrates_to_principal() {
        let mut provider = legacy_provider(Some("secret"));

        assert!(provider.migrate_provider_keys());
        assert!(provider.api_key.is_none());
        assert_eq!(provider.keys.len(), 1);
        assert_eq!(provider.keys[0].name, "Principal");
        assert_eq!(provider.keys[0].api_key.as_deref(), Some("secret"));
        assert!(provider.has_key);
    }

    #[test]
    fn ut_002_existing_key_list_stays_unchanged() {
        let existing = key("key-1", "Main", Some("saved"));
        let mut provider = legacy_provider(None);
        provider.keys = vec![existing.clone()];
        provider.has_key = true;

        assert!(!provider.migrate_provider_keys());
        assert_eq!(provider.keys, vec![existing]);
    }

    #[test]
    fn ut_003_provider_without_legacy_key_stays_empty() {
        let mut provider = legacy_provider(None);

        assert!(!provider.migrate_provider_keys());
        assert!(provider.keys.is_empty());
        assert!(!provider.has_key);
    }

    #[test]
    fn ut_004_legacy_key_merges_once_without_duplicating() {
        let existing = key("key-1", "Main", Some("secret"));
        let mut provider = legacy_provider(Some("secret"));
        provider.keys = vec![existing.clone()];

        assert!(provider.migrate_provider_keys());
        assert_eq!(provider.keys.len(), 1);
        assert_eq!(provider.keys[0].id, "key-1");
        assert_eq!(provider.keys[0].api_key.as_deref(), Some("secret"));
        assert!(provider.has_key);
    }

    #[test]
    fn ut_005_partial_config_migration_preserves_models() {
        let raw = json!({
            "port": 4180,
            "providers": {
                "deepseek": {
                    "id": "deepseek",
                    "name": "DeepSeek",
                    "protocol": "openai",
                    "base_url": "https://api.deepseek.com/v1",
                    "api_key": "secret",
                    "models": [{ "id": "deepseek-chat", "enabled": true }]
                }
            }
        });
        let mut config: AppConfig = serde_json::from_value(raw).unwrap();

        config.migrate_provider_keys();

        let provider = &config.providers["deepseek"];
        assert_eq!(provider.keys.len(), 1);
        assert_eq!(provider.keys[0].name, "Principal");
        assert_eq!(provider.models[0].id, "deepseek-chat");
        assert!(config.migrated);
    }

    #[test]
    fn ut_008_rotation_enabled_defaults_to_false() {
        let config: AppConfig = serde_json::from_value(json!({
            "providers": {
                "deepseek": {
                    "id": "deepseek",
                    "name": "DeepSeek",
                    "protocol": "openai",
                    "base_url": "https://api.deepseek.com/v1",
                    "models": []
                }
            }
        }))
        .unwrap();

        assert!(!config.providers["deepseek"].rotation_enabled);
    }

    #[test]
    fn ut_009_rename_and_reorder_keep_stable_key_ids() {
        let mut provider = legacy_provider(None);
        provider.keys = vec![key("a", "Alpha", Some("1")), key("b", "Beta", Some("2"))];

        provider.keys.reverse();
        provider.keys[0].name = "Gamma".to_string();

        assert_eq!(provider.keys[0].id, "b");
        assert_eq!(provider.keys[1].id, "a");
    }

    #[test]
    fn ut_090_sanitized_config_never_returns_key_values() {
        let mut config = AppConfig::default();
        let mut provider = legacy_provider(None);
        provider.keys = vec![
            key("a", "Alpha", Some("one")),
            key("b", "Beta", Some("two")),
        ];
        provider.has_key = true;
        config.providers.insert(provider.id.clone(), provider);

        config.sanitize_for_frontend();

        let sanitized = &config.providers["deepseek"];
        assert!(sanitized
            .keys
            .iter()
            .all(|key| key.api_key.as_deref() == Some("")));
        assert_eq!(sanitized.api_key.as_deref(), Some(""));
        assert!(sanitized.has_key);
    }

    #[test]
    fn ut_091_has_key_reflects_stored_key_values() {
        let mut provider = legacy_provider(None);
        provider.keys = vec![key("a", "Alpha", Some("one"))];

        assert!(provider.migrate_provider_keys() || provider.has_key);
        assert!(provider.has_key);

        let mut empty = legacy_provider(None);
        assert!(!empty.migrate_provider_keys());
        assert!(!empty.has_key);
    }

    #[test]
    fn it_013_legacy_config_json_migrates_and_keeps_the_provider_slug() {
        let raw = json!({
            "providers": {
                "deepseek": {
                    "id": "deepseek",
                    "name": "DeepSeek",
                    "protocol": "openai",
                    "base_url": "https://api.deepseek.com/v1",
                    "api_key": "secret",
                    "models": [{ "id": "deepseek-chat", "enabled": true }]
                }
            }
        });
        let mut config: AppConfig = serde_json::from_value(raw).unwrap();

        config.migrate_provider_keys();

        let provider = &config.providers["deepseek"];
        assert_eq!(provider.id, "deepseek");
        assert_eq!(provider.keys[0].name, "Principal");
        assert_eq!(provider.keys[0].api_key.as_deref(), Some("secret"));
        assert!(config.migrated);
    }
}
