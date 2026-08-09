use crate::config::{AppConfig, Provider, ProviderProtocol};
use axum::http::HeaderMap;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderFamily {
    Anthropic,
    OpenRouter,
    Kimi,
    DeepSeek,
    OpenAi,
}

pub fn family_of(provider: &crate::providers::Provider) -> ProviderFamily {
    let url = provider.base_url.to_ascii_lowercase();
    if url.contains("anthropic") {
        ProviderFamily::Anthropic
    } else if url.contains("openrouter") {
        ProviderFamily::OpenRouter
    } else if url.contains("kimi") || url.contains("moonshot") {
        ProviderFamily::Kimi
    } else if url.contains("deepseek") {
        ProviderFamily::DeepSeek
    } else {
        ProviderFamily::OpenAi
    }
}

/// A provider protocol is a default: a discovered model can name a more
/// specific dialect when one gateway serves several APIs.
pub fn model_protocol<'a>(
    provider: &'a crate::providers::Provider,
    model_id: &str,
) -> &'a ProviderProtocol {
    provider
        .models
        .iter()
        .find(|model| model.id == model_id)
        .and_then(|model| model.protocol.as_ref())
        .unwrap_or(&provider.protocol)
}

/// Resolve `provider/model` (or a bare upstream id in native-slug mode) to
/// one enabled provider and the model understood by its upstream.
pub(super) fn resolve<'a>(
    config: &'a AppConfig,
    model: &str,
) -> anyhow::Result<(&'a Provider, String)> {
    let (provider_id, upstream) = match model.split_once('/') {
        Some((provider_id, upstream)) => (Some(provider_id.to_string()), upstream.to_string()),
        None => (None, model.to_string()),
    };

    if let Some(provider_id) = provider_id {
        // Saved threads may still use pre-merge per-dialect OpenCode ids;
        // resolve them to the merged provider while retaining the model dialect.
        let resolved = if config.providers.contains_key(&provider_id) {
            provider_id.as_str()
        } else {
            merged_opencode_provider(config, &provider_id).unwrap_or(&provider_id)
        };
        let provider = config
            .providers
            .get(resolved)
            .ok_or_else(|| anyhow::anyhow!("unknown provider '{provider_id}'"))?;
        if !provider.enabled {
            anyhow::bail!("provider '{provider_id}' is disabled");
        }
        return Ok((provider, upstream));
    }

    if !config.native_slug_mode {
        anyhow::bail!("bare model '{model}' is reserved for native passthrough");
    }

    for provider in config
        .providers
        .values()
        .filter(|provider| provider.enabled)
    {
        if provider
            .models
            .iter()
            .any(|candidate| candidate.enabled && candidate.id == model)
        {
            return Ok((provider, model.to_string()));
        }
    }
    anyhow::bail!("no enabled provider serves model '{model}'")
}

pub(super) fn merged_opencode_provider<'a>(config: &'a AppConfig, id: &'a str) -> Option<&'a str> {
    for suffix in ["-chat", "-claude", "-responses"] {
        if let Some(merged) = id.strip_suffix(suffix) {
            if config.providers.contains_key(merged) {
                return Some(merged);
            }
        }
    }
    None
}

/// A route plan has no I/O: dispatch can execute it directly or retry the
/// original route after a failed side-call fallback without recalculating the
/// request classification.
pub(super) enum RoutePlan {
    Native,
    Routed {
        provider: Provider,
        upstream_model: String,
        from_fallback: bool,
    },
}

pub(super) fn resolve_effective(
    config: &AppConfig,
    model: &str,
    payload: &Value,
    headers: Option<&HeaderMap>,
) -> RoutePlan {
    if is_side_call(payload, headers) {
        if let Some(slug) = config.side_call_fallback.as_deref() {
            match resolve(config, slug) {
                Ok((provider, upstream_model)) => {
                    return RoutePlan::Routed {
                        provider: provider.clone(),
                        upstream_model,
                        from_fallback: true,
                    };
                }
                Err(error) => {
                    tracing::warn!(slug, %error, "side_call_fallback does not resolve; using original destination");
                }
            }
        }
    }
    match resolve(config, model) {
        Ok((provider, upstream_model)) => RoutePlan::Routed {
            provider: provider.clone(),
            upstream_model,
            from_fallback: false,
        },
        Err(_) => RoutePlan::Native,
    }
}

fn parse_request_kind(raw: &str) -> Option<String> {
    serde_json::from_str::<Value>(raw)
        .ok()?
        .get("request_kind")?
        .as_str()
        .map(str::to_string)
}

pub(super) fn is_side_call(payload: &Value, headers: Option<&HeaderMap>) -> bool {
    if let Some(raw) = payload
        .get("client_metadata")
        .and_then(|metadata| metadata.get("x-codex-turn-metadata"))
        .and_then(Value::as_str)
    {
        if let Some(kind) = parse_request_kind(raw) {
            return kind != "turn";
        }
    }
    if let Some(raw) = headers
        .and_then(|headers| headers.get("x-codex-turn-metadata"))
        .and_then(|value| value.to_str().ok())
    {
        if let Some(kind) = parse_request_kind(raw) {
            return kind != "turn";
        }
    }
    false
}
