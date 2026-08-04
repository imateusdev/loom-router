//! Local proxy: receives requests from the coding agent and dispatches
//! them to the right provider based on the `model` field, translating
//! both the request and the response (including SSE streams).
//!
//! Endpoints (all bound to 127.0.0.1):
//!   POST /v1/responses        — Codex Responses API
//!   POST /v1/chat/completions — OpenAI-compatible clients
//!   GET  /health              — liveness for the UI

use crate::config::{Provider, ProviderProtocol};
use crate::sse::{frame_data, frame_done, frame_with_event, SseParser};
use crate::state::SharedConfig;
use crate::translate::{self, DownstreamKind, StreamTranslator, UpstreamKind};
use anyhow::{anyhow, bail};
use axum::{
    body::Body,
    extract::State as AxState,
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{get, post},
    Json, Router,
};
use bytes::Bytes;
use futures::StreamExt;
use serde_json::Value;
use std::collections::VecDeque;

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
        .route("/v1/models", get(handle_models))
        .route("/v1/responses", post(handle_responses))
        .route("/v1/chat/completions", post(handle_chat_completions))
        .fallback(log_unmatched)
        .with_state(ctx)
}

async fn health() -> &'static str {
    "ok"
}

/// Codex occasionally calls paths we do not route (compaction, item
/// retrieval, probes). Log them so gaps are visible instead of silent.
async fn log_unmatched(method: axum::http::Method, uri: axum::http::Uri, body: Bytes) -> Response {
    let preview = String::from_utf8_lossy(&body);
    let preview: String = preview.chars().take(200).collect();
    tracing::warn!(%method, path = %uri.path(), body = %preview, "unmatched request");
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header("content-type", "application/json")
        .body(Body::from(format!(
            "{{\"error\":{{\"message\":\"loom-router: no route for {} {}\"}}}}",
            method,
            uri.path()
        )))
        .unwrap()
}

/// Parse a JSON body with logging; axum's default rejection hides the body.
fn parse_body(body: &Bytes, path: &str) -> Result<Value, (StatusCode, String)> {
    serde_json::from_slice::<Value>(body).map_err(|e| {
        let preview = String::from_utf8_lossy(body);
        let preview: String = preview.chars().take(300).collect();
        tracing::warn!(path, error = %e, len = body.len(), body = %preview, "bad JSON body");
        (
            StatusCode::BAD_REQUEST,
            format!("invalid JSON body for {path}: {e}"),
        )
    })
}

/// OpenAI-style model list of everything the proxy can serve.
async fn handle_models(AxState(ctx): AxState<ProxyCtx>) -> Json<Value> {
    let cfg = ctx.config.read().await;
    let data: Vec<Value> = cfg
        .providers
        .values()
        .filter(|p| p.enabled)
        .flat_map(|p| {
            p.models
                .iter()
                .filter(|m| m.enabled)
                .map(|m| {
                    serde_json::json!({
                        "id": format!("{}/{}", p.id, m.id),
                        "object": "model",
                        "owned_by": p.id,
                    })
                })
                .collect::<Vec<_>>()
        })
        .collect();
    Json(serde_json::json!({"object": "list", "data": data}))
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

    for p in config.providers.values().filter(|p| p.enabled) {
        if p.models.iter().any(|m| m.enabled && m.id == model) {
            return Ok((p.clone(), model.to_string()));
        }
    }
    bail!("no enabled provider serves model '{model}'")
}

/// Send a prepared JSON body upstream and return the raw response.
async fn send(
    ctx: &ProxyCtx,
    provider: &Provider,
    path: &str,
    body: &Value,
) -> anyhow::Result<reqwest::Response> {
    let url = format!("{}/{}", provider.base_url.trim_end_matches('/'), path);
    let key = provider
        .api_key
        .clone()
        .ok_or_else(|| anyhow!("provider '{}' has no API key", provider.id))?;

    let mut req = ctx.client.post(&url).json(body);
    if let Some(ua) = &provider.user_agent {
        req = req.header("user-agent", ua);
    }
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
    Ok(req.send().await?)
}

