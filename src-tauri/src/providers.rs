//! Built-in provider presets (OpenAI-compatible unless noted).
//! Users can also add fully custom endpoints; presets are just convenience.
//!
//! Kimi mirrors claude-code-router's three options: the Coding Plan
//! subscription endpoint plus the Global/China pay-as-you-go APIs.
//! The Coding Plan endpoint gates by client User-Agent, so the preset
//! carries the whitelisted Kimi CLI identity.

// `Provider` is re-exported so cross-module helpers (e.g. the proxy's
// `family_of` / `apply_provider_auth`) can refer to
// `crate::providers::Provider`.
pub use crate::config::Provider;
use crate::config::ProviderProtocol;

pub struct Preset {
    pub id: &'static str,
    pub name: &'static str,
    /// Dialect the endpoint speaks, and the default for any model that does
    /// not name its own - including everything discovery turns up.
    pub protocol: ProviderProtocol,
    pub base_url: &'static str,
    /// Models seeded on add (official IDs, or for endpoints where
    /// discovery is unreliable).
    pub default_models: &'static [PresetModel],
    /// User-Agent override for providers with a client whitelist.
    pub user_agent: Option<&'static str>,
}

/// One seeded model. `protocol` is `Some` only where a gateway serves this
/// model in a different dialect than the rest of its catalog - OpenCode
/// speaks three behind one URL, so the split has to live per model rather
/// than per provider.
pub struct PresetModel {
    pub id: &'static str,
    pub protocol: Option<ProviderProtocol>,
}

/// A seeded model in the provider's own dialect.
const fn m(id: &'static str) -> PresetModel {
    PresetModel { id, protocol: None }
}

/// A seeded model the gateway serves in a dialect of its own.
const fn md(id: &'static str, protocol: ProviderProtocol) -> PresetModel {
    PresetModel {
        id,
        protocol: Some(protocol),
    }
}

macro_rules! preset {
    ($id:literal, $name:literal, $proto:expr, $url:literal) => {
        Preset {
            id: $id,
            name: $name,
            protocol: $proto,
            base_url: $url,
            default_models: &[],
            user_agent: None,
        }
    };
}

/// Provider id for the Claude Code (subscription) backend. This provider
/// carries no API key: the credential is the local `claude` CLI's own login
/// (`claude auth status`), and model requests are served by spawning the
/// real binary. See `claude_cli.rs`.
pub const CLAUDE_CODE_PROVIDER_ID: &str = "claude-code";

/// Models a Pro/Max subscription can use through the `claude` CLI, with the
/// context window Claude Code advertises for paid plans and whether the
/// model participates in fast mode (`/fast`). Fast mode exists only on the
/// Opus tier (Claude Code docs, mid-2026); everything else is `false`.
pub const CLAUDE_CODE_MODELS: &[(&str, u32, bool)] = &[
    ("claude-fable-5", 1_000_000, false),
    ("claude-opus-5", 1_000_000, true),
    ("claude-opus-4-8", 1_000_000, true),
    ("claude-sonnet-4-6", 1_000_000, false),
    ("claude-haiku-4-5", 200_000, false),
];

/// Whether a model id is part of the curated claude-code catalog.
pub fn is_claude_code_model(model_id: &str) -> bool {
    CLAUDE_CODE_MODELS.iter().any(|(id, _, _)| *id == model_id)
}

