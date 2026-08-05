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
use crate::stats::SharedStats;
use crate::translate::{self, DownstreamKind, StreamTranslator, UpstreamKind};
use anyhow::{anyhow, bail};
use axum::{
    body::Body,
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Request, State as AxState,
    },
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::OnceLock;

#[derive(Clone)]
struct ProxyCtx {
    config: SharedConfig,
    stats: SharedStats,
    client: reqwest::Client,
}

// ---------------------------------------------------------------------------
// Local authentication (S3)
//
// The proxy listens on 127.0.0.1, but any local process (or a malicious
// webpage, since browsers can freely POST to localhost) could otherwise
// spend the stored API keys. Every route therefore requires a shared local
// token, generated once per process. The Codex integration writes the token
// into the managed block of Codex's config.toml (both as
// `x-loomrouter-token` and `Authorization: Bearer`), so Codex authenticates
// automatically; WS clients may use `?token=` when headers are not an
// option.
// ---------------------------------------------------------------------------

static LOCAL_TOKEN: OnceLock<String> = OnceLock::new();

/// Shared secret required on every request to the local proxy.
/// Generated once per process from 32 random bytes (two UUIDv4s = 64 hex
/// chars; `uuid` is the only RNG crate already in Cargo.toml).
pub fn local_token() -> &'static str {
    LOCAL_TOKEN.get_or_init(|| {
        format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        )
    })
}

