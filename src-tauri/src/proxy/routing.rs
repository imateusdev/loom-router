use crate::config::{AppConfig, Provider, ProviderProtocol};
use anyhow::{anyhow, bail};
use axum::http::HeaderMap;
use serde_json::Value;

pub use crate::config::ProviderFamily;

pub fn family_of(provider: &crate::providers::Provider) -> ProviderFamily {
    crate::providers::family_for(provider)
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
        Some((p, m)) => (Some(p.to_string()), m.to_string()),
        None => (None, model.to_string()),
    };

    if let Some(pid) = provider_id {
        // The OpenCode gateways used to be three providers each - the
        // dialect lived on the provider. Threads saved before the merge
        // still address `opencode-go-chat/deepseek-v4-flash` and friends.
        // Without this alias the slug fails to resolve, the turn falls into
        // the native passthrough, and the ChatGPT backend rejects it with
        // 400 - Codex then loses the conversation. The dialect is a
        // per-model field on the merged provider, so the model resolves to
        // the same upstream either way.
        let resolved = if config.providers.contains_key(&pid) {
            pid.as_str()
        } else {
            merged_opencode_provider(config, &pid).unwrap_or(&pid)
        };
        let p = config
            .providers
            .get(resolved)
            .ok_or_else(|| anyhow!("unknown provider '{pid}'"))?;
        if !p.enabled {
            bail!("provider '{pid}' is disabled");
        }
        return Ok((p, upstream));
    }

    if !config.native_slug_mode {
        bail!("bare model '{model}' is reserved for native passthrough");
    }

    for p in config.providers.values().filter(|p| p.enabled) {
        if p.models.iter().any(|m| m.enabled && m.id == model) {
            return Ok((p, model.to_string()));
        }
    }
    bail!("no enabled provider serves model '{model}'")
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
// Routing needs the full Provider (protocol, base URL, key list, models), so
// boxing it would only add indirection without shrinking the real work.
#[allow(clippy::large_enum_variant)]
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

/// Rewrite a compaction request for an upstream that does not speak Codex's
/// private `compaction_trigger` item: drop the trigger and the tool surface,
/// and ask for the handoff summary in plain terms.
pub(super) fn codex_request_kind(payload: &Value) -> Option<String> {
    payload
        .get("client_metadata")
        .and_then(|m| m.get("x-codex-turn-metadata"))
        .and_then(Value::as_str)
        .and_then(parse_request_kind)
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
