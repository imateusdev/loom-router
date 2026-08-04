//! Built-in provider presets (OpenAI-compatible unless noted).
//! Users can also add fully custom endpoints; presets are just convenience.
//!
//! Kimi mirrors claude-code-router's three options: the Coding Plan
//! subscription endpoint (no /models discovery — ships default models),
//! and the Global/China pay-as-you-go APIs.

use crate::config::{Provider, ProviderProtocol};

pub struct Preset {
    pub id: &'static str,
    pub name: &'static str,
    pub protocol: ProviderProtocol,
    pub base_url: &'static str,
    /// Models seeded on add, for endpoints without a usable /models route.
    pub default_models: &'static [&'static str],
}

pub const PRESETS: &[Preset] = &[
    Preset {
        id: "kimi-coding",
        name: "Kimi Code - Coding Plan",
        protocol: ProviderProtocol::OpenAI,
        base_url: "https://api.kimi.com/coding/v1",
        default_models: &["kimi-for-coding"],
    },
    Preset {
        id: "moonshot-global",
        name: "Kimi API (Global)",
        protocol: ProviderProtocol::OpenAI,
        base_url: "https://api.moonshot.ai/v1",
        default_models: &[],
    },
    Preset {
        id: "moonshot-cn",
        name: "Kimi API (China)",
        protocol: ProviderProtocol::OpenAI,
        base_url: "https://api.moonshot.cn/v1",
        default_models: &[],
    },
    Preset {
        id: "deepseek",
        name: "DeepSeek",
        protocol: ProviderProtocol::OpenAI,
        base_url: "https://api.deepseek.com/v1",
        default_models: &[],
    },
    Preset {
        id: "openrouter",
        name: "OpenRouter",
        protocol: ProviderProtocol::OpenAI,
        base_url: "https://openrouter.ai/api/v1",
        default_models: &[],
    },
    Preset {
        id: "groq",
        name: "Groq",
        protocol: ProviderProtocol::OpenAI,
        base_url: "https://api.groq.com/openai/v1",
        default_models: &[],
    },
    Preset {
        id: "together",
        name: "Together AI",
        protocol: ProviderProtocol::OpenAI,
        base_url: "https://api.together.xyz/v1",
        default_models: &[],
    },
    Preset {
        id: "mistral",
        name: "Mistral AI",
        protocol: ProviderProtocol::OpenAI,
        base_url: "https://api.mistral.ai/v1",
        default_models: &[],
    },
    Preset {
        id: "siliconflow",
        name: "SiliconFlow",
        protocol: ProviderProtocol::OpenAI,
        base_url: "https://api.siliconflow.cn/v1",
        default_models: &[],
    },
    Preset {
        id: "zai-coding",
        name: "Z.ai GLM Coding Plan",
        protocol: ProviderProtocol::OpenAI,
        base_url: "https://api.z.ai/api/coding/paas/v4",
        default_models: &[],
    },
    Preset {
        id: "anthropic",
        name: "Anthropic",
        protocol: ProviderProtocol::Anthropic,
        base_url: "https://api.anthropic.com/v1",
        default_models: &[],
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