pub const PRESETS: &[Preset] = &[
    Preset {
        id: "claude-code",
        name: "Claude Code",
        protocol: ProviderProtocol::Anthropic,
        // No remote endpoint: requests are served by the local `claude` CLI
        // on behalf of the user's own subscription. Discovery (state.rs)
        // short-circuits this value and returns the curated catalog.
        base_url: "local",
        default_models: &[
            m("claude-fable-5"),
            m("claude-opus-5"),
            m("claude-opus-4-8"),
            m("claude-sonnet-4-6"),
            m("claude-haiku-4-5"),
        ],
        user_agent: None,
    },
    Preset {
        id: "kimi-coding",
        name: "Kimi Code - Coding Plan",
        protocol: ProviderProtocol::OpenAI,
        base_url: "https://api.kimi.com/coding/v1",
        // Official model IDs from the Kimi Code docs; tier-gated upstream.
        default_models: &[
            m("k3"),
            m("k3-256k"),
            m("kimi-for-coding"),
            m("kimi-for-coding-highspeed"),
        ],
        // Kimi For Coding rejects clients outside its coding-agent
        // whitelist (403 access_terminated_error).
        user_agent: Some("KimiCLI/0.77"),
    },
    preset!(
        "moonshot-global",
        "Kimi API (Global)",
        ProviderProtocol::OpenAI,
        "https://api.moonshot.ai/v1"
    ),
    preset!(
        "moonshot-cn",
        "Kimi API (China)",
        ProviderProtocol::OpenAI,
        "https://api.moonshot.cn/v1"
    ),
    preset!(
        "deepseek",
        "DeepSeek",
        ProviderProtocol::OpenAI,
        "https://api.deepseek.com/v1"
    ),
    preset!(
        "openrouter",
        "OpenRouter",
        ProviderProtocol::OpenAI,
        "https://openrouter.ai/api/v1"
    ),
    preset!(
        "groq",
        "Groq",
        ProviderProtocol::OpenAI,
        "https://api.groq.com/openai/v1"
    ),
    preset!(
        "together",
        "Together AI",
        ProviderProtocol::OpenAI,
        "https://api.together.xyz/v1"
    ),
    preset!(
        "mistral",
        "Mistral AI",
        ProviderProtocol::OpenAI,
        "https://api.mistral.ai/v1"
    ),
    preset!(
        "siliconflow",
        "SiliconFlow",
        ProviderProtocol::OpenAI,
        "https://api.siliconflow.cn/v1"
    ),
    preset!(
        "zai-coding",
        "Z.ai GLM Coding Plan",
        ProviderProtocol::OpenAI,
        "https://api.z.ai/api/coding/paas/v4"
    ),
    preset!(
        "anthropic",
        "Anthropic",
        ProviderProtocol::Anthropic,
        "https://api.anthropic.com/v1"
    ),
    // OpenCode Zen/Go: one gateway per subscription, three dialects behind
    // each. Same URL and key throughout, so they are one provider with the
    // dialect recorded per model. Chat Completions is the default: it is
    // what most of the catalog speaks, and what an unrecognised model
    // discovered later is assumed to speak until told otherwise.
    Preset {
        id: "opencode-zen",
        name: "OpenCode Zen",
        protocol: ProviderProtocol::OpenAI,
        base_url: "https://opencode.ai/zen/v1",
        default_models: &[
            md("kimi-k3", ProviderProtocol::OpenAI),
            md("kimi-k2.7-code", ProviderProtocol::OpenAI),
            md("glm-5.2", ProviderProtocol::OpenAI),
            md("deepseek-v4-pro", ProviderProtocol::OpenAI),
            // Console Go serves the flash tier only over the Responses API;
            // routed through chat/completions it returns 400 "Model is
            // unavailable" even though the id is in the catalog (verified
            // directly against the /go/ gateway). Keep the /go/ and /zen/
            // presets in agreement since they share the dialect split.
            md("deepseek-v4-flash", ProviderProtocol::Responses),
            md("minimax-m3", ProviderProtocol::OpenAI),
            md("claude-sonnet-5", ProviderProtocol::Anthropic),
            md("claude-opus-5", ProviderProtocol::Anthropic),
            md("claude-haiku-4-5", ProviderProtocol::Anthropic),
            md("qwen3.7-plus", ProviderProtocol::Anthropic),
            md("gpt-5.5", ProviderProtocol::Responses),
            md("gpt-5.4-mini", ProviderProtocol::Responses),
            md("gpt-5.4-nano", ProviderProtocol::Responses),
            md("grok-4.5", ProviderProtocol::Responses),
        ],
        user_agent: None,
    },
    // OpenCode Go (low-cost subscription) is the same gateway under the
    // /go/ path. A Go key only gets a 401 on the Zen endpoint, so the two
    // stay separate providers even though the dialect split is identical.
    Preset {
        id: "opencode-go",
        name: "OpenCode Go",
        protocol: ProviderProtocol::OpenAI,
        base_url: "https://opencode.ai/zen/go/v1",
        default_models: &[
            md("kimi-k3", ProviderProtocol::OpenAI),
            md("kimi-k2.7-code", ProviderProtocol::OpenAI),
            md("glm-5.2", ProviderProtocol::OpenAI),
            md("deepseek-v4-pro", ProviderProtocol::OpenAI),
            md("deepseek-v4-flash", ProviderProtocol::Responses),
            md("mimo-v2.5-pro", ProviderProtocol::OpenAI),
            md("hy3", ProviderProtocol::OpenAI),
            md("minimax-m3", ProviderProtocol::Anthropic),
            md("minimax-m2.7", ProviderProtocol::Anthropic),
            md("qwen3.8-max", ProviderProtocol::Anthropic),
            md("qwen3.7-max", ProviderProtocol::Anthropic),
            md("qwen3.7-plus", ProviderProtocol::Anthropic),
            md("qwen3.6-plus", ProviderProtocol::Anthropic),
            md("gpt-5.6-luna", ProviderProtocol::Responses),
        ],
        user_agent: None,
    },
];

