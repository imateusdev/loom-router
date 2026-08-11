use super::{
    family_of, model_protocol, sanitize_stateless_responses_payload, upstream_unreachable_error,
    ProviderFamily, ProxyCtx, WireApi,
};
use crate::config::{Provider, ProviderKey, ProviderProtocol};
use crate::keypool::FailureKind;
use crate::translate::{self, UpstreamKind};
use anyhow::bail;
use axum::http::StatusCode;
use serde_json::{json, Value};

/// Apply the provider's upstream authentication to an outgoing request.
/// The scheme follows the model's wire protocol, because a gateway may host
/// Anthropic and OpenAI routes at the same base URL.
pub fn apply_provider_auth(
    req: reqwest::RequestBuilder,
    provider: &crate::providers::Provider,
    model_id: Option<&str>,
) -> reqwest::RequestBuilder {
    let Some(key) = provider.api_key.as_deref() else {
        return req;
    };
    let protocol = model_id
        .map(|model_id| crate::proxy::model_protocol(provider, model_id))
        .unwrap_or(&provider.protocol);
    match protocol {
        ProviderProtocol::Anthropic => req
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01"),
        ProviderProtocol::OpenAI | ProviderProtocol::Responses => req.bearer_auth(key),
    }
}

/// Send a prepared JSON body upstream and return its raw response.
///
/// A non-2xx answer is returned, not turned into an error: the caller forwards
/// the upstream's own status and body. Collapsing every failure into a bail
/// made the callers' error branches dead code, dropped the routed HTTP path out
/// of the request log entirely, and handed the client a 502 for a 429 it was
/// supposed to back off from.
pub(super) async fn send(
    ctx: &ProxyCtx,
    provider: &Provider,
    path: &str,
    body: &Value,
) -> anyhow::Result<(reqwest::Response, Option<String>)> {
    // AppConfig::load migrates legacy keys before runtime. This fallback only
    // keeps test fixtures and hand-built configs with no key list working.
    let eligible = if provider.keys.is_empty() {
        provider
            .api_key
            .as_deref()
            .filter(|key| !key.is_empty())
            .map(|key| {
                vec![ProviderKey {
                    id: format!("legacy-{}", provider.id),
                    name: "Principal".to_string(),
                    enabled: true,
                    api_key: Some(key.to_string()),
                    has_key: true,
                }]
            })
            .unwrap_or_default()
    } else {
        ctx.key_pools
            .eligible_keys(provider, provider.rotation_enabled)
            .await
    };
    if eligible.is_empty() {
        // Keys that are configured and enabled but currently ineligible are
        // cooling down after recent failures. Reporting that as a missing key
        // sends the user hunting for a settings problem that is not there.
        if provider.keys.iter().any(|key| key.enabled) {
            bail!(
                "provider '{}' has no key available right now: every enabled key is cooling down after recent failures",
                provider.id
            );
        }
        bail!("provider '{}' has no enabled API key", provider.id);
    }
    let mut last_response: Option<(reqwest::Response, String)> = None;
    let mut last_error = None;
    for key in &eligible {
        let mut provider = provider.clone();
        provider.api_key = key.api_key.clone();
        provider.has_key = key.has_key;
        let result = send_with_key(ctx, &provider, path, body).await;
        match result {
            Ok(res) if res.status().is_success() => {
                ctx.key_pools.record_success(&provider.id, &key.id).await;
                return Ok((res, Some(key.id.clone())));
            }
            Ok(res) => {
                let status = res.status();
                let Some(failure) = classify_status(status) else {
                    // The request is at fault: hand the upstream's own answer
                    // back instead of replaying it against every remaining key,
                    // and leave the pool alone - no key is to blame.
                    return Ok((res, Some(key.id.clone())));
                };
                ctx.key_pools
                    .record_failure(&provider.id, &key.id, failure, retry_after_seconds(&res))
                    .await;
                // Status only: the body belongs to the response the caller may
                // still forward, and reading it here would consume it.
                tracing::warn!(
                    provider = %provider.id,
                    key_id = %key.id,
                    %status,
                    "upstream rejected the request; trying the next key"
                );
                last_response = Some((res, key.id.clone()));
            }
            Err(error) => {
                ctx.key_pools
                    .record_failure(&provider.id, &key.id, FailureKind::Transient, None)
                    .await;
                last_error = Some(error.to_string());
            }
        }
    }
    // Every key answered with a failure status: return the last one so the
    // caller forwards the upstream's real status and body.
    if let Some((res, key_id)) = last_response {
        return Ok((res, Some(key_id)));
    }
    bail!(
        "provider '{}' is unavailable after exhausting configured keys: {}",
        provider.id,
        last_error.unwrap_or_default()
    )
}

