//! Built-in provider presets (OpenAI-compatible unless noted).
//! Users can also add fully custom endpoints; presets are just convenience.
//!
//! Kimi mirrors claude-code-router's three options: the Coding Plan
//! subscription endpoint plus the Global/China pay-as-you-go APIs.
//! The Coding Plan endpoint gates by client User-Agent, so the preset
//! carries the whitelisted Kimi CLI identity.

use crate::config::{Provider, ProviderProtocol};

pub struct Preset {
    pub id: &'static str,
    pub name: &'static str,
    pub protocol: ProviderProtocol,
    pub base_url: &'static str,
    /// Models seeded on add (official IDs, or for endpoints where
    /// discovery is unreliable).
    pub default_models: &'static [&'static str],
    /// User-Agent override for providers with a client whitelist.
    pub user_agent: Option<&'static str>,
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

pub const PRESETS: &[Preset] = &[
    Preset {
        id: "kimi-coding",
        name: "Kimi Code - Coding Plan",
        protocol: ProviderProtocol::OpenAI,
        base_url: "https://api.kimi.com/coding/v1",
        // Official model IDs from the Kimi Code docs; tier-gated upstream.
        default_models: &["k3", "k3-256k", "kimi-for-coding", "kimi-for-coding-highspeed"],
        // Kimi For Coding rejects clients outside its coding-agent
        // whitelist (403 access_terminated_error).
        user_agent: Some("KimiCLI/0.77"),
    },
    preset!("moonshot-global", "Kimi API (Global)", ProviderProtocol::OpenAI, "https://api.moonshot.ai/v1"),
    preset!("moonshot-cn", "Kimi API (China)", ProviderProtocol::OpenAI, "https://api.moonshot.cn/v1"),
    preset!("deepseek", "DeepSeek", ProviderProtocol::OpenAI, "https://api.deepseek.com/v1"),
    preset!("openrouter", "OpenRouter", ProviderProtocol::OpenAI, "https://openrouter.ai/api/v1"),
    preset!("groq", "Groq", ProviderProtocol::OpenAI, "https://api.groq.com/openai/v1"),
    preset!("together", "Together AI", ProviderProtocol::OpenAI, "https://api.together.xyz/v1"),
    preset!("mistral", "Mistral AI", ProviderProtocol::OpenAI, "https://api.mistral.ai/v1"),
    preset!("siliconflow", "SiliconFlow", ProviderProtocol::OpenAI, "https://api.siliconflow.cn/v1"),
    preset!("zai-coding", "Z.ai GLM Coding Plan", ProviderProtocol::OpenAI, "https://api.z.ai/api/coding/paas/v4"),
    preset!("anthropic", "Anthropic", ProviderProtocol::Anthropic, "https://api.anthropic.com/v1"),
];

impl Provider {
    pub fn from_preset(preset: &Preset) -> Self {
        Self {
            id: preset.id.to_string(),
            name: preset.name.to_string(),
            protocol: preset.protocol.clone(),
            base_url: preset.base_url.to_string(),
            api_key: None,
            user_agent: preset.user_agent.map(str::to_string),
            models: preset
                .default_models
                .iter()
                .map(|id| crate::config::ProviderModel {
                    id: id.to_string(),
                    label: None,
                    enabled: true,
                })
                .collect(),
            enabled: true,
        }
    }
}