impl Provider {
    pub fn from_preset(preset: &Preset) -> Self {
        Self {
            id: preset.id.to_string(),
            name: preset.name.to_string(),
            protocol: preset.protocol.clone(),
            base_url: preset.base_url.to_string(),
            api_key: None,
            keys: vec![],
            rotation_enabled: false,
            has_key: false,
            context_window: None,
            user_agent: preset.user_agent.map(str::to_string),
            models: preset
                .default_models
                .iter()
                .filter(|model| !crate::config::is_gpt_model_id(model.id))
                .map(|m| crate::config::ProviderModel {
                    id: m.id.to_string(),
                    label: claude_code_label(m.id),
                    // The claude-code catalog is curated (context windows and
                    // fast mode are known at preset time); every other preset
                    // learns them during discovery.
                    context_window: claude_code_context(m.id),
                    protocol: m.protocol.clone(),
                    fast_mode: claude_code_fast_mode(m.id),
                    enabled: true,
                    supports_vision: false,
                })
                .collect(),
            enabled: true,
        }
    }
}

/// Context window for a claude-code model id, if it is one.
pub fn claude_code_context(model_id: &str) -> Option<u32> {
    CLAUDE_CODE_MODELS
        .iter()
        .find(|(id, _, _)| *id == model_id)
        .map(|(_, ctx, _)| *ctx)
}

/// Whether a claude-code model participates in fast mode.
pub fn claude_code_fast_mode(model_id: &str) -> bool {
    CLAUDE_CODE_MODELS
        .iter()
        .any(|(id, _, fast)| *id == model_id && *fast)
}

/// Friendly picker label for a claude-code model id. The catalog ids are
/// slug-cased (`claude-opus-4-8`); the tray and the providers panel read
/// better when each dash-delimited token is capitalized ("Claude Opus 4 8").
/// `None` for ids outside the curated catalog, so callers can stamp labels
/// unconditionally.
pub fn claude_code_label(model_id: &str) -> Option<String> {
    if !is_claude_code_model(model_id) {
        return None;
    }
    let chars = model_id.chars();
    let mut pretty = String::with_capacity(model_id.len());
    let mut at_word_start = true;
    for c in chars {
        if c == '-' {
            pretty.push(' ');
            at_word_start = true;
        } else {
            pretty.push(if at_word_start {
                c.to_ascii_uppercase()
            } else {
                c
            });
            at_word_start = false;
        }
    }
    Some(pretty)
}
