use super::{
    family_of, model_protocol, sanitize_stateless_responses_payload, upstream_unreachable_error,
    ProviderFamily, ProxyCtx, WireApi,
};
use crate::config::{PromptCacheMode, Provider, ProviderKey, ProviderProtocol};
use crate::keypool::FailureKind;
use crate::translate::{self, UpstreamKind};
use anyhow::bail;
use axum::http::StatusCode;
use serde_json::{json, Value};

/// Independent facts about an upstream attempt. A request can time out while
/// the OS also reports an exit/status, or fail with no HTTP response at all;
/// callers must be able to read those outcomes separately instead of deriving
/// them from one nested error branch.
#[derive(Debug, Clone, Default)]
pub(super) struct UpstreamOutcome {
    pub status: Option<StatusCode>,
    pub timed_out: bool,
    pub network_error: Option<String>,
}

/// One normalized routed attempt: the optional HTTP response, the key that
/// served it, the orthogonal facts, and an error string when no response was
/// produced. This keeps callers from re-deriving timeout/network/status facts
/// from one nested `Result`.
pub(super) struct UpstreamResponse {
    pub response: Option<reqwest::Response>,
    pub key_id: Option<String>,
    pub outcome: UpstreamOutcome,
    pub error: Option<String>,
}

#[derive(Debug)]
struct UpstreamRequestError {
    message: String,
    timed_out: bool,
}

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
    let result = send_outcome(ctx, provider, path, body).await?;
    let Some(response) = result.response else {
        bail!(result.error.unwrap_or_default());
    };
    Ok((response, result.key_id))
}

/// [`send`] plus the normalized orthogonal outcome of the winning attempt.
pub(super) async fn send_outcome(
    ctx: &ProxyCtx,
    provider: &Provider,
    path: &str,
    body: &Value,
) -> anyhow::Result<UpstreamResponse> {
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
        let error = if provider.keys.iter().any(|key| key.enabled) {
            format!(
                "provider '{}' has no key available right now: every enabled key is cooling down after recent failures",
                provider.id
            )
        } else {
            format!("provider '{}' has no enabled API key", provider.id)
        };
        return Ok(UpstreamResponse {
            response: None,
            key_id: None,
            outcome: UpstreamOutcome::default(),
            error: Some(error),
        });
    }
    let mut last_response: Option<(reqwest::Response, String)> = None;
    let mut last_error = None;
    let mut last_outcome = UpstreamOutcome::default();
    for key in &eligible {
        let mut provider = provider.clone();
        provider.api_key = key.api_key.clone();
        provider.has_key = key.has_key;
        let result = send_with_key(ctx, &provider, path, body).await;
        match result {
            Ok(res) if res.status().is_success() => {
                ctx.key_pools.record_success(&provider.id, &key.id).await;
                let outcome = UpstreamOutcome {
                    status: Some(res.status()),
                    ..UpstreamOutcome::default()
                };
                return Ok(UpstreamResponse {
                    response: Some(res),
                    key_id: Some(key.id.clone()),
                    outcome,
                    error: None,
                });
            }
            Ok(res) => {
                let status = res.status();
                let Some(failure) = classify_status(status) else {
                    // The request is at fault: hand the upstream's own answer
                    // back instead of replaying it against every remaining key,
                    // and leave the pool alone - no key is to blame.
                    let outcome = UpstreamOutcome {
                        status: Some(status),
                        ..UpstreamOutcome::default()
                    };
                    return Ok(UpstreamResponse {
                        response: Some(res),
                        key_id: Some(key.id.clone()),
                        outcome,
                        error: None,
                    });
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
                last_outcome = UpstreamOutcome {
                    status: Some(status),
                    ..UpstreamOutcome::default()
                };
            }
            Err(error) => {
                ctx.key_pools
                    .record_failure(&provider.id, &key.id, FailureKind::Transient, None)
                    .await;
                last_error = Some(error.message.clone());
                last_outcome = UpstreamOutcome {
                    status: None,
                    timed_out: error.timed_out,
                    network_error: Some(error.message),
                };
            }
        }
    }
    // Every key answered with a failure status: return the last one so the
    // caller forwards the upstream's real status and body.
    if let Some((res, key_id)) = last_response {
        return Ok(UpstreamResponse {
            response: Some(res),
            key_id: Some(key_id),
            outcome: last_outcome,
            error: None,
        });
    }
    Ok(UpstreamResponse {
        response: None,
        key_id: None,
        outcome: last_outcome,
        error: Some(format!(
            "provider '{}' is unavailable after exhausting configured keys: {}",
            provider.id,
            last_error.unwrap_or_default()
        )),
    })
}