async fn send_with_key(
    ctx: &ProxyCtx,
    provider: &Provider,
    path: &str,
    body: &Value,
) -> anyhow::Result<reqwest::Response> {
    let url = format!("{}/{}", provider.base_url.trim_end_matches('/'), path);
    if provider.api_key.is_none() {
        anyhow::bail!("provider '{}' has no API key", provider.id);
    }

    let mut request = ctx.client.post(&url).json(body);
    if let Some(user_agent) = &provider.user_agent {
        request = request.header("user-agent", user_agent);
    }
    request = apply_provider_auth(request, provider, body.get("model").and_then(Value::as_str));
    request
        .send()
        .await
        .map_err(|e| upstream_unreachable_error(&url, &e, &format!("provider '{}'", provider.id)))
}

/// How an upstream status reflects on the key that sent the request.
///
/// `None` means the request itself is at fault: no other key would fare any
/// better, so blaming the key cools down a perfectly good credential. A few
/// malformed requests used to park the only key for 25 minutes and then
/// report the provider as "no enabled API key", which hides the real error
/// behind a credentials problem that does not exist.
pub(super) fn classify_status(status: StatusCode) -> Option<FailureKind> {
    match status.as_u16() {
        401 | 403 => Some(FailureKind::Auth),
        402 | 408 | 429 => Some(FailureKind::Transient),
        500..=599 => Some(FailureKind::Transient),
        400..=499 => None,
        _ => Some(FailureKind::Transient),
    }
}

fn retry_after_seconds(res: &reqwest::Response) -> Option<u64> {
    res.headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
}

/// Build the exact upstream endpoint and request body for a routed model.
pub(super) fn build_upstream(
    provider: &Provider,
    payload: &Value,
    upstream_model: &str,
    wire: WireApi,
) -> anyhow::Result<(&'static str, Value, UpstreamKind)> {
    // OpenRouter speaks the unified reasoning object; everyone else gets
    // OpenAI-style reasoning_effort (sending both = 400 conflict there).
    let unified_reasoning = family_of(provider) == ProviderFamily::OpenRouter;
    // Per model, not per provider: OpenCode serves Chat Completions,
    // Anthropic Messages and Responses behind one URL.
    match (model_protocol(provider, upstream_model), wire) {
        (ProviderProtocol::OpenAI, WireApi::ChatCompletions) => {
            let mut body = payload.clone();
            translate::flatten_agent_messages(&mut body);
            body["model"] = Value::String(upstream_model.to_string());
            set_minimax_reasoning_split(&mut body, upstream_model);
            Ok(("chat/completions", body, UpstreamKind::OpenAiChat))
        }
        (ProviderProtocol::OpenAI, WireApi::Responses) => {
            let mut body = payload.clone();
            translate::flatten_agent_messages(&mut body);
            let mut chat = translate::responses_to_chat(&body, upstream_model, unified_reasoning)?;
            set_minimax_reasoning_split(&mut chat, upstream_model);
            Ok(("chat/completions", chat, UpstreamKind::OpenAiChat))
        }
        (ProviderProtocol::Anthropic, WireApi::ChatCompletions) => {
            let mut body = payload.clone();
            translate::flatten_agent_messages(&mut body);
            Ok((
                "messages",
                translate::chat_to_anthropic(&body, upstream_model)?,
                UpstreamKind::Anthropic,
            ))
        }
        (ProviderProtocol::Anthropic, WireApi::Responses) => {
            let mut body = payload.clone();
            translate::flatten_agent_messages(&mut body);
            let chat = translate::responses_to_chat(&body, upstream_model, unified_reasoning)?;
            Ok((
                "messages",
                translate::chat_to_anthropic(&chat, upstream_model)?,
                UpstreamKind::Anthropic,
            ))
        }
        (ProviderProtocol::Responses, WireApi::Responses) => {
            // The live OpenCode Go probe for deepseek-v4-flash accepts
            // ordinary Responses functions but rejects Codex's freeform
            // `custom` tools. Keep the quirk on the one verified route so
            // grammar-aware native providers retain their custom-tool format.
            let mut body = if needs_responses_function_tool_compat(provider, upstream_model) {
                translate::responses_with_function_tools(payload)
            } else {
                payload.clone()
            };
            translate::flatten_agent_messages(&mut body);
            translate::compaction_items_for_routed(&mut body);
            sanitize_stateless_responses_payload(&mut body);
            body["model"] = Value::String(upstream_model.to_string());
            Ok(("responses", body, UpstreamKind::Responses))
        }
        (ProviderProtocol::Responses, WireApi::ChatCompletions) => {
            anyhow::bail!(
                "provider '{}' only speaks the Responses API; use a Responses-wire client",
                provider.id
            )
        }
    }
}

fn set_minimax_reasoning_split(body: &mut Value, model: &str) {
    // MiniMax's OpenAI-compatible chat embeds thinking as <think> blocks in
    // content unless reasoning_split moves it to reasoning fields.
    if model.to_ascii_lowercase().contains("minimax") {
        body["reasoning_split"] = json!(true);
    }
}

pub(super) fn needs_responses_function_tool_compat(provider: &Provider, model: &str) -> bool {
    provider.id == "opencode-go" && model == "deepseek-v4-flash"
}
