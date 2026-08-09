use super::{sanitize_stateless_responses_payload, ProxyCtx, WireApi};
use crate::config::{Provider, ProviderProtocol};
use crate::translate::{self, UpstreamKind};
use serde_json::Value;

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
pub(super) async fn send(
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
    Ok(request.send().await?)
}

/// Build the exact upstream endpoint and request body for a routed model.
pub(super) fn build_upstream(
    provider: &Provider,
    payload: &Value,
    upstream_model: &str,
    wire: WireApi,
) -> anyhow::Result<(&'static str, Value, UpstreamKind)> {
    let unified_reasoning =
        crate::proxy::family_of(provider) == crate::proxy::ProviderFamily::OpenRouter;
    match (crate::proxy::model_protocol(provider, upstream_model), wire) {
        (ProviderProtocol::OpenAI, WireApi::ChatCompletions) => {
            let mut body = payload.clone();
            body["model"] = Value::String(upstream_model.to_string());
            Ok(("chat/completions", body, UpstreamKind::OpenAiChat))
        }
        (ProviderProtocol::OpenAI, WireApi::Responses) => Ok((
            "chat/completions",
            translate::responses_to_chat(payload, upstream_model, unified_reasoning)?,
            UpstreamKind::OpenAiChat,
        )),
        (ProviderProtocol::Anthropic, WireApi::ChatCompletions) => Ok((
            "messages",
            translate::chat_to_anthropic(payload, upstream_model)?,
            UpstreamKind::Anthropic,
        )),
        (ProviderProtocol::Anthropic, WireApi::Responses) => {
            let chat = translate::responses_to_chat(payload, upstream_model, unified_reasoning)?;
            Ok((
                "messages",
                translate::chat_to_anthropic(&chat, upstream_model)?,
                UpstreamKind::Anthropic,
            ))
        }
        (ProviderProtocol::Responses, WireApi::Responses) => {
            let mut body = if needs_responses_function_tool_compat(provider, upstream_model) {
                translate::responses_with_function_tools(payload)
            } else {
                payload.clone()
            };
            sanitize_stateless_responses_payload(&mut body);
            body["model"] = Value::String(upstream_model.to_string());
            Ok(("responses", body, UpstreamKind::Responses))
        }
        (ProviderProtocol::Responses, WireApi::ChatCompletions) => anyhow::bail!(
            "provider '{}' only speaks the Responses API; use a Responses-wire client",
            provider.id
        ),
    }
}

pub(super) fn needs_responses_function_tool_compat(provider: &Provider, model: &str) -> bool {
    provider.id == "opencode-go" && model == "deepseek-v4-flash"
}
