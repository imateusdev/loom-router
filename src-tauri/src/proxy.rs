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
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State as AxState,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
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
        .route("/v1/responses", get(handle_responses_ws).post(handle_responses))
        .route("/v1/responses/compact", post(handle_compact))
        .route("/v1/chat/completions", post(handle_chat_completions))
        .fallback(log_unmatched)
        .with_state(ctx)
        // The Codex App sends request bodies compressed (gzip/br/zstd).
        // Decompress transparently before handlers see the bytes.
        .layer(tower_http::decompression::RequestDecompressionLayer::new())
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

/// Remote compaction: Codex asks the native backend to summarize the
/// conversation (uses the caller's ChatGPT auth, runs on OpenAI models).
/// Always forwarded untouched, regardless of the routed model.
async fn handle_compact(
    AxState(ctx): AxState<ProxyCtx>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, (StatusCode, String)> {
    let payload = parse_body(&body, "/v1/responses/compact")?;
    let base = std::env::var("CODEX_NATIVE_BASE_URL")
        .unwrap_or_else(|_| "https://chatgpt.com/backend-api/codex".to_string());
    let url = format!("{}/responses/compact", base.trim_end_matches('/'));

    let mut req = ctx.client.post(&url).json(&payload);
    for name in NATIVE_FORWARD_HEADERS {
        if let Some(value) = headers.get(*name) {
            if let Ok(v) = value.to_str() {
                req = req.header(*name, v);
            }
        }
    }
    let upstream = req
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
    let status = StatusCode::from_u16(upstream.status().as_u16())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    tracing::info!(%url, %status, "compact passthrough");
    Ok(Response::builder()
        .status(status)
        .body(Body::from_stream(upstream.bytes_stream()))
        .unwrap())
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
    let upstream = native_send(ctx, wire, headers, &payload).await?;
    let status = StatusCode::from_u16(upstream.status().as_u16())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    tracing::info!(%status, "native passthrough");
    Ok(Response::builder()
        .status(status)
        .body(Body::from_stream(upstream.bytes_stream()))?)
}

/// Headers relayed to the native backend so ChatGPT auth and session
/// telemetry keep working through the proxy.
const NATIVE_FORWARD_HEADERS: &[&str] = &[
    "authorization",
    "chatgpt-account-id",
    "openai-beta",
    "originator",
    "session_id",
    "user-agent",
    "accept",
    "version",
];

async fn native_send(
    ctx: &ProxyCtx,
    wire: WireApi,
    headers: &HeaderMap,
    payload: &Value,
) -> anyhow::Result<reqwest::Response> {
    let base = std::env::var("CODEX_NATIVE_BASE_URL")
        .unwrap_or_else(|_| "https://chatgpt.com/backend-api/codex".to_string());
    let path = match wire {
        WireApi::Responses => "/responses",
        WireApi::ChatCompletions => "/chat/completions",
    };
    let url = format!("{}{}", base.trim_end_matches('/'), path);

    let mut req = ctx.client.post(&url).json(payload);
    for name in NATIVE_FORWARD_HEADERS {
        if let Some(value) = headers.get(*name) {
            if let Ok(v) = value.to_str() {
                req = req.header(*name, v);
            }
        }
    }
    let res = req.send().await?;
    tracing::info!(%url, status = %res.status(), "native passthrough");
    Ok(res)
}

// ---------------------------------------------------------------------------
// WebSocket transport (Responses over WS, Codex v2 protocol)
//
// Codex sends one text frame per turn: the usual Responses request JSON plus
// `"type": "response.create"`. The server answers with one text frame per
// response event (the same JSON objects SSE carries in `data:`), ending with
// `response.completed`. Errors are `{"type":"error","status":N,"error":{...}}`.
//
// Follow-up turns may arrive as `previous_response_id` + incremental input.
// The native backend stores prior turns, but routed providers do not, so we
// cache each routed turn's full item list per connection and rebuild the
// complete input before forwarding.
// ---------------------------------------------------------------------------

async fn handle_responses_ws(
    AxState(ctx): AxState<ProxyCtx>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    ws.on_upgrade(move |socket| ws_session(socket, ctx, headers))
        .into_response()
}

async fn ws_session(socket: WebSocket, ctx: ProxyCtx, headers: HeaderMap) {
    let (mut tx, mut rx) = socket.split();
    let mut history: std::collections::HashMap<String, Vec<Value>> =
        std::collections::HashMap::new();

    while let Some(msg) = rx.next().await {
        let Ok(msg) = msg else { break };
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };
        let Ok(mut payload) = serde_json::from_str::<Value>(&text) else {
            let preview: String = text.chars().take(200).collect();
            tracing::warn!(body = %preview, "bad WS frame");
            continue;
        };
        match payload.get("type").and_then(Value::as_str) {
            Some("response.create") => {
                if let Some(m) = payload.as_object_mut() {
                    m.remove("type");
                }
            }
            // Best effort: in-flight turns are not interrupted.
            Some("response.cancel") => continue,
            _ => continue,
        }
        payload["stream"] = Value::Bool(true);

        let model = payload
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let routed = {
            let cfg = ctx.config.read().await.clone();
            resolve(&cfg, &model).ok()
        };

        // Rebuild full input for routed models on incremental turns.
        let mut full_input_items: Option<Vec<Value>> = None;
        if routed.is_some() {
            let prev = payload
                .get("previous_response_id")
                .and_then(Value::as_str)
                .map(str::to_string);
            let delta = payload
                .get("input")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let items = match prev.as_deref().and_then(|id| history.get(id)) {
                Some(base) => {
                    let mut v = base.clone();
                    v.extend(delta);
                    v
                }
                None => delta,
            };
            payload["input"] = Value::Array(items.clone());
            if let Some(m) = payload.as_object_mut() {
                m.remove("previous_response_id");
            }
            full_input_items = Some(items);
        }

        let mut output_items: Vec<Value> = Vec::new();
        let mut completed_response_id: Option<String> = None;

        match ws_turn_events(&ctx, &headers, payload).await {
            Ok(mut events) => {
                while let Some(item) = events.next().await {
                    let frame = match &item {
                        Ok(v) => v.clone(),
                        Err(e) => ws_error_frame(502, e),
                    };
                    if let Ok(v) = &item {
                        match v.get("type").and_then(Value::as_str) {
                            Some("response.output_item.done") => {
                                if let Some(it) = v.get("item") {
                                    output_items.push(it.clone());
                                }
                            }
                            Some("response.completed") => {
                                completed_response_id = v
                                    .pointer("/response/id")
                                    .and_then(Value::as_str)
                                    .map(str::to_string);
                            }
                            _ => {}
                        }
                    }
                    let done =
                        frame.get("type").and_then(Value::as_str) == Some("response.completed");
                    if tx.send(Message::Text(frame.to_string().into())).await.is_err() {
                        return;
                    }
                    if done {
                        break;
                    }
                }
            }
            Err(e) => {
                let frame = ws_error_frame(502, &e.to_string());
                let _ = tx.send(Message::Text(frame.to_string().into())).await;
            }
        }

        if let (Some(items), Some(rid)) = (full_input_items, completed_response_id) {
            let mut record = items;
            record.extend(output_items);
            history.insert(rid, record);
        }
    }
}

