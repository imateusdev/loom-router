//! Built-in provider presets (OpenAI-compatible unless noted).
//! Users can also add fully custom endpoints; presets are just convenience.

use crate::config::{Provider, ProviderProtocol};

pub struct Preset {
    pub id: &'static str,
    pub name: &'static str,
    pub protocol: ProviderProtocol,
    pub base_url: &'static str,
}

pub const PRESETS: &[Preset] = &[
    Preset {
        id: "deepseek",
        name: "DeepSeek",
        protocol: ProviderProtocol::OpenAI,
        base_url: "https://api.deepseek.com/v1",
    },
    Preset {
        id: "moonshot",
        name: "Moonshot AI (Kimi)",
        protocol: ProviderProtocol::OpenAI,
        base_url: "https://api.moonshot.ai/v1",
    },
    Preset {
        id: "openrouter",
        name: "OpenRouter",
        protocol: ProviderProtocol::OpenAI,
        base_url: "https://openrouter.ai/api/v1",
    },
    Preset {
        id: "groq",
        name: "Groq",
        protocol: ProviderProtocol::OpenAI,
        base_url: "https://api.groq.com/openai/v1",
    },
    Preset {
        id: "together",
        name: "Together AI",
        protocol: ProviderProtocol::OpenAI,
        base_url: "https://api.together.xyz/v1",
    },
    Preset {
        id: "mistral",
        name: "Mistral AI",
        protocol: ProviderProtocol::OpenAI,
        base_url: "https://api.mistral.ai/v1",
    },
    Preset {
        id: "siliconflow",
        name: "SiliconFlow",
        protocol: ProviderProtocol::OpenAI,
        base_url: "https://api.siliconflow.cn/v1",
    },
    Preset {
        id: "anthropic",
        name: "Anthropic",
        protocol: ProviderProtocol::Anthropic,
        base_url: "https://api.anthropic.com/v1",
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
            models: Vec::new(),
            enabled: true,
        }
    }
}
