use super::{
    family_of, model_protocol, sanitize_stateless_responses_payload, ProviderFamily, ProxyCtx,
    WireApi,
};
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
            Ok(("chat/completions", body, UpstreamKind::OpenAiChat))
        }
        (ProviderProtocol::OpenAI, WireApi::Responses) => {
            let mut body = payload.clone();
            translate::flatten_agent_messages(&mut body);
            Ok((
                "chat/completions",
                translate::responses_to_chat(&body, upstream_model, unified_reasoning)?,
                UpstreamKind::OpenAiChat,
            ))
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

pub(super) fn needs_responses_function_tool_compat(provider: &Provider, model: &str) -> bool {
    provider.id == "opencode-go" && model == "deepseek-v4-flash"
}