fn ws_error_frame(status: u16, message: &str) -> Value {
    json!({
        "type": "error",
        "status": status,
        "error": {"code": Value::Null, "message": message},
    })
}

/// Run one turn and return a stream of Responses event objects ready to be
/// sent as WS text frames.
async fn ws_turn_events(
    ctx: &ProxyCtx,
    headers: &HeaderMap,
    payload: Value,
) -> anyhow::Result<futures::stream::BoxStream<'static, Result<Value, String>>> {
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing 'model' field"))?
        .to_string();

    let cfg = ctx.config.read().await.clone();
    match resolve(&cfg, &model) {
        // Native GPT model: relay the backend's SSE events as WS frames.
        Err(_) => {
            let upstream = native_send(ctx, WireApi::Responses, headers, &payload).await?;
            let status = upstream.status();
            if !status.is_success() {
                let body = upstream.text().await.unwrap_or_default();
                let preview: String = body.chars().take(300).collect();
                bail!("native upstream returned {status}: {preview}");
            }
            Ok(sse_values_stream(upstream, None))
        }
        Ok((provider, upstream_model)) => {
            tracing::info!(%model, provider = %provider.id, %upstream_model, transport = "ws", "routing request");
            let (path, body, upstream_kind) = match provider.protocol {
                ProviderProtocol::OpenAI => (
                    "chat/completions",
                    translate::responses_to_chat(&payload, &upstream_model)?,
                    UpstreamKind::OpenAiChat,
                ),
                ProviderProtocol::Anthropic => {
                    let chat = translate::responses_to_chat(&payload, &upstream_model)?;
                    (
                        "messages",
                        translate::chat_to_anthropic(&chat, &upstream_model)?,
                        UpstreamKind::Anthropic,
                    )
                }
            };
            let upstream = send(ctx, &provider, path, &body).await?;
            let status = upstream.status();
            if !status.is_success() {
                let body = upstream.text().await.unwrap_or_default();
                let preview: String = body.chars().take(300).collect();
                bail!("provider '{}' returned {status}: {preview}", provider.id);
            }
            Ok(sse_values_stream(upstream, Some((upstream_kind, model))))
        }
    }
}

