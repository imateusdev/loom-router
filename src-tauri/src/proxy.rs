//! Local proxy: receives requests from the coding agent and dispatches
//! them to the right provider based on the `model` field.
//!
//! Endpoints (all bound to 127.0.0.1):
//!   POST /v1/responses        — Codex Responses API (translated upstream)
//!   POST /v1/chat/completions — OpenAI-compatible clients
//!   GET  /health              — liveness for the UI

use crate::config::{Provider, ProviderProtocol};
use crate::state::SharedConfig;
use crate::translate;
use anyhow::{anyhow, bail};
use axum::{
    body::Body,
    extract::State as AxState,
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{get, post},
    Json, Router,
};
use serde_json::Value;

#[derive(Clone)]
struct ProxyCtx {
    config: SharedConfig,
    client: reqwest::Client,
}

pub fn router(config: SharedConfig) -> Router {
    let ctx = ProxyCtx {
        config,
        client: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(600))
            .build()
            .expect("reqwest client"),
    };
    Router::new()
        .route("/health", get(health))
        .route("/v1/responses", post(handle_responses))
        .route("/v1/chat/completions", post(handle_chat_completions))
        .with_state(ctx)
}

async fn health() -> &'static str {
    "ok"
}

/// Resolve `provider/model` (or a bare upstream id) to (provider, upstream model).
fn resolve(config: &crate::config::AppConfig, model: &str) -> anyhow::Result<(Provider, String)> {
    let (provider_id, upstream) = match model.split_once('/') {
        Some((p, m)) => (Some(p.to_string()), m.to_string()),
        None => (None, model.to_string()),
    };

    if let Some(pid) = provider_id {
        let p = config
            .providers
            .get(&pid)
            .ok_or_else(|| anyhow!("unknown provider '{pid}'"))?;
        if !p.enabled {
            bail!("provider '{pid}' is disabled");
        }
        return Ok((p.clone(), upstream));
    }

    // Bare id: search enabled providers for a matching enabled model.
    for p in config.providers.values().filter(|p| p.enabled) {
        if p.models.iter().any(|m| m.enabled && m.id == model) {
            return Ok((p.clone(), model.to_string()));
        }
    }
    bail!("no enabled provider serves model '{model}'")
}

async fn forward(
    ctx: &ProxyCtx,
    provider: &Provider,
    path: &str,
    body: Value,
) -> anyhow::Result<Response> {
    let url = format!("{}/{}", provider.base_url.trim_end_matches('/'), path);
    let key = provider
        .api_key
        .clone()
        .ok_or_else(|| anyhow!("provider '{}' has no API key", provider.id))?;

    let mut req = ctx.client.post(&url).json(&body);
    match provider.protocol {
        ProviderProtocol::OpenAI => {
            req = req.bearer_auth(key);
        }
        ProviderProtocol::Anthropic => {
            req = req
                .header("x-api-key", key)
                .header("anthropic-version", "2023-06-01");
        }
    }

    let upstream = req.send().await?;
    let status = StatusCode::from_u16(upstream.status().as_u16())
        .unwrap_or(StatusCode::BAD_GATEWAY);

    // Stream the upstream body straight through (SSE included).
    let stream = upstream.bytes_stream();
    let body = Body::from_stream(stream);
    Ok(Response::builder().status(status).body(body)?)
}

async fn handle_responses(
    AxState(ctx): AxState<ProxyCtx>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Result<Response, (StatusCode, String)> {
    dispatch(ctx, headers, payload, WireApi::Responses)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))
}

async fn handle_chat_completions(
    AxState(ctx): AxState<ProxyCtx>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Result<Response, (StatusCode, String)> {
    dispatch(ctx, headers, payload, WireApi::ChatCompletions)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))
}

enum WireApi {
    Responses,
    ChatCompletions,
}

async fn dispatch(
    ctx: ProxyCtx,
    _headers: HeaderMap,
    payload: Value,
    wire: WireApi,
) -> anyhow::Result<Response> {
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing 'model' field"))?
        .to_string();

    let cfg = ctx.config.read().await.clone();
    let (provider, upstream_model) = resolve(&cfg, &model)?;

    tracing::info!(%model, provider = %provider.id, %upstream_model, "routing request");

    match (&provider.protocol, wire) {
        (ProviderProtocol::OpenAI, WireApi::ChatCompletions) => {
            let mut body = payload;
            body["model"] = Value::String(upstream_model);
            forward(&ctx, &provider, "chat/completions", body).await
        }
        (ProviderProtocol::OpenAI, WireApi::Responses) => {
            let body = translate::responses_to_chat(&payload, &upstream_model)?;
            forward(&ctx, &provider, "chat/completions", body).await
            // TODO(milestone-2): translate the response back to Responses API
            // shape, including SSE event names.
        }
        (ProviderProtocol::Anthropic, WireApi::ChatCompletions) => {
            let body = translate::chat_to_anthropic(&payload, &upstream_model)?;
            forward(&ctx, &provider, "messages", body).await
        }
        (ProviderProtocol::Anthropic, WireApi::Responses) => {
            let chat = translate::responses_to_chat(&payload, &upstream_model)?;
            let body = translate::chat_to_anthropic(&chat, &upstream_model)?;
            forward(&ctx, &provider, "messages", body).await
        }
    }
}