#[derive(Clone, Copy, PartialEq)]
enum WireApi {
    Responses,
    ChatCompletions,
}

async fn handle_responses(
    AxState(ctx): AxState<ProxyCtx>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, (StatusCode, String)> {
    let payload = parse_body(&body, "/v1/responses")?;
    dispatch(ctx, headers, payload, WireApi::Responses)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))
}

async fn handle_chat_completions(
    AxState(ctx): AxState<ProxyCtx>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, (StatusCode, String)> {
    let payload = parse_body(&body, "/v1/chat/completions")?;
    dispatch(ctx, headers, payload, WireApi::ChatCompletions)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))
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
    let wants_stream = payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let cfg = ctx.config.read().await.clone();
    let (provider, upstream_model) = match resolve(&cfg, &model) {
        Ok(r) => r,
        // Not an external model: native GPT models are forwarded unchanged
        // to OpenAI's backend with the caller's own ChatGPT credentials, so
        // the native models in the picker keep working through the proxy.
        Err(_) => return forward_native(&ctx, wire, &_headers, payload).await,
    };

    tracing::info!(%model, provider = %provider.id, %upstream_model, stream = wants_stream, "routing request");

    // Build the upstream request and remember the conversion path.
    let (path, body, upstream_kind) = match (&provider.protocol, wire) {
        (ProviderProtocol::OpenAI, WireApi::ChatCompletions) => {
            let mut body = payload.clone();
            body["model"] = Value::String(upstream_model.clone());
            ("chat/completions", body, UpstreamKind::OpenAiChat)
        }
        (ProviderProtocol::OpenAI, WireApi::Responses) => (
            "chat/completions",
            translate::responses_to_chat(&payload, &upstream_model)?,
            UpstreamKind::OpenAiChat,
        ),
        (ProviderProtocol::Anthropic, WireApi::ChatCompletions) => (
            "messages",
            translate::chat_to_anthropic(&payload, &upstream_model)?,
            UpstreamKind::Anthropic,
        ),
        (ProviderProtocol::Anthropic, WireApi::Responses) => {
            let chat = translate::responses_to_chat(&payload, &upstream_model)?;
            (
                "messages",
                translate::chat_to_anthropic(&chat, &upstream_model)?,
                UpstreamKind::Anthropic,
            )
        }
    };

    let upstream = send(&ctx, &provider, path, &body).await?;
    let status = StatusCode::from_u16(upstream.status().as_u16())
        .unwrap_or(StatusCode::BAD_GATEWAY);

    // Error or same-format pass-through: stream untouched.
    let same_format = matches!(
        (upstream_kind, wire),
        (UpstreamKind::OpenAiChat, WireApi::ChatCompletions)
    );
    if !status.is_success() || same_format {
        return Ok(Response::builder()
            .status(status)
            .body(Body::from_stream(upstream.bytes_stream()))?);
    }

    let downstream_kind = match wire {
        WireApi::Responses => DownstreamKind::Responses,
        WireApi::ChatCompletions => DownstreamKind::ChatCompletions,
    };

    if wants_stream {
        Ok(Response::builder()
            .status(status)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .body(Body::from_stream(translate_byte_stream(
                upstream,
                upstream_kind,
                downstream_kind,
                &model,
            )))?)
    } else {
        let json: Value = upstream.json().await?;
        let translated = match (upstream_kind, downstream_kind) {
            (UpstreamKind::OpenAiChat, DownstreamKind::Responses) => {
                translate::chat_completion_to_responses(&json, &model)
            }
            (UpstreamKind::Anthropic, DownstreamKind::Responses) => {
                translate::anthropic_to_responses(&json, &model)
            }
            (UpstreamKind::Anthropic, DownstreamKind::ChatCompletions) => {
                translate::anthropic_to_chat(&json, &model)
            }
            _ => json,
        };
        Ok(Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(Body::from(translated.to_string()))?)
    }
}