/// Parse an upstream SSE byte stream into Responses event objects. With a
/// translator, upstream chat/anthropic events are converted to the Responses
/// format; without one, the payloads pass through untouched.
fn sse_values_stream(
    upstream: reqwest::Response,
    translator: Option<(UpstreamKind, String)>,
) -> futures::stream::BoxStream<'static, Result<Value, String>> {
    struct St {
        bytes: futures::stream::BoxStream<'static, Result<Bytes, reqwest::Error>>,
        parser: SseParser,
        translator: Option<StreamTranslator>,
        pending: VecDeque<Value>,
        upstream_done: bool,
        finalized: bool,
    }

    let state = St {
        bytes: upstream.bytes_stream().boxed(),
        parser: SseParser::new(),
        translator: translator
            .map(|(kind, model)| StreamTranslator::new(kind, DownstreamKind::Responses, &model)),
        pending: VecDeque::new(),
        upstream_done: false,
        finalized: false,
    };

    futures::stream::unfold(state, |mut st| async move {
        loop {
            if let Some(v) = st.pending.pop_front() {
                return Some((Ok(v), st));
            }
            if st.upstream_done {
                if !st.finalized {
                    st.finalized = true;
                    if let Some(t) = st.translator.as_mut() {
                        for f in t.finalize() {
                            if !f.done_marker {
                                st.pending.push_back(f.data);
                            }
                        }
                    }
                    continue;
                }
                return None;
            }
            match st.bytes.next().await {
                Some(Ok(chunk)) => {
                    for ev in st.parser.push(&chunk) {
                        if let Some(t) = st.translator.as_mut() {
                            for f in t.push_event(ev.event.as_deref(), &ev.data) {
                                if !f.done_marker {
                                    st.pending.push_back(f.data);
                                }
                            }
                        } else if ev.data.trim() != "[DONE]" {
                            if let Ok(v) = serde_json::from_str::<Value>(&ev.data) {
                                st.pending.push_back(v);
                            }
                        }
                    }
                }
                Some(Err(e)) => {
                    return Some((Err(format!("upstream stream error: {e}")), st));
                }
                None => st.upstream_done = true,
            }
        }
    })
    .boxed()
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