async fn send_with_key(
    ctx: &ProxyCtx,
    provider: &Provider,
    path: &str,
    body: &Value,
) -> Result<reqwest::Response, UpstreamRequestError> {
    let url = format!("{}/{}", provider.base_url.trim_end_matches('/'), path);
    if provider.api_key.is_none() {
        return Err(UpstreamRequestError {
            message: format!("provider '{}' has no API key", provider.id),
            timed_out: false,
        });
    }

    let mut request = ctx.client.post(&url).json(body);
    if let Some(user_agent) = &provider.user_agent {
        request = request.header("user-agent", user_agent);
    }
    request = apply_provider_auth(request, provider, body.get("model").and_then(Value::as_str));
    request.send().await.map_err(|e| {
        let message = upstream_unreachable_error(&url, &e, &format!("provider '{}'", provider.id))
            .to_string();
        UpstreamRequestError {
            timed_out: e.is_timeout(),
            message,
        }
    })
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

/// Drop every `cache_control` the payload carries, at any depth.
///
/// It is a content-block property, not a request parameter, so a downstream
/// client's breakpoints live inside `messages[].content[]`, `system[]` and
/// `tools[]`. Removing only a top-level key would leave all of them in place
/// and hand a cache directive to an upstream that never documented one.
fn strip_cache_control(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("cache_control");
            for child in map.values_mut() {
                strip_cache_control(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(strip_cache_control),
        _ => {}
    }
}

fn set_cache_control(target: &mut Value, control: &Value) {
    if let Some(object) = target.as_object_mut() {
        object.insert("cache_control".into(), control.clone());
    }
}

/// Put the breakpoint on the final content block of the final message.
///
/// Both wires spell a text block the same way, and both take the marker on a
/// block rather than on the message. String content is valid on either and
/// cannot carry one, so it is promoted to the single-block form first.
/// Returns whether a marker was placed.
fn mark_last_message(body: &mut Value, control: &Value) -> bool {
    let Some(last) = body
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .and_then(|messages| messages.last_mut())
    else {
        return false;
    };
    match last.get("content").cloned() {
        Some(Value::String(text)) => {
            last["content"] = json!([{"type": "text", "text": text, "cache_control": control}]);
            true
        }
        Some(Value::Array(mut blocks)) => match blocks.last_mut() {
            Some(block) => {
                set_cache_control(block, control);
                last["content"] = Value::Array(blocks);
                true
            }
            None => false,
        },
        _ => false,
    }
}

/// Mark the end of the cacheable prefix of an Anthropic `messages` body.
///
/// Anthropic reads `cache_control` from content blocks — a tool entry, a
/// system block, a message block — never from the top level of the request.
/// A breakpoint caches everything up to and including itself, and the prefix
/// hierarchy is tools, then system, then messages, so the end of the last
/// message covers all three. That is also the marker that pays off for an
/// agent conversation, which only ever appends: this turn writes the cache
/// the next one reads. System and tools are the fallbacks for a payload that
/// carries no message to mark.
fn set_anthropic_cache_breakpoint(body: &mut Value, control: &Value) {
    if mark_last_message(body, control) {
        return;
    }
    // `chat_to_anthropic` joins the system parts into a plain string, and only
    // the block form can carry the marker.
    if let Some(text) = body.get("system").and_then(Value::as_str) {
        body["system"] = json!([{"type": "text", "text": text, "cache_control": control}]);
        return;
    }
    if let Some(last) = body
        .get_mut("system")
        .and_then(Value::as_array_mut)
        .and_then(|blocks| blocks.last_mut())
    {
        set_cache_control(last, control);
        return;
    }
    if let Some(last) = body
        .get_mut("tools")
        .and_then(Value::as_array_mut)
        .and_then(|tools| tools.last_mut())
    {
        set_cache_control(last, control);
    }
}

/// Mark the cacheable prefix of an OpenAI-compatible `messages` body.
///
/// OpenRouter follows Anthropic's convention on this wire, so the marker rides
/// a content part here too.
fn set_openai_cache_breakpoint(body: &mut Value, control: &Value) {
    mark_last_message(body, control);
}

fn apply_prompt_cache(provider: &Provider, protocol: &ProviderProtocol, body: &mut Value) {
    use crate::providers::PromptCacheSupport;

    // Always first, and unconditionally: an incompatible upstream must not see
    // a directive just because the client sent one, and where Loom does cache,
    // its own breakpoint should be the only one in the payload.
    strip_cache_control(body);

    let support = crate::providers::prompt_cache_support(provider);
    let accepts_explicit = support == PromptCacheSupport::Hybrid
        || (support == PromptCacheSupport::ExplicitTtl && protocol == &ProviderProtocol::Anthropic);
    if !accepts_explicit {
        return;
    }
    let mode = provider.prompt_cache.unwrap_or_else(|| {
        if provider.id == "anthropic" {
            PromptCacheMode::FiveMinutes
        } else {
            PromptCacheMode::Off
        }
    });
    let control = match mode {
        PromptCacheMode::Off => return,
        PromptCacheMode::FiveMinutes => json!({"type": "ephemeral"}),
        PromptCacheMode::OneHour => json!({"type": "ephemeral", "ttl": "1h"}),
    };
    match protocol {
        ProviderProtocol::Anthropic => set_anthropic_cache_breakpoint(body, &control),
        // A Responses body has `input`, not `messages`, so this is a no-op
        // there — better than inventing a field the upstream never defined.
        _ => set_openai_cache_breakpoint(body, &control),
    }
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
            apply_prompt_cache(provider, &ProviderProtocol::OpenAI, &mut body);
            Ok(("chat/completions", body, UpstreamKind::OpenAiChat))
        }
        (ProviderProtocol::OpenAI, WireApi::Responses) => {
            let mut body = payload.clone();
            translate::flatten_agent_messages(&mut body);
            let mut chat = translate::responses_to_chat(&body, upstream_model, unified_reasoning)?;
            set_minimax_reasoning_split(&mut chat, upstream_model);
            apply_prompt_cache(provider, &ProviderProtocol::OpenAI, &mut chat);
            Ok(("chat/completions", chat, UpstreamKind::OpenAiChat))
        }
        (ProviderProtocol::Anthropic, WireApi::ChatCompletions) => {
            let mut body = payload.clone();
            translate::flatten_agent_messages(&mut body);
            let mut body = translate::chat_to_anthropic(&body, upstream_model)?;
            apply_prompt_cache(provider, &ProviderProtocol::Anthropic, &mut body);
            Ok(("messages", body, UpstreamKind::Anthropic))
        }
        (ProviderProtocol::Anthropic, WireApi::Responses) => {
            let mut body = payload.clone();
            translate::flatten_agent_messages(&mut body);
            let chat = translate::responses_to_chat(&body, upstream_model, unified_reasoning)?;
            let mut body = translate::chat_to_anthropic(&chat, upstream_model)?;
            apply_prompt_cache(provider, &ProviderProtocol::Anthropic, &mut body);
            Ok(("messages", body, UpstreamKind::Anthropic))
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
            apply_prompt_cache(provider, &ProviderProtocol::Responses, &mut body);
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