/// Forward a request untouched to OpenAI's native Codex backend.
///
/// The Codex app authenticates itself: its ChatGPT token and account headers
/// arrive on the incoming request, and we pass them straight through. Used
/// for native GPT models and any slug LoomRouter does not route.
async fn forward_native(
    ctx: &ProxyCtx,
    wire: WireApi,
    headers: &HeaderMap,
    payload: Value,
) -> anyhow::Result<Response> {
    const FORWARD: &[&str] = &[
        "authorization",
        "chatgpt-account-id",
        "openai-beta",
        "originator",
        "session_id",
        "user-agent",
        "accept",
        "version",
    ];
    let base = std::env::var("CODEX_NATIVE_BASE_URL")
        .unwrap_or_else(|_| "https://chatgpt.com/backend-api/codex".to_string());
    let path = match wire {
        WireApi::Responses => "/responses",
        WireApi::ChatCompletions => "/chat/completions",
    };
    let url = format!("{}{}", base.trim_end_matches('/'), path);

    let mut req = ctx.client.post(&url).json(&payload);
    for name in FORWARD {
        if let Some(value) = headers.get(*name) {
            if let Ok(v) = value.to_str() {
                req = req.header(*name, v);
            }
        }
    }
    let upstream = req.send().await?;
    let status = StatusCode::from_u16(upstream.status().as_u16())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    tracing::info!(%url, %status, "native passthrough");
    Ok(Response::builder()
        .status(status)
        .body(Body::from_stream(upstream.bytes_stream()))?)
}

/// Transform an upstream SSE byte stream into the downstream wire format.
fn translate_byte_stream(
    upstream: reqwest::Response,
    upstream_kind: UpstreamKind,
    downstream_kind: DownstreamKind,
    model: &str,
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> {
    struct St {
        bytes: futures::stream::BoxStream<'static, Result<Bytes, reqwest::Error>>,
        parser: SseParser,
        translator: StreamTranslator,
        pending: VecDeque<Bytes>,
        upstream_done: bool,
        finalized: bool,
    }

    let state = St {
        bytes: upstream.bytes_stream().boxed(),
        parser: SseParser::new(),
        translator: StreamTranslator::new(upstream_kind, downstream_kind, model),
        pending: VecDeque::new(),
        upstream_done: false,
        finalized: false,
    };

    futures::stream::unfold(state, move |mut st| async move {
        loop {
            if let Some(b) = st.pending.pop_front() {
                return Some((Ok(b), st));
            }
            if st.upstream_done {
                if !st.finalized {
                    st.finalized = true;
                    for f in st.translator.finalize() {
                        push_frame(&mut st.pending, &f, downstream_kind);
                    }
                    continue;
                }
                return None;
            }
            match st.bytes.next().await {
                Some(Ok(chunk)) => {
                    for ev in st.parser.push(&chunk) {
                        for f in st.translator.push_event(ev.event.as_deref(), &ev.data) {
                            push_frame(&mut st.pending, &f, downstream_kind);
                        }
                    }
                }
                Some(Err(e)) => {
                    tracing::warn!("upstream stream error: {e}");
                    st.upstream_done = true;
                }
                None => st.upstream_done = true,
            }
        }
    })
}

fn push_frame(
    pending: &mut VecDeque<Bytes>,
    f: &translate::OutFrame,
    downstream: DownstreamKind,
) {
    if f.done_marker {
        if downstream == DownstreamKind::ChatCompletions {
            pending.push_back(Bytes::from(frame_done()));
        }
        return;
    }
    let bytes = match (&f.event, downstream) {
        (Some(ev), DownstreamKind::Responses) => frame_with_event(ev, &f.data),
        _ => frame_data(&f.data),
    };
    pending.push_back(Bytes::from(bytes));
}