/// Constant-time token comparison (cheap to do, avoids leaking the prefix
/// through timing on a local port).
fn token_eq(given: &str) -> bool {
    let expected = local_token().as_bytes();
    let given = given.as_bytes();
    if given.len() != expected.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in given.iter().zip(expected.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

fn is_authorized(req: &Request) -> bool {
    if let Some(v) = req
        .headers()
        .get("x-loomrouter-token")
        .and_then(|v| v.to_str().ok())
    {
        if token_eq(v.trim()) {
            return true;
        }
    }
    if let Some(v) = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
    {
        if let Some(t) = v.strip_prefix("Bearer ") {
            if token_eq(t.trim()) {
                return true;
            }
        }
    }
    // WS clients that cannot set headers authenticate via the query string.
    if let Some(query) = req.uri().query() {
        for pair in query.split('&') {
            if let Some(v) = pair.strip_prefix("token=") {
                if token_eq(v) {
                    return true;
                }
            }
        }
    }
    false
}

async fn auth_gate(req: Request, next: Next) -> Response {
    if is_authorized(&req) {
        return next.run(req).await;
    }
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header("content-type", "application/json")
        .body(Body::from(
            "{\"error\":{\"message\":\"loom-router: missing or invalid local token\"}}",
        ))
        .unwrap()
}

// ---------------------------------------------------------------------------
// Provider families (D3/D4): coarse grouping derived from the base URL,
// used for routing quirks (e.g. OpenRouter's unified reasoning object) and
// shared with the other modules via the `pub` API below.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderFamily {
    Anthropic,
    OpenRouter,
    Kimi,
    DeepSeek,
    OpenAi,
}

pub fn family_of(p: &crate::providers::Provider) -> ProviderFamily {
    let url = p.base_url.to_ascii_lowercase();
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

/// Apply the provider's upstream authentication to an outgoing request.
/// The scheme follows the wire protocol, not the URL family: gateways like
/// OpenCode Zen speak the Anthropic protocol (and expect `x-api-key`) on a
/// non-Anthropic URL.
pub fn apply_provider_auth(
    req: reqwest::RequestBuilder,
    p: &crate::providers::Provider,
) -> reqwest::RequestBuilder {
    let Some(key) = p.api_key.as_deref() else {
        return req;
    };
    match p.protocol {
        ProviderProtocol::Anthropic => req
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01"),
        ProviderProtocol::OpenAI | ProviderProtocol::Responses => req.bearer_auth(key),
    }
}

pub fn router(config: SharedConfig, stats: SharedStats) -> Router {
    // Materialize the local token at startup so the first request never
    // pays (or races) initialization.
    let _ = local_token();
    let ctx = ProxyCtx {
        config,
        stats,
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
        // Every route (including /health and the WS upgrade) requires the
        // local token; see `auth_gate`.
        .layer(middleware::from_fn(auth_gate))
}

async fn health() -> &'static str {
    "ok"
}

/// Record a completed turn's usage in the background (SQLite insert).
fn record_usage(
    stats: &SharedStats,
    provider: &str,
    model: &str,
    transport: &'static str,
    started: Option<std::time::Instant>,
    usage: &Value,
) {
    if usage.is_null() {
        return;
    }
    let latency_ms = started.map(|s| s.elapsed().as_millis() as u64);
    let Some(entry) = crate::stats::RequestEntry::ok(provider, model, transport, latency_ms, usage)
    else {
        return;
    };
    let stats = stats.clone();
    tokio::spawn(async move {
        stats.read().await.record_entry(entry);
    });
}

/// Record a failed turn (upstream error, routing failure) in the background.
fn record_failure(
    stats: &SharedStats,
    provider: &str,
    model: &str,
    transport: &'static str,
    started: Option<std::time::Instant>,
    error: &str,
) {
    let latency_ms = started.map(|s| s.elapsed().as_millis() as u64);
    let entry = crate::stats::RequestEntry::error(provider, model, transport, latency_ms, error);
    let stats = stats.clone();
    tokio::spawn(async move {
        stats.read().await.record_entry(entry);
    });
}

/// Codex occasionally calls paths we do not route (compaction, item
/// retrieval, probes). Log them so gaps are visible instead of silent.
/// S7: never log body content — it carries user prompts and source code.
async fn log_unmatched(method: axum::http::Method, uri: axum::http::Uri, body: Bytes) -> Response {
    tracing::warn!(%method, path = %uri.path(), body_len = body.len(), "unmatched request");
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
/// S7: log metadata only (path, error, byte length), never body content.
fn parse_body(body: &Bytes, path: &str) -> Result<Value, (StatusCode, String)> {
    serde_json::from_slice::<Value>(body).map_err(|e| {
        tracing::warn!(path, error = %e, body_len = body.len(), "bad JSON body");
        (
            StatusCode::BAD_REQUEST,
            format!("invalid JSON body for {path}: {e}"),
        )
    })
}

/// S2: POST routes only accept real JSON requests. `text/plain` fetch is a
/// CORS "simple request" (no preflight), so any webpage could otherwise
/// trigger paid upstream calls. `Sec-Fetch-Site: cross-site` is rejected
/// outright.
fn enforce_json_post(headers: &HeaderMap) -> Result<(), (StatusCode, String)> {
    if let Some(site) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        if site.eq_ignore_ascii_case("cross-site") {
            return Err((
                StatusCode::FORBIDDEN,
                "cross-site requests are not allowed".to_string(),
            ));
        }
    }
    let ct = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let mime = ct.split(';').next().unwrap_or("").trim();
    if !mime.eq_ignore_ascii_case("application/json") {
        return Err((
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Content-Type must be application/json".to_string(),
        ));
    }
    Ok(())
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
/// Borrows the provider from the config; callers clone only the single
/// resolved provider instead of the whole AppConfig (P1).
fn resolve<'a>(
    config: &'a crate::config::AppConfig,
    model: &str,
) -> anyhow::Result<(&'a Provider, String)> {
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
        return Ok((p, upstream));
    }

    for p in config.providers.values().filter(|p| p.enabled) {
        if p.models.iter().any(|m| m.enabled && m.id == model) {
            return Ok((p, model.to_string()));
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
    if provider.api_key.is_none() {
        bail!("provider '{}' has no API key", provider.id);
    }

    let mut req = ctx.client.post(&url).json(body);
    if let Some(ua) = &provider.user_agent {
        req = req.header("user-agent", ua);
    }
    req = apply_provider_auth(req, provider);
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
    enforce_json_post(&headers)?;
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
    enforce_json_post(&headers)?;
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
    enforce_json_post(&headers)?;
    let payload = parse_body(&body, "/v1/chat/completions")?;
    dispatch(ctx, headers, payload, WireApi::ChatCompletions)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))
}

/// Build the upstream request (path, body, upstream kind) for a routed
/// provider — the single translation pipeline shared by the HTTP `dispatch`
/// and the WS `ws_turn_events` paths (D2). Covers every
/// (provider protocol × downstream wire) combination, including
/// Responses-protocol + ChatCompletions-wire, which the WS path used to
/// miss.
fn build_upstream(
    provider: &Provider,
    payload: &Value,
    upstream_model: &str,
    wire: WireApi,
) -> anyhow::Result<(&'static str, Value, UpstreamKind)> {
    // OpenRouter speaks the unified reasoning object; everyone else gets
    // OpenAI-style reasoning_effort (sending both = 400 conflict there).
    let unified_reasoning = family_of(provider) == ProviderFamily::OpenRouter;
    match (&provider.protocol, wire) {
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
            // Responses-native upstream (e.g. OpenCode Zen GPT models):
            // forward the payload nearly untouched, just swap the model.
            let mut body = payload.clone();
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

    // P1: read the config once per request and clone only the single
    // resolved provider, instead of cloning the whole AppConfig (every
    // provider, model and API key) per request. state.rs exposes
    // Arc<RwLock<AppConfig>>, so an Arc<AppConfig> swap is not possible
    // without touching state.rs; this is the minimal-copy version.
    let resolved = {
        let cfg = ctx.config.read().await;
        resolve(&cfg, &model).map(|(p, m)| (p.clone(), m))
    };
    let (provider, upstream_model) = match resolved {
        Ok(r) => r,
        // Not an external model: native GPT models are forwarded unchanged
        // to OpenAI's backend with the caller's own ChatGPT credentials, so
        // the native models in the picker keep working through the proxy.
        Err(_) => return forward_native(&ctx, wire, &_headers, payload).await,
    };

    tracing::info!(%model, provider = %provider.id, %upstream_model, stream = wants_stream, "routing request");
    let started = std::time::Instant::now();

    let (path, body, upstream_kind) = build_upstream(&provider, &payload, &upstream_model, wire)?;

    let upstream = send(&ctx, &provider, path, &body).await?;
    let status = StatusCode::from_u16(upstream.status().as_u16())
        .unwrap_or(StatusCode::BAD_GATEWAY);

    // Error or same-format pass-through: stream untouched.
    let same_format = matches!(
        (upstream_kind, wire),
        (UpstreamKind::OpenAiChat, WireApi::ChatCompletions)
    );
    if !status.is_success() || same_format {
        if !status.is_success() {
            record_failure(
                &ctx.stats,
                &provider.id,
                &model,
                "http",
                Some(started),
                &format!("upstream returned {status}"),
            );
        }
        return Ok(Response::builder()
            .status(status)
            .body(Body::from_stream(upstream.bytes_stream()))?);
    }

    // Responses-native upstream: the downstream wire is already Responses,
    // so pass bytes through and only tap usage for stats/logs.
    if upstream_kind == UpstreamKind::Responses {
        if wants_stream {
            return Ok(Response::builder()
                .status(status)
                .header("content-type", "text/event-stream")
                .header("cache-control", "no-cache")
                .body(Body::from_stream(tap_responses_stream(
                    upstream,
                    ctx.stats.clone(),
                    provider.id.clone(),
                    model.clone(),
                    started,
                )))?);
        }
        let json: Value = upstream.json().await?;
        record_usage(
            &ctx.stats,
            &provider.id,
            &model,
            "http",
            Some(started),
            &json["usage"],
        );
        return Ok(Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(Body::from(json.to_string()))?);
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
                Some((
                    ctx.stats.clone(),
                    provider.id.clone(),
                    model.clone(),
                    started,
                )),
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
        record_usage(
            &ctx.stats,
            &provider.id,
            &model,
            "http",
            Some(started),
            &translated["usage"],
        );
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

/// S1: WebSocket upgrades are not subject to the Same-Origin Policy, so any
/// webpage open in a browser could connect to the proxy and spend the stored
/// API keys (including the relayed ChatGPT token). Reject the upgrade when an
/// Origin header is present and is not a trusted local origin. Non-browser
/// clients (Codex CLI) send no Origin and are allowed.
fn is_trusted_ws_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let Some(rest) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    else {
        return false;
    };
    let host = rest.split([':', '/']).next().unwrap_or("");
    matches!(host, "localhost" | "127.0.0.1" | "tauri.localhost")
}

async fn handle_responses_ws(
    AxState(ctx): AxState<ProxyCtx>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !is_trusted_ws_origin(&headers) {
        tracing::warn!("WS upgrade rejected: untrusted Origin");
        return Response::builder()
            .status(StatusCode::FORBIDDEN)
            .header("content-type", "application/json")
            .body(Body::from(
                "{\"error\":{\"message\":\"loom-router: untrusted Origin\"}}",
            ))
            .unwrap();
    }
    ws.on_upgrade(move |socket| ws_session(socket, ctx, headers))
        .into_response()
}

/// P5: per-connection history bounds. Routed providers are stateless, so
/// every incremental turn replays the full item list, and each cached entry
/// holds the whole conversation so far — unbounded growth is O(n²) memory
/// per conversation and only freed on disconnect. We cap entries and total
/// serialized size, evicting the oldest first. Trade-off: in very long
/// conversations the oldest turns are forgotten, so a `previous_response_id`
/// pointing at an evicted entry degrades to a delta-only turn.
const WS_HISTORY_MAX_ENTRIES: usize = 100;
const WS_HISTORY_MAX_BYTES: usize = 512 * 1024;

struct WsHistory {
    map: std::collections::HashMap<String, Vec<Value>>,
    /// Insertion order with per-entry serialized size, for FIFO eviction.
    order: VecDeque<(String, usize)>,
    total_bytes: usize,
}

impl WsHistory {
    fn new() -> Self {
        Self {
            map: std::collections::HashMap::new(),
            order: VecDeque::new(),
            total_bytes: 0,
        }
    }

    fn get(&self, id: &str) -> Option<&Vec<Value>> {
        self.map.get(id)
    }

    fn insert(&mut self, rid: String, record: Vec<Value>) {
        if self.map.contains_key(&rid) {
            return;
        }
        let size = record.iter().map(|v| v.to_string().len()).sum::<usize>();
        self.map.insert(rid.clone(), record);
        self.order.push_back((rid, size));
        self.total_bytes += size;
        while self.order.len() > WS_HISTORY_MAX_ENTRIES || self.total_bytes > WS_HISTORY_MAX_BYTES
        {
            let Some((old_id, old_size)) = self.order.pop_front() else {
                break;
            };
            if self.map.remove(&old_id).is_some() {
                self.total_bytes = self.total_bytes.saturating_sub(old_size);
            }
        }
    }
}

async fn ws_session(socket: WebSocket, ctx: ProxyCtx, headers: HeaderMap) {
    let (mut tx, mut rx) = socket.split();
    let mut history = WsHistory::new();

    while let Some(msg) = rx.next().await {
        let Ok(msg) = msg else { break };
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };
        let Ok(mut payload) = serde_json::from_str::<Value>(&text) else {
            // S7: never log frame content — it carries user prompts.
            tracing::warn!(frame_len = text.len(), "bad WS frame");
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
            let cfg = ctx.config.read().await;
            resolve(&cfg, &model).ok().map(|(p, m)| (p.clone(), m))
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
        let turn_start = std::time::Instant::now();

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
                                let label = routed
                                    .as_ref()
                                    .map(|(p, _)| p.id.clone())
                                    .unwrap_or_else(|| "codex-native".to_string());
                                let usage =
                                    v.pointer("/response/usage").cloned().unwrap_or(Value::Null);
                                record_usage(
                                    &ctx.stats,
                                    &label,
                                    &model,
                                    "ws",
                                    Some(turn_start),
                                    &usage,
                                );
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
                let label = routed
                    .as_ref()
                    .map(|(p, _)| p.id.clone())
                    .unwrap_or_else(|| "codex-native".to_string());
                record_failure(
                    &ctx.stats,
                    &label,
                    &model,
                    "ws",
                    Some(turn_start),
                    &e.to_string(),
                );
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

    let resolved = {
        let cfg = ctx.config.read().await;
        resolve(&cfg, &model).map(|(p, m)| (p.clone(), m))
    };
    match resolved {
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
            // Same translation pipeline as the HTTP dispatch (D2).
            // Responses-native upstreams relay events untouched (no
            // translator); chat/anthropic upstreams get one.
            let (path, body, upstream_kind) =
                build_upstream(&provider, &payload, &upstream_model, WireApi::Responses)?;
            let translator = match upstream_kind {
                UpstreamKind::Responses => None,
                kind => Some((kind, model.clone())),
            };
            let upstream = send(ctx, &provider, path, &body).await?;
            let status = upstream.status();
            if !status.is_success() {
                let body = upstream.text().await.unwrap_or_default();
                let preview: String = body.chars().take(300).collect();
                bail!("provider '{}' returned {status}: {preview}", provider.id);
            }
            Ok(sse_values_stream(upstream, translator))
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

/// Forward a Responses-format SSE stream byte-for-byte, tapping the
/// terminal response.completed event to record usage.
fn tap_responses_stream(
    upstream: reqwest::Response,
    stats: SharedStats,
    provider: String,
    model: String,
    started: std::time::Instant,
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> {
    // P3: stats/provider/model/started live in the state struct (built once)
    // instead of being cloned on every SSE chunk.
    struct St {
        bytes: futures::stream::BoxStream<'static, Result<Bytes, reqwest::Error>>,
        parser: SseParser,
        recorded: bool,
        stats: SharedStats,
        provider: String,
        model: String,
        started: std::time::Instant,
    }
    let state = St {
        bytes: upstream.bytes_stream().boxed(),
        parser: SseParser::new(),
        recorded: false,
        stats,
        provider,
        model,
        started,
    };
    futures::stream::unfold(state, |mut st| async move {
        match st.bytes.next().await {
            Some(Ok(chunk)) => {
                if !st.recorded {
                    for ev in st.parser.push(&chunk) {
                        let is_completed = ev.event.as_deref() == Some("response.completed")
                            || ev.data.contains("\"response.completed\"");
                        if is_completed {
                            if let Ok(v) = serde_json::from_str::<Value>(&ev.data) {
                                let usage = v
                                    .pointer("/response/usage")
                                    .cloned()
                                    .unwrap_or(Value::Null);
                                if !usage.is_null() {
                                    st.recorded = true;
                                    record_usage(
                                        &st.stats,
                                        &st.provider,
                                        &st.model,
                                        "http",
                                        Some(st.started),
                                        &usage,
                                    );
                                }
                            }
                        }
                    }
                }
                Some((Ok(chunk), st))
            }
            Some(Err(e)) => Some((
                Err(std::io::Error::new(std::io::ErrorKind::Other, e)),
                st,
            )),
            None => None,
        }
    })
}

/// Transform an upstream SSE byte stream into the downstream wire format.
/// When `tap` is set, completed Responses turns report their usage.
fn translate_byte_stream(
    upstream: reqwest::Response,
    upstream_kind: UpstreamKind,
    downstream_kind: DownstreamKind,
    model: &str,
    tap: Option<(SharedStats, String, String, std::time::Instant)>,
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> {
    struct St {
        bytes: futures::stream::BoxStream<'static, Result<Bytes, reqwest::Error>>,
        parser: SseParser,
        translator: StreamTranslator,
        pending: VecDeque<Bytes>,
        upstream_done: bool,
        finalized: bool,
        tap: Option<(SharedStats, String, String, std::time::Instant)>,
    }

    let state = St {
        bytes: upstream.bytes_stream().boxed(),
        parser: SseParser::new(),
        translator: StreamTranslator::new(upstream_kind, downstream_kind, model),
        pending: VecDeque::new(),
        upstream_done: false,
        finalized: false,
        tap,
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
                        if let Some((stats, prov, mdl, started)) = &st.tap {
                            if f.event.as_deref() == Some("response.completed") {
                                let usage = f.data.pointer("/response/usage").cloned().unwrap_or(Value::Null);
                                record_usage(stats, prov, mdl, "http", Some(*started), &usage);
                            }
                        }
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
                            if let Some((stats, prov, mdl, started)) = &st.tap {
                                if f.event.as_deref() == Some("response.completed") {
                                    let usage = f.data.pointer("/response/usage").cloned().unwrap_or(Value::Null);
                                    record_usage(stats, prov, mdl, "http", Some(*started), &usage);
                                }
                            }
                            push_frame(&mut st.pending, &f, downstream_kind);
                        }
                    }
                }
                Some(Err(e)) => {
                    // B3: a mid-stream upstream error used to fall through to
                    // finalize(), which emitted `response.completed` — the
                    // client saw a truncated turn marked as successful and
                    // nothing was recorded as a failure. Mirror the WS path:
                    // emit an explicit error event, record the failure, and
                    // skip finalize() so no `response.completed` follows.
                    tracing::warn!("upstream stream error: {e}");
                    let message = format!("upstream stream error: {e}");
                    if let Some((stats, prov, mdl, started)) = &st.tap {
                        record_failure(stats, prov, mdl, "http", Some(*started), &message);
                    }
                    let err_event = match downstream_kind {
                        DownstreamKind::Responses => json!({
                            "type": "error",
                            "status": 502,
                            "error": {"code": Value::Null, "message": message},
                        }),
                        DownstreamKind::ChatCompletions => json!({
                            "error": {"message": message, "type": "upstream_stream_error"},
                        }),
                    };
                    let bytes = match downstream_kind {
                        DownstreamKind::Responses => frame_with_event("error", &err_event),
                        DownstreamKind::ChatCompletions => frame_data(&err_event),
                    };
                    st.pending.push_back(Bytes::from(bytes));
                    if downstream_kind == DownstreamKind::ChatCompletions {
                        st.pending.push_back(Bytes::from(frame_done()));
                    }
                    st.upstream_done = true;
                    st.finalized = true; // skip finalize(): failed turns never complete
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
