//! Local proxy: receives requests from the coding agent and dispatches
//! them to the right provider based on the `model` field, translating
//! both the request and the response (including SSE streams).
//!
//! Endpoints (all bound to 127.0.0.1):
//!   POST /v1/responses        - Codex Responses API
//!   POST /v1/chat/completions - OpenAI-compatible clients
//!   GET  /health              - liveness for the UI

use crate::config::{AppConfig, Provider, ProviderProtocol};
use crate::sse::{frame_data, frame_done, frame_with_event, SseParser};
use crate::state::SharedConfig;
use crate::stats::{
    SharedStats, VisualAssistanceMetadata, VisualAttemptProvenance, VisualImageProvenance,
};
use crate::translate::{self, DownstreamKind, StreamTranslator, UpstreamKind};
use crate::visual::{self, ImagePart};
use anyhow::{anyhow, bail};
use axum::{
    body::Body,
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        DefaultBodyLimit, Request, State as AxState,
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
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};

// Codex remote compaction can legitimately carry a multi-megabyte transcript.
// Keep a finite bound while exceeding Axum's 2 MiB default for `Bytes`.
const MAX_REQUEST_BODY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone)]
struct ProxyCtx {
    config: SharedConfig,
    stats: SharedStats,
    client: reqwest::Client,
    /// Routed-turn history shared across WebSocket connections. Routed
    /// providers are stateless, so each incremental follow-up turn replays
    /// the full item list; the cache is what lets that rebuild happen. It is
    /// connection-scoped for capacity reasons but *shared* because a Codex
    /// reconnect (idle timeout, network blip) creates a new WS session with
    /// the conversation's thread still alive - a per-session cache would
    /// lose everything on reconnect and reset the context window to zero.
    history: Arc<Mutex<WsHistory>>,
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

/// The wire dialect one model is served in.
///
/// A provider's `protocol` is only the default. OpenCode puts three dialects
/// behind a single URL and key, so a model that names its own wins - and
/// anything untagged (every ordinary endpoint, and every model discovery
/// turned up before someone said otherwise) falls back to the provider's.
pub fn model_protocol<'a>(
    p: &'a crate::providers::Provider,
    model_id: &str,
) -> &'a ProviderProtocol {
    p.models
        .iter()
        .find(|m| m.id == model_id)
        .and_then(|m| m.protocol.as_ref())
        .unwrap_or(&p.protocol)
}

/// Apply the provider's upstream authentication to an outgoing request.
/// The scheme follows the wire protocol, not the URL family: gateways like
/// OpenCode Zen speak the Anthropic protocol (and expect `x-api-key`) on a
/// non-Anthropic URL - and they do it for some of their models only, which
/// is why the scheme is resolved per model. `None` (catalog fetches, balance
/// probes: requests that belong to no model) uses the provider's own.
pub fn apply_provider_auth(
    req: reqwest::RequestBuilder,
    p: &crate::providers::Provider,
    model_id: Option<&str>,
) -> reqwest::RequestBuilder {
    let Some(key) = p.api_key.as_deref() else {
        return req;
    };
    let protocol = match model_id {
        Some(id) => model_protocol(p, id),
        None => &p.protocol,
    };
    match protocol {
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
        history: Arc::new(Mutex::new(WsHistory::new())),
    };
    Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(handle_models))
        .route(
            "/v1/responses",
            get(handle_responses_ws).post(handle_responses),
        )
        .route("/v1/responses/compact", post(handle_compact))
        .route("/v1/chat/completions", post(handle_chat_completions))
        .fallback(log_unmatched)
        .with_state(ctx)
        // The Codex App sends request bodies compressed (gzip/br/zstd).
        // Decompress transparently before handlers see the bytes.
        .layer(tower_http::decompression::RequestDecompressionLayer::new())
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        // Every route (including /health and the WS upgrade) requires the
        // local token; see `auth_gate`.
        .layer(middleware::from_fn(auth_gate))
}

async fn health() -> &'static str {
    "ok"
}

/// Record a completed turn's usage in the background (SQLite insert).
#[allow(clippy::too_many_arguments)] // why: one flat recorder all dialects share
fn record_usage_with_kind(
    stats: &SharedStats,
    provider: &str,
    model: &str,
    transport: &'static str,
    started: Option<std::time::Instant>,
    usage: &Value,
    visual_assistance: Option<&VisualAssistanceMetadata>,
    kind: &str,
) {
    if usage.is_null() {
        return;
    }
    let latency_ms = started.map(|s| s.elapsed().as_millis() as u64);
    let Some(entry) = crate::stats::RequestEntry::ok(provider, model, transport, latency_ms, usage)
    else {
        return;
    };
    let entry = entry
        .with_kind(kind)
        .with_visual_assistance(visual_assistance.cloned());
    let stats = stats.clone();
    tokio::spawn(async move {
        stats.read().await.record_entry(entry);
    });
}

/// Normalize the usage carried by an upstream `payload` of the given
/// dialect and record it. Returns whether anything was recorded.
///
/// Every call site funnels through `translate::normalize_usage`, so the
/// knowledge of where each provider puts its token counts (and what it
/// calls them) lives in exactly one module. A payload with no usage yet -
/// the normal case for every streaming frame before the terminal one - is
/// simply not recorded.
#[allow(clippy::too_many_arguments)] // why: one flat recorder all dialects share
fn record_payload_usage_with_kind(
    stats: &SharedStats,
    provider: &str,
    model: &str,
    transport: &'static str,
    started: Option<std::time::Instant>,
    wire_kind: UpstreamKind,
    payload: &Value,
    visual_assistance: Option<&VisualAssistanceMetadata>,
    log_kind: &str,
) -> bool {
    let Some(usage) = translate::normalize_usage(wire_kind, payload) else {
        return false;
    };
    record_usage_with_kind(
        stats,
        provider,
        model,
        transport,
        started,
        &usage,
        visual_assistance,
        log_kind,
    );
    true
}

#[allow(clippy::too_many_arguments)] // why: same flat recorder contract as above
fn record_payload_usage(
    stats: &SharedStats,
    provider: &str,
    model: &str,
    transport: &'static str,
    started: Option<std::time::Instant>,
    kind: UpstreamKind,
    payload: &Value,
    visual_assistance: Option<&VisualAssistanceMetadata>,
) -> bool {
    record_payload_usage_with_kind(
        stats,
        provider,
        model,
        transport,
        started,
        kind,
        payload,
        visual_assistance,
        "request",
    )
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
    record_failure_with_visual(stats, provider, model, transport, started, error, None);
}

fn record_failure_with_visual(
    stats: &SharedStats,
    provider: &str,
    model: &str,
    transport: &'static str,
    started: Option<std::time::Instant>,
    error: &str,
    visual_assistance: Option<VisualAssistanceMetadata>,
) {
    let latency_ms = started.map(|s| s.elapsed().as_millis() as u64);
    let entry = crate::stats::RequestEntry::error(provider, model, transport, latency_ms, error)
        .with_visual_assistance(visual_assistance);
    let stats = stats.clone();
    tokio::spawn(async move {
        stats.read().await.record_entry(entry);
    });
}

fn record_problem(
    stats: &SharedStats,
    provider: &str,
    model: &str,
    transport: &'static str,
    started: Option<std::time::Instant>,
    kind: &str,
    error: &str,
) {
    let latency_ms = started.map(|s| s.elapsed().as_millis() as u64);
    let entry =
        crate::stats::RequestEntry::problem(provider, model, transport, latency_ms, kind, error);
    let stats = stats.clone();
    tokio::spawn(async move {
        stats.read().await.record_entry(entry);
    });
}

fn visual_attempt_provenance(attempt: &visual::VisionAttempt) -> VisualAttemptProvenance {
    let error = if attempt.error.is_empty() {
        String::new()
    } else if attempt.error.contains("timed out") {
        "provider request timed out".to_string()
    } else if let Some(status) = attempt.status {
        format!("provider returned HTTP {status}")
    } else {
        "visual assistance provider failed".to_string()
    };
    VisualAttemptProvenance {
        model: attempt.model.clone(),
        retryable: attempt.retryable,
        status: attempt.status,
        duration_ms: attempt.duration_ms.min(u64::MAX as u128) as u64,
        error,
    }
}

fn visual_failure_metadata(error: &anyhow::Error) -> Option<VisualAssistanceMetadata> {
    error
        .downcast_ref::<visual::VisualAnalysisFailure>()
        .map(|failure| VisualAssistanceMetadata {
            images: Vec::new(),
            attempts: failure
                .attempts
                .iter()
                .map(visual_attempt_provenance)
                .collect(),
        })
}

/// Codex occasionally calls paths we do not route (compaction, item
/// retrieval, probes). Log them so gaps are visible instead of silent.
/// S7: never log body content - it carries user prompts and source code.
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

/// Resolve `provider/model` (or a bare upstream id in native-slug mode) to
/// (provider, upstream model).
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

/// Map a legacy per-dialect OpenCode provider id to the merged one:
/// `opencode-go-chat`/`-claude`/`-responses` → `opencode-go`, and the Zen
/// equivalents → `opencode-zen`. Only when the merged provider still exists;
/// a provider the user repointed to a URL of their own is left alone.
fn merged_opencode_provider<'a>(
    config: &'a crate::config::AppConfig,
    pid: &'a str,
) -> Option<&'a str> {
    for suffix in ["-chat", "-claude", "-responses"] {
        if let Some(merged) = pid.strip_suffix(suffix) {
            if config.providers.contains_key(merged) {
                return Some(merged);
            }
        }
    }
    None
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
    // The upstream model is already in the body (every dialect carries a
    // `model` key), and on a multi-dialect gateway it decides the auth
    // scheme. A body without one is not a model call: provider default.
    req = apply_provider_auth(req, provider, body.get("model").and_then(Value::as_str));
    req.send()
        .await
        .map_err(|e| upstream_unreachable_error(&url, &e, &format!("provider '{}'", provider.id)))
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
    let upstream = req.send().await.map_err(|e| {
        let message = upstream_unreachable_error(&url, &e, "ChatGPT/OpenAI").to_string();
        (StatusCode::BAD_GATEWAY, message)
    })?;
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    tracing::info!(%url, %status, "compact passthrough");
    Ok(Response::builder()
        .status(status)
        .body(Body::from_stream(upstream.bytes_stream()))
        .unwrap())
}

/// Build label for log diagnostics. Kept in the row text so reports from
/// different installed builds are comparable without a separate schema field.
const BUILD_LABEL: &str = concat!("loom-router/", env!("CARGO_PKG_VERSION"));

/// Codex remote compaction v2 is a normal Responses turn whose input ends in
/// `{"type":"compaction_trigger"}`. Native GPT goes to the ChatGPT backend,
/// which returns the encrypted compaction item; routed providers cannot, so
/// this path asks the routed model for a plain summary and wraps it in the
/// transparent envelope the translator can decode on the next replay.
fn is_remote_compaction_v2(payload: &Value) -> bool {
    payload
        .get("input")
        .and_then(Value::as_array)
        .and_then(|items| items.last())
        .is_some_and(|item| item.get("type").and_then(Value::as_str) == Some("compaction_trigger"))
}

/// Rewrite a compaction request for an upstream that does not speak Codex's
/// private `compaction_trigger` item: drop the trigger and the tool surface,
/// and ask for the handoff summary in plain terms.
fn codex_request_kind(payload: &Value) -> Option<String> {
    payload
        .get("client_metadata")
        .and_then(|m| m.get("x-codex-turn-metadata"))
        .and_then(Value::as_str)
        .and_then(parse_request_kind)
}

fn build_compaction_payload(payload: &Value) -> Value {
    let mut out = payload.clone();
    let Some(items) = out.get_mut("input").and_then(Value::as_array_mut) else {
        return out;
    };
    let mut kept = Vec::with_capacity(items.len() + 1);
    for mut item in items.drain(..) {
        if item.get("type").and_then(Value::as_str) == Some("compaction_trigger") {
            continue;
        }
        if let Some(parts) = item.get_mut("content").and_then(Value::as_array_mut) {
            for part in parts.iter_mut() {
                if part.get("type").and_then(Value::as_str) == Some("input_image") {
                    *part = json!({"type":"input_text","text":"[image omitted for compaction]"});
                }
            }
        }
        kept.push(item);
    }
    kept.push(json!({
        "type": "message",
        "role": "user",
        "content": [{"type":"input_text","text":translate::COMPACTION_PROMPT}],
    }));
    *items = kept;
    if let Some(object) = out.as_object_mut() {
        object.remove("tools");
        object.remove("tool_choice");
        object.remove("parallel_tool_calls");
        object.remove("previous_response_id");
        object.remove("store");
        object["stream"] = Value::Bool(false);
    }
    out
}

fn truncate_head(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        return text.to_string();
    }
    let mut end = max_chars;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[truncated]", &text[..end])
}

fn truncate_tail(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        return text.to_string();
    }
    let mut start = text.len() - max_chars;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    format!("[earlier context truncated]\n\n{}", &text[start..])
}

/// Keep the compaction summarizer under the destination window even when the
/// history is one oversized item that item-level clamping cannot split.
fn fit_compaction_input(provider: &Provider, upstream_model: &str, payload: &Value) -> Value {
    let mut prepared = build_compaction_payload(payload);
    let Some(items) = prepared.get("input").and_then(Value::as_array).cloned() else {
        return prepared;
    };
    let budget = (crate::codex::context_window_for(provider, upstream_model).window as usize)
        .saturating_sub(CONTEXT_RESERVE_TOKENS * 2);
    let estimated = estimate_tokens(&items) + estimate_non_input_tokens(&prepared, &items);
    if estimated <= budget {
        return prepared;
    }

    let (prompt, history) = items.split_last().expect("compaction prompt is appended");
    let mut transcript = render_items_as_text(history);
    if let Some(instructions) = prepared.get("instructions").and_then(Value::as_str) {
        transcript = format!(
            "Instructions:\n{}\n\nConversation:\n{}",
            truncate_head(instructions, (budget * 3 / 4).max(1)),
            transcript
        );
    }
    // The chars/3 estimator is optimistic for real tokenizers (observed
    // oversized compaction payloads around 2.7 bytes/token). Truncating at
    // 2 bytes/token leaves enough headroom for tokenizer and prompt overhead.
    let transcript = truncate_tail(&transcript, (budget * 2).max(1));
    prepared["input"] = json!([
        {"type": "message", "role": "user", "content": [{"type": "input_text", "text": transcript}]},
        prompt,
    ]);
    if let Some(object) = prepared.as_object_mut() {
        object.insert(
            "instructions".into(),
            Value::String("You are a conversation summarizer.".into()),
        );
    }
    prepared
}

/// Run a compaction turn as a plain summarization request to the routed model.
async fn summarize_compaction(
    ctx: &ProxyCtx,
    provider: &Provider,
    upstream_model: &str,
    payload: &Value,
) -> anyhow::Result<(String, Option<Value>)> {
    let prepared = fit_compaction_input(provider, upstream_model, payload);

    if provider.id == crate::providers::CLAUDE_CODE_PROVIDER_ID {
        let (result, _) = run_claude_turn(&prepared, upstream_model, WireApi::Responses).await?;
        let usage = Some(json!({
            "input_tokens": result.input_tokens,
            "output_tokens": result.output_tokens,
            "total_tokens": result.input_tokens + result.output_tokens,
        }));
        return Ok((result.text, usage));
    }

    let (path, body, kind) =
        build_upstream(provider, &prepared, upstream_model, WireApi::Responses)?;
    let upstream = send(ctx, provider, path, &body).await?;
    let status = upstream.status();
    if !status.is_success() {
        let text = upstream.text().await.unwrap_or_default();
        if let Ok(parsed) = serde_json::from_str::<Value>(&text) {
            log_rejected_upstream_request(provider, path, status, &parsed);
        }
        let message = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|value| {
                value
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| text.chars().take(300).collect());
        bail!(
            "provider '{}' returned {} during compaction: {message}",
            provider.id,
            status
        );
    }
    let bytes = upstream.bytes().await?;
    let parsed: Value = serde_json::from_slice(&bytes)?;
    let usage = translate::normalize_usage(kind, &parsed);
    let summary = translate::extract_text(kind, &parsed)
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| anyhow!("provider '{}' returned no compaction summary", provider.id))?;
    Ok((summary, usage))
}

fn compaction_completed_response(payload: &Value, summary: &str, usage: Option<Value>) -> Value {
    let item = json!({
        "type": "compaction",
        "encrypted_content": translate::encode_compaction_summary(summary),
    });
    let created_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let mut response = json!({
        "id": format!("resp_{}", uuid::Uuid::new_v4().simple()),
        "object": "response",
        "created_at": created_at,
        "status": "completed",
        "model": payload.get("model").cloned().unwrap_or(Value::Null),
        "output": [item],
    });
    if let Some(usage) = usage {
        response["usage"] = usage;
    }
    response
}

fn compaction_response_events(payload: &Value, summary: &str, usage: Option<Value>) -> Vec<Value> {
    let response = compaction_completed_response(payload, summary, usage);
    let response_id = response["id"].as_str().unwrap_or_default().to_string();
    let item = response["output"][0].clone();
    vec![
        json!({
            "type": "response.created",
            "sequence_number": 1,
            "response": {"id": response_id, "status": "in_progress", "output": []},
        }),
        json!({
            "type": "response.output_item.added",
            "sequence_number": 2,
            "output_index": 0,
            "item": item,
        }),
        json!({
            "type": "response.output_item.done",
            "sequence_number": 3,
            "output_index": 0,
            "item": response["output"][0].clone(),
        }),
        json!({
            "type": "response.completed",
            "sequence_number": 4,
            "response": response,
        }),
    ]
}

fn compaction_sse_frames(payload: &Value, summary: &str, usage: Option<Value>) -> Vec<Bytes> {
    compaction_response_events(payload, summary, usage)
        .into_iter()
        .map(|event| {
            let event_name = event
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            Bytes::from(frame_with_event(event_name, &event))
        })
        .collect()
}

async fn dispatch_routed_compaction(
    ctx: &ProxyCtx,
    provider: &Provider,
    upstream_model: &str,
    model: &str,
    payload: &Value,
) -> anyhow::Result<Response> {
    let started = std::time::Instant::now();
    let (summary, usage) = match summarize_compaction(ctx, provider, upstream_model, payload).await
    {
        Ok(ok) => ok,
        Err(error) => {
            record_problem(
                &ctx.stats,
                &provider.id,
                model,
                "http",
                Some(started),
                "compaction",
                &format!("{BUILD_LABEL}: {error}"),
            );
            return Err(error);
        }
    };
    if let Some(usage) = &usage {
        record_payload_usage_with_kind(
            &ctx.stats,
            &provider.id,
            model,
            "http",
            Some(started),
            UpstreamKind::Responses,
            &json!({"usage": usage}),
            None,
            "compaction",
        );
    }

    if payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let stream = futures::stream::iter(
            compaction_sse_frames(payload, &summary, usage)
                .into_iter()
                .map(Ok::<_, std::io::Error>),
        );
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .body(Body::from_stream(stream))?);
    }

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(
            compaction_completed_response(payload, &summary, usage).to_string(),
        ))?)
}

async fn routed_compaction_events(
    ctx: &ProxyCtx,
    provider: &Provider,
    upstream_model: &str,
    payload: &Value,
) -> anyhow::Result<WsEvents> {
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(upstream_model);
    let (summary, usage) = match summarize_compaction(ctx, provider, upstream_model, payload).await
    {
        Ok(ok) => ok,
        Err(error) => {
            record_problem(
                &ctx.stats,
                &provider.id,
                model,
                "ws",
                None,
                "compaction",
                &format!("{BUILD_LABEL}: {error}"),
            );
            return Err(error);
        }
    };
    let events = compaction_response_events(payload, &summary, usage);
    Ok(futures::stream::iter(events.into_iter().map(Ok::<_, String>)).boxed())
}

#[derive(Clone, Copy, PartialEq)]
enum WireApi {
    Responses,
    ChatCompletions,
}

impl WireApi {
    /// The translator dialect this wire speaks - the one mapping both routing
    /// paths must agree on, so it lives here instead of being re-matched at
    /// each call site.
    fn downstream(self) -> DownstreamKind {
        match self {
            WireApi::Responses => DownstreamKind::Responses,
            WireApi::ChatCompletions => DownstreamKind::ChatCompletions,
        }
    }
}

#[derive(Debug)]
struct VisualAssistanceFailure(String);

impl fmt::Display for VisualAssistanceFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "visual assistance preparation failed: {}",
            self.0
        )
    }
}

impl std::error::Error for VisualAssistanceFailure {}

struct PayloadImagePart {
    message_index: usize,
    image: ImagePart,
}

/// Extract client-supplied image references without retaining or logging their
/// bytes. Both wire formats keep image URLs inside content arrays.
fn image_parts_in_payload(payload: &Value, wire: WireApi) -> Vec<PayloadImagePart> {
    let messages = match wire {
        WireApi::Responses => payload.get("input").and_then(Value::as_array),
        WireApi::ChatCompletions => payload.get("messages").and_then(Value::as_array),
    };
    messages
        .into_iter()
        .flatten()
        .enumerate()
        .flat_map(|(message_index, message)| {
            message
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(move |part| match wire {
                    WireApi::Responses
                        if part.get("type").and_then(Value::as_str) == Some("input_image") =>
                    {
                        image_part_from_url(part.get("image_url")).map(|image| PayloadImagePart {
                            message_index,
                            image,
                        })
                    }
                    WireApi::ChatCompletions
                        if part.get("type").and_then(Value::as_str) == Some("image_url") =>
                    {
                        image_part_from_url(part.get("image_url")).map(|image| PayloadImagePart {
                            message_index,
                            image,
                        })
                    }
                    _ => None,
                })
        })
        .collect()
}

fn validate_image_part_roles(payload: &Value, wire: WireApi) -> anyhow::Result<()> {
    let messages = match wire {
        WireApi::Responses => payload.get("input").and_then(Value::as_array),
        WireApi::ChatCompletions => payload.get("messages").and_then(Value::as_array),
    };
    let image_type = match wire {
        WireApi::Responses => "input_image",
        WireApi::ChatCompletions => "image_url",
    };
    for message in messages.into_iter().flatten() {
        let has_image = message
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|content| {
                content
                    .iter()
                    .any(|part| part.get("type").and_then(Value::as_str) == Some(image_type))
            });
        if has_image && message.get("role").and_then(Value::as_str) != Some("user") {
            bail!("visual assistance only supports image parts in user messages");
        }
    }
    Ok(())
}

fn image_part_from_url(value: Option<&Value>) -> Option<ImagePart> {
    let url = match value? {
        Value::String(url) => url.clone(),
        Value::Object(_) => value?.get("url")?.as_str()?.to_string(),
        _ => return None,
    };
    let mime_type = url
        .strip_prefix("data:")
        .and_then(|rest| rest.split_once(','))
        .map(|(metadata, _)| metadata.split(';').next().unwrap_or_default())
        .filter(|mime| mime.starts_with("image/"))
        .map(str::to_string);
    Some(ImagePart { url, mime_type })
}

/// Resolve the configured visual capability for the exact routed model.
fn model_supports_vision(config: &AppConfig, slug: &str) -> anyhow::Result<bool> {
    let (provider, model) = resolve(config, slug)?;
    provider
        .models
        .iter()
        .find(|candidate| candidate.id == model)
        .map(|candidate| candidate.supports_vision)
        .ok_or_else(|| anyhow!("configured model '{slug}' is unavailable"))
}

/// Remove image parts from user content and append the already-delimited
/// visual evidence to the user message that supplied each image.
fn enrich_payload_with_evidence(
    payload: &mut Value,
    wire: WireApi,
    evidence_by_message: &[(usize, String)],
) -> anyhow::Result<()> {
    let messages = match wire {
        WireApi::Responses => payload.get_mut("input").and_then(Value::as_array_mut),
        WireApi::ChatCompletions => payload.get_mut("messages").and_then(Value::as_array_mut),
    }
    .ok_or_else(|| anyhow!("payload has no message array for visual evidence"))?;

    let text_type = match wire {
        WireApi::Responses => "input_text",
        WireApi::ChatCompletions => "text",
    };
    let image_type = match wire {
        WireApi::Responses => "input_image",
        WireApi::ChatCompletions => "image_url",
    };

    for (index, message) in messages.iter_mut().enumerate() {
        let is_user = message.get("role").and_then(Value::as_str) == Some("user");
        let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        let has_image = content
            .iter()
            .any(|part| part.get("type").and_then(Value::as_str) == Some(image_type));
        if !has_image {
            continue;
        }
        if !is_user {
            bail!("visual assistance only supports image parts in user messages");
        }
        let block = evidence_by_message
            .iter()
            .find_map(|(message_index, block)| (*message_index == index).then_some(block))
            .ok_or_else(|| anyhow!("payload image has no visual evidence"))?;
        content.retain(|part| part.get("type").and_then(Value::as_str) != Some(image_type));
        content.push(json!({"type": text_type, "text": block}));
    }
    Ok(())
}

/// Run configured visual analysis only when a routed destination cannot
/// receive images natively. Errors deliberately occur before an upstream
/// request or downstream stream exists.
async fn prepare_visual_assistance(
    client: &reqwest::Client,
    config: &AppConfig,
    payload: &mut Value,
    wire: WireApi,
    destination_slug: &str,
) -> anyhow::Result<Option<VisualAssistanceMetadata>> {
    let images = image_parts_in_payload(payload, wire);
    if images.is_empty() || !config.visual_assistance.enabled {
        return Ok(None);
    }
    validate_image_part_roles(payload, wire)?;
    if model_supports_vision(config, destination_slug)? {
        return Ok(None);
    }

    let started = std::time::Instant::now();
    let mut evidence_by_message: Vec<(usize, String)> = Vec::new();
    let mut provenance = Vec::with_capacity(images.len());
    let mut attempts = Vec::new();
    for image in &images {
        let image_started = std::time::Instant::now();
        let outcome = visual::analyze_with_fallbacks(client, config, &image.image, None).await?;
        attempts.extend(outcome.attempts.iter().map(visual_attempt_provenance));
        let block = visual::evidence_block(&outcome.evidence, &outcome.model);
        match evidence_by_message.last_mut() {
            Some((message_index, blocks)) if *message_index == image.message_index => {
                blocks.push_str("\n\n");
                blocks.push_str(&block);
            }
            _ => evidence_by_message.push((image.message_index, block)),
        }
        provenance.push(VisualImageProvenance {
            model: outcome.model,
            attempts: outcome.attempts.len().min(u32::MAX as usize) as u32,
            duration_ms: image_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            cache_hit: outcome.cache_hit,
        });
    }
    enrich_payload_with_evidence(payload, wire, &evidence_by_message)?;

    let metadata = VisualAssistanceMetadata {
        images: provenance,
        attempts,
    };
    tracing::info!(
        visual_assistance_duration_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        visual_assistance_images = ?metadata.images,
        visual_assistance_attempts = ?metadata.attempts,
        "visual analysis completed"
    );
    Ok(Some(metadata))
}

/// Keep visual-assistance diagnostics safe for the persistent request log and
/// UI: provider errors must never reveal request bodies, image URLs, or keys.
fn redacted_visual_assistance_error(error: &anyhow::Error) -> String {
    let message = error.to_string();
    if message.starts_with("visual assistance exhausted configured fallbacks") {
        "visual assistance exhausted configured fallbacks".to_string()
    } else if message.contains("timed out") {
        "visual assistance provider request timed out".to_string()
    } else {
        "visual assistance failed".to_string()
    }
}

/// Shared HTTP/WS visual-preparation failure path. Persist only the redacted
/// summary before returning the same safe diagnostic to the gateway caller.
fn visual_preparation_failure(
    stats: &SharedStats,
    provider: &str,
    model: &str,
    transport: &'static str,
    started: std::time::Instant,
    error: &anyhow::Error,
) -> VisualAssistanceFailure {
    let visual_assistance = visual_failure_metadata(error);
    let error = redacted_visual_assistance_error(error);
    if let Some(metadata) = &visual_assistance {
        tracing::warn!(
            visual_assistance_attempts = ?metadata.attempts,
            "visual analysis failed"
        );
    }
    record_failure_with_visual(
        stats,
        provider,
        model,
        transport,
        Some(started),
        &error,
        visual_assistance,
    );
    VisualAssistanceFailure(error)
}

fn structured_error_response(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({
            "error": {
                "type": "gateway_error",
                "message": message.into(),
            }
        })),
    )
        .into_response()
}

/// Convert a reqwest send failure into a user-facing proxy error. The raw
/// error stays in the logs; Codex and the Logs page only need the actionable
/// part: the proxy could not reach the upstream at all, usually because the
/// machine lost connectivity.
fn upstream_unreachable_error(url: &str, error: &reqwest::Error, label: &str) -> anyhow::Error {
    tracing::warn!(%url, error = %error, "upstream request failed");
    anyhow!("could not reach {label} ({url}). Check your internet connection and try again.")
}

async fn handle_responses(
    AxState(ctx): AxState<ProxyCtx>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, Response> {
    enforce_json_post(&headers)
        .map_err(|(status, message)| structured_error_response(status, message))?;
    let payload = parse_body(&body, "/v1/responses")
        .map_err(|(status, message)| structured_error_response(status, message))?;
    dispatch(ctx, headers, payload, WireApi::Responses)
        .await
        .map_err(|error| structured_error_response(StatusCode::BAD_GATEWAY, error.to_string()))
}

async fn handle_chat_completions(
    AxState(ctx): AxState<ProxyCtx>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, Response> {
    enforce_json_post(&headers)
        .map_err(|(status, message)| structured_error_response(status, message))?;
    let payload = parse_body(&body, "/v1/chat/completions")
        .map_err(|(status, message)| structured_error_response(status, message))?;
    dispatch(ctx, headers, payload, WireApi::ChatCompletions)
        .await
        .map_err(|error| structured_error_response(StatusCode::BAD_GATEWAY, error.to_string()))
}

/// Build the upstream request (path, body, upstream kind) for a routed
/// provider - the single translation pipeline shared by the HTTP `dispatch`
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

fn needs_responses_function_tool_compat(provider: &Provider, model: &str) -> bool {
    provider.id == "opencode-go" && model == "deepseek-v4-flash"
}

/// Summary of a rejected upstream request. It intentionally contains only
/// structural metadata: provider errors must be diagnosable without writing
/// prompts, image-derived evidence, tool arguments, or credentials to logs.
#[derive(Debug, PartialEq, Eq)]
struct UpstreamRequestDiagnostics {
    body_bytes: usize,
    top_level_fields: BTreeSet<String>,
    message_count: usize,
    input_item_types: BTreeMap<String, usize>,
    user_message_count: usize,
    system_message_count: usize,
    assistant_message_count: usize,
    content_part_count: usize,
    tool_count: usize,
    tool_types: BTreeMap<String, usize>,
    nested_tool_types: BTreeMap<String, usize>,
    function_parameter_root_types: BTreeMap<String, usize>,
    matched_function_call_count: usize,
    unmatched_function_call_count: usize,
    unmatched_function_output_count: usize,
    function_output_before_call_count: usize,
    function_call_field_sets: BTreeMap<String, usize>,
    function_output_field_sets: BTreeMap<String, usize>,
    function_output_value_types: BTreeMap<String, usize>,
    reasoning_positions: Vec<usize>,
    reasoning_field_sets: BTreeMap<String, usize>,
    reasoning_content_part_types: BTreeMap<String, usize>,
    reasoning_content_text_bytes: usize,
    reasoning_summary_part_types: BTreeMap<String, usize>,
    reasoning_summary_text_bytes: usize,
    reasoning_encrypted_content_count: usize,
    has_visual_evidence: bool,
    has_reasoning_effort: bool,
    has_response_format: bool,
    stream: bool,
}

fn upstream_request_diagnostics(body: &Value) -> UpstreamRequestDiagnostics {
    let messages = body
        .get("messages")
        .or_else(|| body.get("input"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let role_count = |role| {
        messages
            .iter()
            .filter(|message| message.get("role").and_then(Value::as_str) == Some(role))
            .count()
    };
    let content_part_count = messages
        .iter()
        .map(|message| match message.get("content") {
            Some(Value::Array(parts)) => parts.len(),
            Some(Value::String(_)) => 1,
            _ => 0,
        })
        .sum();
    let tool_count = body
        .get("tools")
        .or_else(|| body.get("functions"))
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let tools = body
        .get("tools")
        .or_else(|| body.get("functions"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut input_item_types = BTreeMap::new();
    for item in messages {
        let kind = item
            .get("type")
            .and_then(Value::as_str)
            .or_else(|| item.get("role").and_then(Value::as_str))
            .unwrap_or("unknown");
        *input_item_types.entry(kind.to_string()).or_insert(0) += 1;
    }
    let mut tool_types = BTreeMap::new();
    let mut nested_tool_types = BTreeMap::new();
    let mut function_parameter_root_types = BTreeMap::new();
    for tool in tools {
        let kind = tool
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        *tool_types.entry(kind.to_string()).or_insert(0) += 1;
        if kind == "namespace" {
            for nested in tool
                .get("tools")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let nested_kind = nested
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                *nested_tool_types
                    .entry(nested_kind.to_string())
                    .or_insert(0) += 1;
            }
        }
        if kind == "function" {
            let parameters = tool.get("parameters").or_else(|| {
                tool.get("function")
                    .and_then(|function| function.get("parameters"))
            });
            let root = parameters
                .and_then(|parameters| parameters.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("missing");
            *function_parameter_root_types
                .entry(root.to_string())
                .or_insert(0) += 1;
        }
    }
    let mut call_positions: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut output_positions: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut calls_without_id = 0;
    let mut outputs_without_id = 0;
    let mut function_call_field_sets = BTreeMap::new();
    let mut function_output_field_sets = BTreeMap::new();
    let mut function_output_value_types = BTreeMap::new();
    let mut reasoning_positions = Vec::new();
    let mut reasoning_field_sets = BTreeMap::new();
    let mut reasoning_content_part_types = BTreeMap::new();
    let mut reasoning_content_text_bytes = 0;
    let mut reasoning_summary_part_types = BTreeMap::new();
    let mut reasoning_summary_text_bytes = 0;
    let mut reasoning_encrypted_content_count = 0;
    for (index, item) in messages.iter().enumerate() {
        let item_type = item.get("type").and_then(Value::as_str);
        let field_set = item
            .as_object()
            .map(|object| object.keys().cloned().collect::<Vec<_>>().join(","))
            .unwrap_or_else(|| "non-object".to_string());
        match item_type {
            Some("function_call") | Some("custom_tool_call") => {
                *function_call_field_sets.entry(field_set).or_insert(0) += 1;
            }
            Some("function_call_output") | Some("custom_tool_call_output") => {
                *function_output_field_sets.entry(field_set).or_insert(0) += 1;
                let value_type = match item.get("output") {
                    Some(Value::String(_)) => "string",
                    Some(Value::Array(_)) => "array",
                    Some(Value::Object(_)) => "object",
                    Some(Value::Null) => "null",
                    Some(Value::Bool(_)) => "boolean",
                    Some(Value::Number(_)) => "number",
                    None => "missing",
                };
                *function_output_value_types
                    .entry(value_type.to_string())
                    .or_insert(0) += 1;
            }
            Some("reasoning") => {
                reasoning_positions.push(index);
                *reasoning_field_sets.entry(field_set).or_insert(0) += 1;
                for part in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let part_type = part
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    *reasoning_content_part_types
                        .entry(part_type.to_string())
                        .or_insert(0) += 1;
                    reasoning_content_text_bytes +=
                        part.get("text").and_then(Value::as_str).map_or(0, str::len);
                }
                for part in item
                    .get("summary")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let part_type = part
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    *reasoning_summary_part_types
                        .entry(part_type.to_string())
                        .or_insert(0) += 1;
                    reasoning_summary_text_bytes +=
                        part.get("text").and_then(Value::as_str).map_or(0, str::len);
                }
                reasoning_encrypted_content_count += item
                    .get("encrypted_content")
                    .and_then(Value::as_str)
                    .filter(|content| !content.is_empty())
                    .is_some() as usize;
            }
            _ => {}
        }
        let target = match item_type {
            Some("function_call") | Some("custom_tool_call") => Some(&mut call_positions),
            Some("function_call_output") | Some("custom_tool_call_output") => {
                Some(&mut output_positions)
            }
            _ => None,
        };
        let Some(target) = target else {
            continue;
        };
        if let Some(call_id) = item.get("call_id").and_then(Value::as_str) {
            target.entry(call_id.to_string()).or_default().push(index);
        } else if matches!(item_type, Some("function_call") | Some("custom_tool_call")) {
            calls_without_id += 1;
        } else {
            outputs_without_id += 1;
        }
    }
    let mut matched_function_call_count = 0;
    let mut unmatched_function_call_count = calls_without_id;
    let mut unmatched_function_output_count = outputs_without_id;
    let mut function_output_before_call_count = 0;
    let all_call_ids: BTreeSet<&String> = call_positions
        .keys()
        .chain(output_positions.keys())
        .collect();
    for call_id in all_call_ids {
        let calls = call_positions
            .get(call_id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let outputs = output_positions
            .get(call_id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let matched = calls.len().min(outputs.len());
        matched_function_call_count += matched;
        unmatched_function_call_count += calls.len().saturating_sub(outputs.len());
        unmatched_function_output_count += outputs.len().saturating_sub(calls.len());
        function_output_before_call_count += calls
            .iter()
            .zip(outputs.iter())
            .take(matched)
            .filter(|(call, output)| output < call)
            .count();
    }

    UpstreamRequestDiagnostics {
        body_bytes: serde_json::to_vec(body).map_or(0, |encoded| encoded.len()),
        top_level_fields: body
            .as_object()
            .map(|object| object.keys().cloned().collect())
            .unwrap_or_default(),
        message_count: messages.len(),
        input_item_types,
        user_message_count: role_count("user"),
        system_message_count: role_count("system"),
        assistant_message_count: role_count("assistant"),
        content_part_count,
        tool_count,
        tool_types,
        nested_tool_types,
        function_parameter_root_types,
        matched_function_call_count,
        unmatched_function_call_count,
        unmatched_function_output_count,
        function_output_before_call_count,
        function_call_field_sets,
        function_output_field_sets,
        function_output_value_types,
        reasoning_positions,
        reasoning_field_sets,
        reasoning_content_part_types,
        reasoning_content_text_bytes,
        reasoning_summary_part_types,
        reasoning_summary_text_bytes,
        reasoning_encrypted_content_count,
        has_visual_evidence: body.to_string().contains("<visual-evidence"),
        has_reasoning_effort: body.get("reasoning_effort").is_some()
            || body.get("reasoning").is_some(),
        has_response_format: body.get("response_format").is_some(),
        stream: body.get("stream").and_then(Value::as_bool).unwrap_or(false),
    }
}

fn log_rejected_upstream_request(
    provider: &Provider,
    path: &str,
    status: StatusCode,
    body: &Value,
) {
    let request = upstream_request_diagnostics(body);
    tracing::warn!(
        provider = %provider.id,
        endpoint = path,
        %status,
        ?request,
        "provider rejected upstream request"
    );
}

// ---------------------------------------------------------------------------
// Side-call routing (background/auxiliary call fallback)
//
// Codex issues auxiliary model calls alongside the user's main turns: inline
// context compaction, startup prewarm probes, and memory consolidation. They
// flow through the regular Responses endpoint and, for native GPT models, are
// forwarded to the ChatGPT backend where they spend the user's quota even
// though they are not user-facing turns.
//
// Detection (evidence: codex-rs `core/src/responses_metadata.rs` in the
// openai/codex repo, plus its HTTP/WS client tests): every model request
// carries `client_metadata["x-codex-turn-metadata"]` - a JSON *string* whose
// `request_kind` field is "turn" (main turns), "prewarm" (connection warmup
// probes), "compaction" (inline compaction), or "memory" (memory
// consolidation). The same JSON is mirrored in the `x-codex-turn-metadata`
// HTTP header (a bounded compatibility projection), which also rides the WS
// upgrade request. A request is a side call only when this marker is present
// and request_kind is something other than "turn".
//
// This is deliberately conservative:
// - False negatives (a side call we miss) fall through to the original
//   destination - the pre-feature behavior. Known miss: thread-title
//   generation runs as a regular "turn" of an internal helper thread and
//   carries no marker we could verify in the open-source codex-rs codebase,
//   so it is NOT rerouted.
// - False positives (rerouting a user's main turn) are unacceptable, so
//   unknown/missing/malformed metadata always means "main turn".
// - Subagent calls (`x-openai-subagent` header: review, collab spawn, ...)
//   are real user-facing work and are deliberately not treated as side calls.
// - Remote compaction (POST /v1/responses/compact) is a native-backend-only
//   endpoint whose response carries OpenAI-encrypted compaction items; it
//   stays on the native passthrough (see handle_compact) because no existing
//   translator can reproduce that envelope for third-party providers.
//
// When the request is a side call and `config.side_call_fallback` names a
// resolvable `provider/model` slug, the call is routed there through the
// normal pipeline (resolve/build_upstream/translate). If the fallback call
// itself fails, the request is retried against its original destination so a
// broken fallback can never break a side call.
// ---------------------------------------------------------------------------

/// Extract `request_kind` from an `x-codex-turn-metadata` JSON string.
fn parse_request_kind(raw: &str) -> Option<String> {
    serde_json::from_str::<Value>(raw)
        .ok()?
        .get("request_kind")?
        .as_str()
        .map(str::to_string)
}

/// Whether the payload is an auxiliary Codex call (compaction / prewarm
/// probe / memory) rather than a user-facing main turn. Conservative: any
/// missing or malformed marker means "main turn".
fn is_side_call(payload: &Value, headers: Option<&HeaderMap>) -> bool {
    // Canonical transport: client_metadata inside the request body (both the
    // HTTP body and WS `response.create` frames carry it).
    if let Some(raw) = payload
        .get("client_metadata")
        .and_then(|m| m.get("x-codex-turn-metadata"))
        .and_then(Value::as_str)
    {
        if let Some(kind) = parse_request_kind(raw) {
            return kind != "turn";
        }
    }
    // Compatibility projection: the same JSON as an HTTP header (also sent on
    // the WS upgrade request).
    if let Some(raw) = headers
        .and_then(|h| h.get("x-codex-turn-metadata"))
        .and_then(|v| v.to_str().ok())
    {
        if let Some(kind) = parse_request_kind(raw) {
            return kind != "turn";
        }
    }
    false
}

/// Routing decision for one request: native passthrough or a resolved
/// provider, plus whether the provider came from the side-call fallback.
enum EffectiveRoute {
    Native,
    Routed {
        provider: Provider,
        upstream_model: String,
        from_fallback: bool,
    },
}

/// Resolve the effective route for a request. Side calls take the configured
/// `side_call_fallback` before the normal resolve, so they never reach the
/// native ChatGPT passthrough. A stale/disabled fallback slug changes
/// nothing.
fn resolve_effective(
    config: &crate::config::AppConfig,
    model: &str,
    payload: &Value,
    headers: Option<&HeaderMap>,
) -> EffectiveRoute {
    if is_side_call(payload, headers) {
        if let Some(slug) = config.side_call_fallback.as_deref() {
            match resolve(config, slug) {
                Ok((p, upstream_model)) => {
                    return EffectiveRoute::Routed {
                        provider: p.clone(),
                        upstream_model,
                        from_fallback: true,
                    }
                }
                Err(e) => {
                    tracing::warn!(slug, error = %e, "side_call_fallback does not resolve; using original destination");
                }
            }
        }
    }
    match resolve(config, model) {
        Ok((p, upstream_model)) => EffectiveRoute::Routed {
            provider: p.clone(),
            upstream_model,
            from_fallback: false,
        },
        Err(_) => EffectiveRoute::Native,
    }
}

async fn dispatch(
    ctx: ProxyCtx,
    headers: HeaderMap,
    payload: Value,
    wire: WireApi,
) -> anyhow::Result<Response> {
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing 'model' field"))?
        .to_string();

    // P1: read the config once per request and clone only the single
    // resolved provider, instead of cloning the whole AppConfig (every
    // provider, model and API key) per request. state.rs exposes
    // Arc<RwLock<AppConfig>>, so an Arc<AppConfig> swap is not possible
    // without touching state.rs; this is the minimal-copy version.
    let route = {
        let cfg = ctx.config.read().await;
        resolve_effective(&cfg, &model, &payload, Some(&headers))
    };
    let EffectiveRoute::Routed {
        provider,
        upstream_model,
        from_fallback,
    } = route
    else {
        // Not an external model: native GPT models are forwarded unchanged
        // to OpenAI's backend with the caller's own ChatGPT credentials, so
        // the native models in the picker keep working through the proxy.
        return forward_native(&ctx, wire, &headers, payload).await;
    };

    let response = dispatch_routed(&ctx, &provider, &upstream_model, &model, &payload, wire).await;
    // A failed fallback (provider down, bad model) must never break a side
    // call: retry against the request's original destination and return that.
    // Visual preparation is different: retrying a different destination could
    // forward the original image after its explicitly configured bridge
    // failed, so it is a terminal gateway error.
    let visual_failure = response
        .as_ref()
        .err()
        .is_some_and(|error| error.downcast_ref::<VisualAssistanceFailure>().is_some());
    let failed = match &response {
        Ok(r) => !r.status().is_success(),
        Err(_) => true,
    };
    if visual_failure || !from_fallback || !failed {
        return response;
    }
    tracing::warn!(
        %model,
        fallback_provider = %provider.id,
        error = %response.as_ref().err().map(ToString::to_string).unwrap_or_default(),
        "side-call fallback failed; retrying original destination"
    );
    let original = {
        let cfg = ctx.config.read().await;
        resolve(&cfg, &model).map(|(p, m)| (p.clone(), m))
    };
    match original {
        Ok((p, upstream_model)) => {
            dispatch_routed(&ctx, &p, &upstream_model, &model, &payload, wire).await
        }
        Err(_) => forward_native(&ctx, wire, &headers, payload).await,
    }
}

/// Run one routed HTTP turn: translate the request, send it upstream, and
/// translate/tap the response (shared by the normal route, the side-call
/// fallback, and the fallback's retry against the original destination).
async fn dispatch_routed(
    ctx: &ProxyCtx,
    provider: &Provider,
    upstream_model: &str,
    model: &str,
    payload: &Value,
    wire: WireApi,
) -> anyhow::Result<Response> {
    if is_remote_compaction_v2(payload) {
        return dispatch_routed_compaction(ctx, provider, upstream_model, model, payload).await;
    }
    if codex_request_kind(payload).as_deref() == Some("compaction") {
        record_problem(
            &ctx.stats,
            &provider.id,
            upstream_model,
            "http",
            None,
            "compaction",
            &format!(
                "{BUILD_LABEL}: Codex sent a compaction call without a compaction_trigger item; treating it as a normal turn"
            ),
        );
    }
    let mut prepared_payload = payload.clone();
    // HTTP turns (including Codex remote compaction) can carry a full
    // transcript in one request. Apply the same routed clamp as WS so they
    // cannot reach a stateless upstream beyond its window.
    if let Some(items) = prepared_payload
        .get("input")
        .and_then(Value::as_array)
        .cloned()
    {
        let fit = clamp_routed_input(ctx, provider, upstream_model, &prepared_payload, items).await;
        prepared_payload["input"] = Value::Array(fit);
        if let Some(object) = prepared_payload.as_object_mut() {
            object.remove("previous_response_id");
        }
    }
    let started = std::time::Instant::now();
    let visual_assistance = if !image_parts_in_payload(&prepared_payload, wire).is_empty() {
        let config = ctx.config.read().await.clone();
        let destination_slug = format!("{}/{}", provider.id, upstream_model);
        match prepare_visual_assistance(
            &ctx.client,
            &config,
            &mut prepared_payload,
            wire,
            &destination_slug,
        )
        .await
        {
            Ok(metadata) => metadata,
            Err(error) => {
                return Err(anyhow::Error::new(visual_preparation_failure(
                    &ctx.stats,
                    &provider.id,
                    model,
                    "http",
                    started,
                    &error,
                )));
            }
        }
    } else {
        None
    };
    let wants_stream = payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // The claude-code backend routes through the local `claude` CLI, which
    // is not wired up yet: the models are listed and published to the picker
    // (Phase 1), but a request would otherwise hit the placeholder base_url
    // and return a meaningless 502.
    if provider.id == crate::providers::CLAUDE_CODE_PROVIDER_ID {
        return dispatch_claude_cli(ctx, provider, upstream_model, model, payload, wire).await;
    }

    tracing::info!(%model, provider = %provider.id, %upstream_model, stream = wants_stream, "routing request");
    let (path, body, upstream_kind) =
        build_upstream(provider, &prepared_payload, upstream_model, wire)?;

    let upstream = send(ctx, provider, path, &body).await?;
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);

    // Upstream error: pass the body through untouched and record the failure.
    if !status.is_success() {
        log_rejected_upstream_request(provider, path, status, &body);
        record_failure(
            &ctx.stats,
            &provider.id,
            model,
            "http",
            Some(started),
            &format!("upstream returned {status}"),
        );
        return Ok(Response::builder()
            .status(status)
            .body(Body::from_stream(upstream.bytes_stream()))?);
    }

    if needs_responses_function_tool_compat(provider, upstream_model) {
        tracing::info!(
            provider = %provider.id,
            endpoint = path,
            %status,
            request = ?upstream_request_diagnostics(&body),
            "provider accepted upstream request"
        );
    }

    // Same-format pass-through: the payload needs no translation, but usage
    // still has to be recorded.
    //
    // This branch used to return before any tap ran, so every request from
    // an OpenAI-compatible client to an OpenAI-compatible provider was
    // missing from the dashboard - the single largest gap in the stats,
    // since it is the one path that never reaches the translator. Codex was
    // unaffected (it speaks Responses), which is why it went unnoticed.
    let same_format = matches!(
        (upstream_kind, wire),
        (UpstreamKind::OpenAiChat, WireApi::ChatCompletions)
    );
    if same_format {
        if wants_stream {
            return Ok(Response::builder()
                .status(status)
                .header("content-type", "text/event-stream")
                .header("cache-control", "no-cache")
                .body(Body::from_stream(tap_usage_stream(
                    upstream,
                    upstream_kind,
                    ctx.stats.clone(),
                    provider.id.clone(),
                    model.to_string(),
                    started,
                    visual_assistance.clone(),
                )))?);
        }
        // Keep the upstream bytes verbatim: parsing for usage must not
        // reorder or reshape a response we promised to pass through.
        let raw = upstream.bytes().await?;
        match serde_json::from_slice::<Value>(&raw) {
            Ok(parsed) => {
                record_payload_usage(
                    &ctx.stats,
                    &provider.id,
                    model,
                    "http",
                    Some(started),
                    upstream_kind,
                    &parsed,
                    visual_assistance.as_ref(),
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, len = raw.len(), "pass-through body was not JSON; usage not recorded")
            }
        }
        return Ok(Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(Body::from(raw))?);
    }

    // Responses-native upstream: the downstream wire is already Responses,
    // so pass bytes through and only tap usage for stats/logs.
    if upstream_kind == UpstreamKind::Responses {
        if wants_stream {
            if needs_responses_function_tool_compat(provider, upstream_model) {
                return Ok(Response::builder()
                    .status(status)
                    .header("content-type", "text/event-stream")
                    .header("cache-control", "no-cache")
                    .body(Body::from_stream(translate_byte_stream(
                        upstream.bytes_stream().boxed(),
                        upstream_kind,
                        DownstreamKind::Responses,
                        model,
                        translate::tool_namespace_map(&prepared_payload),
                        translate::freeform_tool_names(&prepared_payload),
                        Some((
                            ctx.stats.clone(),
                            provider.id.clone(),
                            model.to_string(),
                            started,
                            visual_assistance.clone(),
                        )),
                    )))?);
            }
            return Ok(Response::builder()
                .status(status)
                .header("content-type", "text/event-stream")
                .header("cache-control", "no-cache")
                .body(Body::from_stream(tap_usage_stream(
                    upstream,
                    upstream_kind,
                    ctx.stats.clone(),
                    provider.id.clone(),
                    model.to_string(),
                    started,
                    visual_assistance.clone(),
                )))?);
        }
        let json: Value = upstream.json().await?;
        record_payload_usage(
            &ctx.stats,
            &provider.id,
            model,
            "http",
            Some(started),
            upstream_kind,
            &json,
            visual_assistance.as_ref(),
        );
        let json = if needs_responses_function_tool_compat(provider, upstream_model) {
            translate_json(
                upstream_kind,
                DownstreamKind::Responses,
                &json,
                model,
                &prepared_payload,
            )
        } else {
            json
        };
        return Ok(Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(Body::from(json.to_string()))?);
    }

    let downstream_kind = wire.downstream();

    if wants_stream {
        Ok(Response::builder()
            .status(status)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .body(Body::from_stream(translate_byte_stream(
                upstream.bytes_stream().boxed(),
                upstream_kind,
                downstream_kind,
                model,
                translate::tool_namespace_map(&prepared_payload),
                translate::freeform_tool_names(&prepared_payload),
                Some((
                    ctx.stats.clone(),
                    provider.id.clone(),
                    model.to_string(),
                    started,
                    visual_assistance.clone(),
                )),
            )))?)
    } else {
        let json: Value = upstream.json().await?;
        // Record from the upstream payload, before translation: when the
        // downstream wire is Chat Completions the translated usage is back in
        // chat shape, which the canonical recorder would discard.
        record_payload_usage(
            &ctx.stats,
            &provider.id,
            model,
            "http",
            Some(started),
            upstream_kind,
            &json,
            visual_assistance.as_ref(),
        );
        let translated = translate_json(upstream_kind, downstream_kind, &json, model, payload);
        Ok(Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(Body::from(translated.to_string()))?)
    }
}

/// Translate one non-stream upstream JSON response body to the downstream
/// wire. The Responses shape gets its namespaced/freeform tool names restored
/// from the original request, which the raw translators intentionally leave
/// untouched so tool calls round-trip cleanly.
fn translate_json(
    upstream_kind: UpstreamKind,
    downstream_kind: DownstreamKind,
    json: &Value,
    model: &str,
    payload: &Value,
) -> Value {
    match (upstream_kind, downstream_kind) {
        (UpstreamKind::OpenAiChat, DownstreamKind::Responses) => {
            let mut resp = translate::chat_completion_to_responses(json, model);
            if let Some(output) = resp.get_mut("output").and_then(Value::as_array_mut) {
                translate::apply_namespaces_to_output(
                    output,
                    &translate::tool_namespace_map(payload),
                );
                translate::unwrap_freeform_to_output(
                    output,
                    &translate::freeform_tool_names(payload),
                );
            }
            resp
        }
        (UpstreamKind::Anthropic, DownstreamKind::Responses) => {
            let mut resp = translate::anthropic_to_responses(json, model);
            if let Some(output) = resp.get_mut("output").and_then(Value::as_array_mut) {
                translate::apply_namespaces_to_output(
                    output,
                    &translate::tool_namespace_map(payload),
                );
                translate::unwrap_freeform_to_output(
                    output,
                    &translate::freeform_tool_names(payload),
                );
            }
            resp
        }
        (UpstreamKind::Anthropic, DownstreamKind::ChatCompletions) => {
            translate::anthropic_to_chat(json, model)
        }
        (UpstreamKind::Responses, DownstreamKind::Responses) => {
            let mut resp = json.clone();
            if let Some(output) = resp.get_mut("output").and_then(Value::as_array_mut) {
                translate::unwrap_freeform_to_output(
                    output,
                    &translate::freeform_tool_names(payload),
                );
            }
            resp
        }
        _ => json.clone(),
    }
}

/// Bridge one request of either wire to a single `claude -p` turn: flatten
/// to a chat transcript, render the prompt, run the CLI, and mint the
/// synthetic message id. Shared by the HTTP and WS bridges so the pipeline
/// shape lives in exactly one place.
async fn run_claude_turn(
    payload: &Value,
    upstream_model: &str,
    wire: WireApi,
) -> anyhow::Result<(crate::claude_cli::ClaudePrintResult, String)> {
    let messages = match wire {
        WireApi::Responses => {
            let chat = translate::responses_to_chat(payload, upstream_model, false)?;
            chat.get("messages")
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new()))
        }
        WireApi::ChatCompletions => payload
            .get("messages")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new())),
    };
    let prompt = crate::claude_cli::render_prompt(
        messages.as_array().map(Vec::as_slice).unwrap_or_default(),
    );
    let result = crate::claude_cli::run_print_turn(&prompt, upstream_model, None).await?;
    let id = format!(
        "msg_cli_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );
    Ok((result, id))
}

/// Bridge a routed HTTP turn to the local `claude` CLI (claude-code provider).
///
/// The subscription provider has no API endpoint: its models are served by
/// the user's own `claude -p` binary with their existing login. The request
/// is rendered to a text prompt, run through the CLI, and the answer is
/// synthesized in Anthropic's wire shape - which the rest of the pipeline
/// (translation, tap, stats) already consumes unchanged.
async fn dispatch_claude_cli(
    ctx: &ProxyCtx,
    provider: &Provider,
    upstream_model: &str,
    model: &str,
    payload: &Value,
    wire: WireApi,
) -> anyhow::Result<Response> {
    let wants_stream = payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let started = std::time::Instant::now();
    let downstream_kind = wire.downstream();

    // Any wire -> flat chat transcript -> one text prompt for `claude -p`.
    let (result, id) = run_claude_turn(payload, upstream_model, wire).await?;
    tracing::debug!(%model, input_tokens = result.input_tokens, output_tokens = result.output_tokens, "claude -p turn finished");

    if wants_stream {
        let frames = crate::claude_cli::anthropic_sse_stream(
            &id,
            upstream_model,
            &result.text,
            result.input_tokens,
            result.output_tokens,
        );
        let bytes = futures::stream::iter(
            frames
                .into_iter()
                .map(Bytes::from)
                .map(Ok::<_, reqwest::Error>),
        )
        .boxed();
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .body(Body::from_stream(translate_byte_stream(
                bytes,
                UpstreamKind::Anthropic,
                downstream_kind,
                model,
                translate::tool_namespace_map(payload),
                translate::freeform_tool_names(payload),
                Some((
                    ctx.stats.clone(),
                    provider.id.clone(),
                    model.to_string(),
                    started,
                    None,
                )),
            )))?);
    }

    // Non-stream: record usage from the Anthropic shape, then translate.
    let anthropic = crate::claude_cli::anthropic_json_response(
        &id,
        upstream_model,
        &result.text,
        result.input_tokens,
        result.output_tokens,
    );
    record_payload_usage(
        &ctx.stats,
        &provider.id,
        model,
        "http",
        Some(started),
        UpstreamKind::Anthropic,
        &anthropic,
        None,
    );
    let translated = translate_json(
        UpstreamKind::Anthropic,
        downstream_kind,
        &anthropic,
        model,
        payload,
    );
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(translated.to_string()))?)
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
    mut payload: Value,
) -> anyhow::Result<Response> {
    sanitize_responses_payload(&mut payload);
    let upstream = native_send(ctx, wire, headers, &payload).await?;
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    tracing::info!(%status, "native passthrough");
    Ok(Response::builder()
        .status(status)
        .body(Body::from_stream(upstream.bytes_stream()))?)
}

fn sanitize_responses_payload(payload: &mut Value) {
    // Codex marks an internal generation mode that this Responses upstream
    // does not expose. It does not change the prompt, model, or tool shape.
    if let Some(object) = payload.as_object_mut() {
        object.remove("generate");
    }
}

/// Routed Responses providers receive the complete conversation on every
/// turn. Item ids are storage references, not pairing keys; replaying them to
/// a stateless gateway can make a valid `function_call_output` look like a
/// reference to an item it never stored. `call_id` remains untouched and is
/// the portable link between each call and its output.
fn ensure_reasoning_text(item: &mut serde_json::Map<String, Value>) {
    let has_reasoning_text = item
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|parts| {
            parts
                .iter()
                .any(|part| part.get("type").and_then(Value::as_str) == Some("reasoning_text"))
        });
    if has_reasoning_text {
        return;
    }
    let text = item
        .get("summary")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    if !text.is_empty() {
        item.insert(
            "content".into(),
            json!([{"type": "reasoning_text", "text": text}]),
        );
    }
}

fn sanitize_stateless_responses_payload(payload: &mut Value) {
    sanitize_responses_payload(payload);
    if matches!(payload.get("input"), Some(Value::Array(items)) if items.is_empty()) {
        payload["input"] = Value::String(String::new());
        return;
    }
    let Some(input) = payload.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    for item in input.iter_mut() {
        if let Some(object) = item.as_object_mut() {
            object.remove("id");
            object.remove("internal_chat_message_metadata_passthrough");
            if object.get("type").and_then(Value::as_str) == Some("reasoning") {
                ensure_reasoning_text(object);
            }
            if matches!(
                object.get("type").and_then(Value::as_str),
                Some("function_call_output") | Some("custom_tool_call_output")
            ) {
                if let Some(Value::Array(parts)) = object.get("output") {
                    let text = parts
                        .iter()
                        .map(|part| {
                            part.as_str()
                                .map(str::to_string)
                                .or_else(|| {
                                    part.get("text").and_then(Value::as_str).map(str::to_string)
                                })
                                .unwrap_or_else(|| part.to_string())
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    object.insert("output".into(), Value::String(text));
                }
            }
        }
    }

    // Console Go interprets an output between two calls as the end of the
    // thinking turn and then requires a second reasoning_text for the later
    // call. Codex can interleave parallel results and non-user context messages,
    // so stably group calls first, outputs second, and those messages last.
    let is_tool_exchange_item = |item: &Value| {
        matches!(
            item.get("type").and_then(Value::as_str),
            Some("function_call")
                | Some("custom_tool_call")
                | Some("function_call_output")
                | Some("custom_tool_call_output")
        )
    };
    let is_tool_exchange_block_item = |item: &Value| {
        is_tool_exchange_item(item)
            || (item.get("type").and_then(Value::as_str) == Some("message")
                && item
                    .get("role")
                    .and_then(Value::as_str)
                    .is_some_and(|role| role != "user"))
    };
    let mut start = 0;
    while start < input.len() {
        if !is_tool_exchange_item(&input[start]) {
            start += 1;
            continue;
        }
        let mut end = start + 1;
        while end < input.len() && is_tool_exchange_block_item(&input[end]) {
            end += 1;
        }
        input[start..end].sort_by_key(|item| match item.get("type").and_then(Value::as_str) {
            Some("function_call") | Some("custom_tool_call") => 0,
            Some("function_call_output") | Some("custom_tool_call_output") => 1,
            _ => 2,
        });
        start = end;
    }
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

    // The thread may have passed through a routed model, whose reply the
    // translator had to give invented item ids. The native backend resolves
    // ids it issued itself and 404s the rest, so they come out here.
    let mut payload = payload.clone();
    let scrubbed = translate::compaction_items_for_native(&mut payload);
    if scrubbed > 0 {
        tracing::info!(
            scrubbed,
            "converted routed compaction summaries to plain input"
        );
    }
    let stripped = translate::strip_synthetic_ids(&mut payload);
    if stripped > 0 {
        tracing::info!(stripped, "dropped item ids the native backend never issued");
    }

    let mut req = ctx.client.post(&url).json(&payload);
    for name in NATIVE_FORWARD_HEADERS {
        if let Some(value) = headers.get(*name) {
            if let Ok(v) = value.to_str() {
                req = req.header(*name, v);
            }
        }
    }
    let res = req
        .send()
        .await
        .map_err(|e| upstream_unreachable_error(&url, &e, "ChatGPT/OpenAI"))?;
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
// cache each routed turn's full item list and rebuild the complete input
// before forwarding. The cache is shared across WebSocket connections: a
// Codex reconnect starts a new session but resumes the same thread, and
// losing the cache there would reset the conversation to zero.
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

/// P5: shared history bounds. Routed providers are stateless, so every
/// incremental turn replays the full item list, and each cached entry holds
/// the whole conversation so far. A follow-up turn's rebuilt input contains
/// everything the previous turn's entry held, so each insert replaces the
/// entry it was built on - one entry per conversation, never O(n²) growth.
/// The cache is shared across connections (a reconnect keeps the thread
/// alive) and capped by entry count and total serialized size, evicting the
/// oldest first. The entry just stored is never evicted: it is what the next
/// turn's `previous_response_id` resolves against, and dropping it would
/// reset the conversation to a delta-only turn. A single entry may therefore
/// exceed the byte budget - a long conversation keeps its newest turn even
/// when that one entry alone is larger than the cap.
///
/// The byte budget is large on purpose: each live conversation contributes
/// exactly one entry (the rebuild input of its newest turn, which is a full
/// transcription of the conversation up to that point). At ~304k tokens that
/// serializes to ~1.2MB, so several long conversations must coexist without
/// evicting each other - a small cap turns a second long conversation into a
/// context reset for the first.
const WS_HISTORY_MAX_ENTRIES: usize = 100;
const WS_HISTORY_MAX_BYTES: usize = 16 * 1024 * 1024;

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

    /// Record a completed turn under `rid`. `prev` is the response id this
    /// turn was rebuilt from (when it was a follow-up): its entry is fully
    /// contained in the new one, so it is dropped to keep exactly one entry
    /// per conversation.
    fn insert(&mut self, rid: String, record: Vec<Value>, prev: Option<&str>) {
        if let Some(p) = prev {
            self.remove(p);
        }
        if self.map.contains_key(&rid) {
            return;
        }
        let size = record.iter().map(|v| v.to_string().len()).sum::<usize>();
        self.map.insert(rid.clone(), record);
        self.order.push_back((rid, size));
        self.total_bytes += size;
        // Never evict the entry just stored (it is the next turn's
        // `previous_response_id`), even when it alone exceeds the byte cap.
        while (self.order.len() > WS_HISTORY_MAX_ENTRIES || self.total_bytes > WS_HISTORY_MAX_BYTES)
            && self.order.len() > 1
        {
            let Some((old_id, old_size)) = self.order.pop_front() else {
                break;
            };
            if self.map.remove(&old_id).is_some() {
                self.total_bytes = self.total_bytes.saturating_sub(old_size);
            }
        }
    }

    fn remove(&mut self, id: &str) {
        if let Some(pos) = self.order.iter().position(|(rid, _)| rid == id) {
            if let Some((_, size)) = self.order.remove(pos) {
                self.total_bytes = self.total_bytes.saturating_sub(size);
            }
        }
        self.map.remove(id);
    }
}

/// Estimate the token count of a rebuilt Responses input item list. The proxy
/// has no tokenizer; chars/3 is deliberately conservative because code and
/// tool output typically tokenize heavier than English prose. The estimate
/// counts the serialized JSON envelope, not just message text.
fn estimate_tokens(items: &[Value]) -> usize {
    items.iter().map(|v| v.to_string().len() / 3).sum()
}

/// Same heuristic for the parts of a Responses payload outside the input
/// items: instructions, tool definitions, structured fields and delimiters.
/// Codex's own context accounting does not always include these, so the proxy
/// subtracts them from the budget before deciding what fits.
fn estimate_non_input_tokens(payload: &Value, items: &[Value]) -> usize {
    (payload.to_string().len() / 3).saturating_sub(estimate_tokens(items))
}

/// How many tokens to keep free for the destination model's reply when
/// clamping a routed turn, mirroring opencode's COMPACTION_BUFFER (20k).
/// Without the reserve, a full-window input is rejected before the model
/// can answer.
const CONTEXT_RESERVE_TOKENS: usize = 20_000;

/// Injected at the front of a clamped turn so the destination model does not
/// mistake the surviving tail for the whole conversation. Kept minimal: the
/// point is honesty, not an accurate resume (that is the anchored summary in
/// the side-call fallback path).
fn truncation_marker() -> Value {
    serde_json::json!({
        "role": "system",
        "content": [{
            "type": "input_text",
            "text": "The beginning of this conversation exceeded the model's context window and was removed. Only the most recent turns remain."
        }],
    })
}

/// Cut a conversation down to fit the destination model's window, dropping
/// the OLDEST items and never touching the recent tail (the model needs it
/// to answer). Returns the surviving items and the dropped ones (empty when
/// nothing was removed).
#[cfg(test)]
fn clamp_to_window(items: Vec<Value>, window_tokens: i64) -> (Vec<Value>, Vec<Value>) {
    clamp_to_window_with_overhead(items, window_tokens, 0)
}

/// Clamp using the same heuristic while reserving room for the non-input
/// fields (instructions/tools) that the upstream will tokenize too.
fn clamp_to_window_with_overhead(
    items: Vec<Value>,
    window_tokens: i64,
    non_input_tokens: usize,
) -> (Vec<Value>, Vec<Value>) {
    let usable = (window_tokens as usize)
        .saturating_sub(CONTEXT_RESERVE_TOKENS)
        .saturating_sub(non_input_tokens);
    if estimate_tokens(&items) <= usable {
        return (items, Vec::new());
    }
    // Drop from the front until the serialized estimate fits. The tail is
    // never cut: the newest turns carry the actual question. `keep_from` is
    // the first surviving index; it advances while the surviving slice still
    // overflows and there is at least one newer item left to keep.
    let mut keep_from = 0;
    while keep_from + 1 < items.len() && estimate_tokens(&items[keep_from..]) > usable {
        keep_from += 1;
    }
    (items[keep_from..].to_vec(), items[..keep_from].to_vec())
}

/// Flatten Responses-wire input items (role + content blocks) to plain text.
/// `render_prompt` cannot be used directly: it reads string `content`, while
/// these items carry `content` as an array of blocks.
fn render_items_as_text(items: &[Value]) -> String {
    let mut out = String::new();
    for item in items {
        let item_type = item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message");
        let mut role = item.get("role").and_then(Value::as_str).unwrap_or("user");
        let mut text = String::new();
        if let Some(parts) = item.get("content").and_then(Value::as_array) {
            for part in parts {
                if let Some(t) = part.get("text").and_then(Value::as_str) {
                    text.push_str(t);
                    text.push('\n');
                }
                if let Some(t) = part.get("encrypted_content").and_then(Value::as_str) {
                    text.push_str(t);
                    text.push('\n');
                }
            }
        }
        if item_type == "reasoning" {
            for part in item
                .get("summary")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(t) = part.get("text").and_then(Value::as_str) {
                    text.push_str(t);
                    text.push('\n');
                }
            }
        }
        if matches!(
            item_type,
            "function_call" | "custom_tool_call" | "tool_search_call"
        ) {
            let name = item.get("name").and_then(Value::as_str).unwrap_or("");
            let args = item
                .get("arguments")
                .and_then(Value::as_str)
                .or_else(|| item.get("input").and_then(Value::as_str))
                .unwrap_or("");
            text = format!("{name}: {args}");
        }
        if matches!(
            item_type,
            "function_call_output" | "custom_tool_call_output" | "tool_search_output"
        ) {
            role = "tool";
            text = match item.get("output") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(parts)) => parts
                    .iter()
                    .map(|part| {
                        part.as_str()
                            .map(str::to_string)
                            .or_else(|| {
                                part.get("text").and_then(Value::as_str).map(str::to_string)
                            })
                            .unwrap_or_else(|| part.to_string())
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            };
        }
        if !text.is_empty() {
            out.push_str(&format!("{role}: {}\n\n", text.trim()));
        }
    }
    out
}

/// Generate an anchored summary of the dropped turns via the
/// `side_call_fallback` provider, so the destination model keeps a compact
/// memory of what the clamp removed. Returns a system item with the summary,
/// or `None` when no fallback is configured, the call fails, or it exceeds
/// the timeout - the caller then degrades to the plain truncation marker.
///
/// Mirrors opencode's anchored-summary compaction (summary + recent tail)
/// instead of dropping history silently: the model learns the objective and
/// state of the truncated portion without paying for the full transcript.
async fn summarize_dropped_turns(
    ctx: &ProxyCtx,
    config: &AppConfig,
    dropped: &[Value],
) -> Option<Value> {
    let slug = config.side_call_fallback.as_deref()?;
    let (provider, upstream_model) = resolve(config, slug).ok()?;
    let transcript = render_items_as_text(dropped);
    let prompt = format!(
        "The conversation below was truncated because it exceeded the model's context window. \
         Write a concise anchored summary capturing: the objective, important details and decisions, \
         current work state, and the next move. The model that continues the conversation did not see \
         this transcript, so the summary must stand on its own.\n\n{transcript}"
    );
    let summary_payload = serde_json::json!({
        "model": slug,
        "input": [
            {"role": "system", "content": [{"type": "input_text", "text": "You are a conversation summarizer."}]},
            {"role": "user", "content": [{"type": "input_text", "text": prompt}]},
        ],
        "stream": false,
    });
    let text = if provider.id == crate::providers::CLAUDE_CODE_PROVIDER_ID {
        let (result, _) = run_claude_turn(&summary_payload, &upstream_model, WireApi::Responses)
            .await
            .ok()?;
        result.text
    } else {
        let (path, body, kind) = build_upstream(
            provider,
            &summary_payload,
            &upstream_model,
            WireApi::Responses,
        )
        .ok()?;
        let resp = send(ctx, provider, path, &body).await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let bytes = resp.bytes().await.ok()?;
        let parsed: Value = serde_json::from_slice(&bytes).ok()?;
        translate::extract_text(kind, &parsed)?
    };
    if text.trim().is_empty() {
        return None;
    }
    Some(serde_json::json!({
        "role": "system",
        "content": [{
            "type": "input_text",
            "text": format!(
                "Summary of the earlier conversation (the full transcript was truncated to fit the context window):\n{text}"
            ),
        }],
    }))
}

/// Clamp a routed conversation to the destination model's window and prepend
/// a resume marker when anything was dropped. Shared by the WS and HTTP paths
/// so a Codex side call cannot bypass the proxy's safety net.
async fn clamp_routed_input(
    ctx: &ProxyCtx,
    provider: &Provider,
    upstream_model: &str,
    payload: &Value,
    items: Vec<Value>,
) -> Vec<Value> {
    let window = crate::codex::context_window_for(provider, upstream_model).window;
    let non_input_tokens = estimate_non_input_tokens(payload, &items);
    let (mut fit, dropped) = clamp_to_window_with_overhead(items, window, non_input_tokens);
    if dropped.is_empty() {
        return fit;
    }
    tracing::warn!(
        provider = %provider.id,
        %upstream_model,
        window,
        items = fit.len(),
        dropped = dropped.len(),
        "conversation exceeded destination window; clamped the oldest turns"
    );
    let cfg = ctx.config.read().await.clone();
    let marker = match tokio::time::timeout(
        std::time::Duration::from_secs(45),
        summarize_dropped_turns(ctx, &cfg, &dropped),
    )
    .await
    {
        Ok(Some(summary)) => {
            tracing::info!(
                provider = %provider.id,
                %upstream_model,
                "side-call fallback produced an anchored summary for the clamped turns"
            );
            summary
        }
        Ok(None) | Err(_) => truncation_marker(),
    };
    fit.insert(0, marker);
    fit
}

/// Rebuild the full input for an incremental follow-up turn. Codex sends
/// `previous_response_id` + only the new items; the cached full list from
/// that response id is the conversation so far. A missing id (a fresh
/// conversation, or a cached entry already evicted) degrades to the delta
/// alone, matching the pre-cache behavior.
fn rebuild_input(history: &WsHistory, prev: Option<&str>, delta: Vec<Value>) -> Vec<Value> {
    match prev.and_then(|id| history.get(id)) {
        Some(base) => {
            let mut v = base.clone();
            v.extend(delta);
            v
        }
        None => delta,
    }
}

/// Responses sent to the native upstream cannot carry Codex's continuation
/// handle. The proxy has already rebuilt that handle into a complete input,
/// so forward the portable form instead of a parameter this backend rejects.
fn replace_incremental_input(payload: &mut Value, input: Vec<Value>) {
    payload["input"] = Value::Array(input);
    if let Some(object) = payload.as_object_mut() {
        object.remove("previous_response_id");
    }
}

async fn ws_session(socket: WebSocket, ctx: ProxyCtx, headers: HeaderMap) {
    let (mut tx, mut rx) = socket.split();

    while let Some(msg) = rx.next().await {
        let Ok(msg) = msg else { break };
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };
        let Ok(mut payload) = serde_json::from_str::<Value>(&text) else {
            // S7: never log frame content - it carries user prompts.
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
            match resolve_effective(&cfg, &model, &payload, Some(&headers)) {
                EffectiveRoute::Routed {
                    provider,
                    upstream_model,
                    ..
                } => Some((provider, upstream_model)),
                EffectiveRoute::Native => None,
            }
        };

        // Rebuild the full conversation for incremental turns. Both routes do
        // this: the routed path needs the assembled input because the upstream
        // is stateless, and the native path must keep the cache populated too,
        // or a mid-conversation switch to a routed model would resolve
        // `previous_response_id` against nothing and the routed model would
        // see only the delta (a conversation reset to zero).
        let prev = payload
            .get("previous_response_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let delta = payload
            .get("input")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        // The cache is shared across connections, so a Codex reconnect starts
        // a new WS session but keeps the thread's history - otherwise the
        // rebuild degrades to delta-only and the context window resets to zero.
        let items = {
            let history = ctx.history.lock().unwrap_or_else(|e| e.into_inner());
            rebuild_input(&history, prev.as_deref(), delta)
        };
        replace_incremental_input(&mut payload, items.clone());
        let mut full_input_items: Option<Vec<Value>> = Some(items.clone());
        if let Some((provider, upstream_model)) = &routed {
            let fit = clamp_routed_input(&ctx, provider, upstream_model, &payload, items).await;
            replace_incremental_input(&mut payload, fit.clone());
            full_input_items = Some(fit);
        }

        let turn_start = std::time::Instant::now();

        // Do this after history reconstruction so a text-only destination
        // never receives an image replayed from an earlier turn. Keeping the
        // enriched input below also prevents later turns from re-analyzing
        // already-consumed images.
        let mut visual_assistance = None;
        if let Some((provider, upstream_model)) = &routed {
            if !image_parts_in_payload(&payload, WireApi::Responses).is_empty() {
                let config = ctx.config.read().await.clone();
                let destination_slug = format!("{}/{}", provider.id, upstream_model);
                match prepare_visual_assistance(
                    &ctx.client,
                    &config,
                    &mut payload,
                    WireApi::Responses,
                    &destination_slug,
                )
                .await
                {
                    Ok(metadata) => visual_assistance = metadata,
                    Err(error) => {
                        let error = visual_preparation_failure(
                            &ctx.stats,
                            &provider.id,
                            &model,
                            "ws",
                            turn_start,
                            &error,
                        );
                        let _ = tx
                            .send(Message::Text(
                                ws_error_frame(502, &error.to_string()).to_string().into(),
                            ))
                            .await;
                        continue;
                    }
                }
                full_input_items = payload.get("input").and_then(Value::as_array).cloned();
            }
        }

        let mut output_items: Vec<Value> = Vec::new();
        let mut completed_response_id: Option<String> = None;

        match ws_turn_events(&ctx, &headers, payload).await {
            Ok((mut events, final_provider)) => {
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
                                // Canonical Responses frames on this transport.
                                record_payload_usage(
                                    &ctx.stats,
                                    &final_provider,
                                    &model,
                                    "ws",
                                    Some(turn_start),
                                    UpstreamKind::Responses,
                                    v,
                                    visual_assistance.as_ref(),
                                );
                            }
                            _ => {}
                        }
                    }
                    let done =
                        frame.get("type").and_then(Value::as_str) == Some("response.completed");
                    if tx
                        .send(Message::Text(frame.to_string().into()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    if done {
                        break;
                    }
                }
            }
            Err((e, final_provider)) => {
                record_failure(
                    &ctx.stats,
                    &final_provider,
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
            // The compaction trigger is a one-turn instruction, not history:
            // keeping it would leak into every later rebuilt input.
            record.retain(|item| {
                item.get("type").and_then(Value::as_str) != Some("compaction_trigger")
            });
            record.extend(output_items);
            ctx.history
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(rid, record, prev.as_deref());
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
type WsEvents = futures::stream::BoxStream<'static, Result<Value, String>>;
type LabeledWsEvents = Result<(WsEvents, String), (anyhow::Error, String)>;

fn label_ws_events(result: anyhow::Result<WsEvents>, provider_id: String) -> LabeledWsEvents {
    result
        .map(|events| (events, provider_id.clone()))
        .map_err(|error| (error, provider_id))
}

async fn ws_turn_events(ctx: &ProxyCtx, headers: &HeaderMap, payload: Value) -> LabeledWsEvents {
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| (anyhow!("missing 'model' field"), "codex-native".to_string()))?
        .to_string();

    let route = {
        let cfg = ctx.config.read().await;
        resolve_effective(&cfg, &model, &payload, Some(headers))
    };
    match route {
        // Native GPT model: relay the backend's SSE events as WS frames.
        EffectiveRoute::Native => label_ws_events(
            ws_native_events(ctx, headers, payload).await,
            "codex-native".to_string(),
        ),
        EffectiveRoute::Routed {
            provider,
            upstream_model,
            from_fallback,
        } => {
            tracing::info!(%model, provider = %provider.id, %upstream_model, transport = "ws", from_fallback, "routing request");
            let attempt = if provider.id == crate::providers::CLAUDE_CODE_PROVIDER_ID {
                ws_claude_cli_events(ctx, &provider, &upstream_model, &model, &payload).await
            } else {
                ws_routed_events(ctx, &provider, &upstream_model, &model, &payload).await
            };
            if !from_fallback || attempt.is_ok() {
                return label_ws_events(attempt, provider.id);
            }
            // A failed fallback must never break a side call: retry against
            // the request's original destination (same rule as HTTP).
            tracing::warn!(
                %model,
                fallback_provider = %provider.id,
                error = %attempt.as_ref().err().map(ToString::to_string).unwrap_or_default(),
                "side-call fallback failed; retrying original destination"
            );
            let original = {
                let cfg = ctx.config.read().await;
                resolve(&cfg, &model).map(|(p, m)| (p.clone(), m))
            };
            match original {
                Ok((p, upstream_model)) => {
                    let retry = if p.id == crate::providers::CLAUDE_CODE_PROVIDER_ID {
                        ws_claude_cli_events(ctx, &p, &upstream_model, &model, &payload).await
                    } else {
                        ws_routed_events(ctx, &p, &upstream_model, &model, &payload).await
                    };
                    label_ws_events(retry, p.id)
                }
                Err(_) => label_ws_events(
                    ws_native_events(ctx, headers, payload).await,
                    "codex-native".to_string(),
                ),
            }
        }
    }
}

/// Relay a native-model turn: the backend's SSE events become WS frames.
async fn ws_native_events(
    ctx: &ProxyCtx,
    headers: &HeaderMap,
    mut payload: Value,
) -> anyhow::Result<futures::stream::BoxStream<'static, Result<Value, String>>> {
    sanitize_responses_payload(&mut payload);
    let upstream = native_send(ctx, WireApi::Responses, headers, &payload).await?;
    let status = upstream.status();
    if !status.is_success() {
        let body = upstream.text().await.unwrap_or_default();
        let preview: String = body.chars().take(300).collect();
        bail!("native upstream returned {status}: {preview}");
    }
    Ok(sse_values_stream(upstream.bytes_stream().boxed(), None))
}

/// Run one routed WS turn through the same translation pipeline as the HTTP
/// dispatch (D2). Responses-native upstreams relay events untouched (no
/// translator); chat/anthropic upstreams get one.
async fn ws_routed_events(
    ctx: &ProxyCtx,
    provider: &Provider,
    upstream_model: &str,
    model: &str,
    payload: &Value,
) -> anyhow::Result<futures::stream::BoxStream<'static, Result<Value, String>>> {
    if is_remote_compaction_v2(payload) {
        return routed_compaction_events(ctx, provider, upstream_model, payload).await;
    }
    if codex_request_kind(payload).as_deref() == Some("compaction") {
        record_problem(
            &ctx.stats,
            &provider.id,
            upstream_model,
            "ws",
            None,
            "compaction",
            &format!(
                "{BUILD_LABEL}: Codex sent a compaction call without a compaction_trigger item; treating it as a normal turn"
            ),
        );
    }
    let (path, body, upstream_kind) =
        build_upstream(provider, payload, upstream_model, WireApi::Responses)?;
    // Responses-native upstreams pass through untouched unless freeform tools
    // were converted to ordinary functions for compatibility; those still need
    // the translator so apply_patch comes home as a custom_tool_call.
    let translator = ws_translator_config(provider, upstream_model, model, upstream_kind, payload);
    let upstream = send(ctx, provider, path, &body).await?;
    let status = upstream.status();
    if !status.is_success() {
        log_rejected_upstream_request(provider, path, status, &body);
        let body = upstream.text().await.unwrap_or_default();
        let preview: String = body.chars().take(300).collect();
        bail!("provider '{}' returned {status}: {preview}", provider.id);
    }
    Ok(sse_values_stream(
        upstream.bytes_stream().boxed(),
        translator,
    ))
}

/// Bridge a routed WS turn to the local `claude` CLI (claude-code provider).
///
/// Same contract as `dispatch_claude_cli` for the HTTP path: render the
/// Responses payload to a prompt, run `claude -p`, synthesize the Anthropic
/// SSE frames, and let the existing SSE translator turn them back into
/// Responses event objects for the WS frames.
async fn ws_claude_cli_events(
    ctx: &ProxyCtx,
    provider: &Provider,
    upstream_model: &str,
    model: &str,
    payload: &Value,
) -> anyhow::Result<futures::stream::BoxStream<'static, Result<Value, String>>> {
    if is_remote_compaction_v2(payload) {
        return routed_compaction_events(ctx, provider, upstream_model, payload).await;
    }
    if codex_request_kind(payload).as_deref() == Some("compaction") {
        record_problem(
            &ctx.stats,
            &provider.id,
            upstream_model,
            "ws",
            None,
            "compaction",
            &format!(
                "{BUILD_LABEL}: Codex sent a compaction call without a compaction_trigger item; treating it as a normal turn"
            ),
        );
    }
    let (result, id) = run_claude_turn(payload, upstream_model, WireApi::Responses).await?;
    let frames = crate::claude_cli::anthropic_sse_stream(
        &id,
        upstream_model,
        &result.text,
        result.input_tokens,
        result.output_tokens,
    );
    let bytes: Vec<Bytes> = frames.into_iter().map(Bytes::from).collect();
    let translator = Some((
        UpstreamKind::Anthropic,
        model.to_string(),
        translate::tool_namespace_map(payload),
        translate::freeform_tool_names(payload),
    ));
    Ok(sse_values_stream(
        futures::stream::iter(bytes.into_iter().map(Ok::<_, reqwest::Error>)).boxed(),
        translator,
    ))
}

/// What a routed WS turn passes to the SSE translator: the upstream dialect,
/// the routed model slug, and the request-derived tool maps (namespace +
/// freeform) needed to restore the Responses shape on the way back.
type WsTranslatorConfig = (
    UpstreamKind,
    String,
    std::collections::BTreeMap<String, String>,
    std::collections::BTreeSet<String>,
);

fn ws_translator_config(
    provider: &Provider,
    upstream_model: &str,
    model: &str,
    upstream_kind: UpstreamKind,
    payload: &Value,
) -> Option<WsTranslatorConfig> {
    match upstream_kind {
        UpstreamKind::Responses
            if !needs_responses_function_tool_compat(provider, upstream_model) =>
        {
            None
        }
        kind => Some((
            kind,
            model.to_string(),
            translate::tool_namespace_map(payload),
            translate::freeform_tool_names(payload),
        )),
    }
}

/// Parse an upstream SSE byte stream into Responses event objects. With a
/// translator, upstream chat/anthropic events are converted to the Responses
/// format; without one, the payloads pass through untouched.
fn sse_values_stream(
    bytes: futures::stream::BoxStream<'static, Result<Bytes, reqwest::Error>>,
    translator: Option<WsTranslatorConfig>,
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
        bytes,
        parser: SseParser::new(),
        translator: translator.map(|(kind, model, namespaces, freeform)| {
            StreamTranslator::new(kind, DownstreamKind::Responses, &model)
                .with_tool_namespaces(namespaces)
                .with_freeform_tools(freeform)
        }),
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

/// Forward an SSE stream byte-for-byte, tapping the frame that carries
/// usage so the turn is recorded without altering what the client receives.
///
/// Works for any upstream dialect: `translate::normalize_usage` knows where
/// each one puts its usage, so a Responses stream (`response.completed`) and
/// a Chat Completions stream (final chunk, top-level or per-choice) are both
/// handled here instead of needing a tap apiece.
fn tap_usage_stream(
    upstream: reqwest::Response,
    kind: UpstreamKind,
    stats: SharedStats,
    provider: String,
    model: String,
    started: std::time::Instant,
    visual_assistance: Option<VisualAssistanceMetadata>,
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> {
    // P3: stats/provider/model/started live in the state struct (built once)
    // instead of being cloned on every SSE chunk.
    struct St {
        bytes: futures::stream::BoxStream<'static, Result<Bytes, reqwest::Error>>,
        parser: SseParser,
        recorded: bool,
        kind: UpstreamKind,
        stats: SharedStats,
        provider: String,
        model: String,
        started: std::time::Instant,
        visual_assistance: Option<VisualAssistanceMetadata>,
    }
    let state = St {
        bytes: upstream.bytes_stream().boxed(),
        parser: SseParser::new(),
        recorded: false,
        kind,
        stats,
        provider,
        model,
        started,
        visual_assistance,
    };
    futures::stream::unfold(state, |mut st| async move {
        match st.bytes.next().await {
            Some(Ok(chunk)) => {
                if !st.recorded {
                    for ev in st.parser.push(&chunk) {
                        // Terminator frames ("[DONE]") are not JSON; skip them
                        // quietly rather than treating them as a parse failure.
                        let Ok(v) = serde_json::from_str::<Value>(&ev.data) else {
                            continue;
                        };
                        if record_payload_usage(
                            &st.stats,
                            &st.provider,
                            &st.model,
                            "http",
                            Some(st.started),
                            st.kind,
                            &v,
                            st.visual_assistance.as_ref(),
                        ) {
                            st.recorded = true;
                            break;
                        }
                    }
                }
                Some((Ok(chunk), st))
            }
            Some(Err(e)) => Some((Err(std::io::Error::other(e)), st)),
            None => None,
        }
    })
}

/// Transform an upstream SSE byte stream into the downstream wire format.
/// When `tap` is set, completed Responses turns report their usage.
fn translate_byte_stream(
    bytes: futures::stream::BoxStream<'static, Result<Bytes, reqwest::Error>>,
    upstream_kind: UpstreamKind,
    downstream_kind: DownstreamKind,
    model: &str,
    tool_namespaces: std::collections::BTreeMap<String, String>,
    freeform_tools: std::collections::BTreeSet<String>,
    tap: Option<(
        SharedStats,
        String,
        String,
        std::time::Instant,
        Option<VisualAssistanceMetadata>,
    )>,
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> {
    struct St {
        bytes: futures::stream::BoxStream<'static, Result<Bytes, reqwest::Error>>,
        parser: SseParser,
        translator: StreamTranslator,
        pending: VecDeque<Bytes>,
        upstream_done: bool,
        finalized: bool,
        tap: Option<(
            SharedStats,
            String,
            String,
            std::time::Instant,
            Option<VisualAssistanceMetadata>,
        )>,
    }

    let state = St {
        bytes,
        parser: SseParser::new(),
        translator: StreamTranslator::new(upstream_kind, downstream_kind, model)
            .with_tool_namespaces(tool_namespaces)
            .with_freeform_tools(freeform_tools),
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
                        if let Some((stats, prov, mdl, started, visual_assistance)) = &st.tap {
                            // Translated frames are canonical Responses shape.
                            record_payload_usage(
                                stats,
                                prov,
                                mdl,
                                "http",
                                Some(*started),
                                UpstreamKind::Responses,
                                &f.data,
                                visual_assistance.as_ref(),
                            );
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
                            if let Some((stats, prov, mdl, started, visual_assistance)) = &st.tap {
                                // Translated frames are canonical Responses shape.
                                record_payload_usage(
                                    stats,
                                    prov,
                                    mdl,
                                    "http",
                                    Some(*started),
                                    UpstreamKind::Responses,
                                    &f.data,
                                    visual_assistance.as_ref(),
                                );
                            }
                            push_frame(&mut st.pending, &f, downstream_kind);
                        }
                    }
                }
                Some(Err(e)) => {
                    // B3: a mid-stream upstream error used to fall through to
                    // finalize(), which emitted `response.completed` - the
                    // client saw a truncated turn marked as successful and
                    // nothing was recorded as a failure. Mirror the WS path:
                    // emit an explicit error event, record the failure, and
                    // skip finalize() so no `response.completed` follows.
                    tracing::warn!("upstream stream error: {e}");
                    let message = format!("upstream stream error: {e}");
                    if let Some((stats, prov, mdl, started, _)) = &st.tap {
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

fn push_frame(pending: &mut VecDeque<Bytes>, f: &translate::OutFrame, downstream: DownstreamKind) {
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, ProviderModel, ProviderProtocol};
    use std::collections::BTreeMap;

    /// One cheap provider serving `cheap/mini`; `fallback` maps to
    /// `AppConfig.side_call_fallback`.
    fn demo_config(fallback: Option<&str>) -> AppConfig {
        let mut providers = BTreeMap::new();
        providers.insert(
            "cheap".into(),
            Provider {
                id: "cheap".into(),
                name: "Cheap".into(),
                protocol: ProviderProtocol::OpenAI,
                base_url: "https://api.cheap.example/v1".into(),
                api_key: Some("sk-test".into()),
                has_key: true,
                context_window: None,
                user_agent: None,
                models: vec![ProviderModel {
                    id: "mini".into(),
                    label: None,
                    context_window: None,
                    protocol: None,
                    fast_mode: false,
                    enabled: true,
                    supports_vision: false,
                }],
                enabled: true,
            },
        );
        AppConfig {
            providers,
            side_call_fallback: fallback.map(str::to_string),
            // Other fields evolve in parallel; take their defaults.
            ..Default::default()
        }
    }

    /// The OpenCode shape: one URL, one key, three dialects - the dialect
    /// recorded per model.
    fn multi_dialect_provider() -> Provider {
        let model = |id: &str, protocol: Option<ProviderProtocol>| ProviderModel {
            id: id.into(),
            label: None,
            context_window: None,
            protocol,
            fast_mode: false,
            enabled: true,
            supports_vision: false,
        };
        Provider {
            id: "opencode-go".into(),
            name: "OpenCode Go".into(),
            protocol: ProviderProtocol::OpenAI,
            base_url: "https://opencode.ai/zen/go/v1".into(),
            api_key: Some("sk-test".into()),
            has_key: true,
            context_window: None,
            user_agent: None,
            models: vec![
                model("kimi-k3", Some(ProviderProtocol::OpenAI)),
                model("qwen3.8-max", Some(ProviderProtocol::Anthropic)),
                model("gpt-5.6-luna", Some(ProviderProtocol::Responses)),
                model("deepseek-v4-flash", Some(ProviderProtocol::Responses)),
                // Turned up by discovery, never given a dialect.
                model("something-new", None),
            ],
            enabled: true,
        }
    }

    #[test]
    fn legacy_opencode_slugs_resolve_to_the_merged_provider() {
        // Threads saved before the provider merge still address
        // `opencode-go-chat/<model>`. Without the alias they fell into the
        // native passthrough and the ChatGPT backend rejected the turn with
        // 400, resetting the conversation.
        let mut providers = BTreeMap::new();
        let provider = multi_dialect_provider();
        providers.insert("opencode-go".to_string(), provider);
        let cfg = AppConfig {
            providers,
            ..Default::default()
        };

        for slug in [
            "opencode-go-chat/kimi-k3",
            "opencode-go-claude/qwen3.8-max",
            "opencode-go-responses/gpt-5.6-luna",
        ] {
            let (p, upstream) = resolve(&cfg, slug).expect(slug);
            assert_eq!(
                p.id, "opencode-go",
                "{slug} must resolve to the merged provider"
            );
            assert_eq!(upstream, slug.rsplit_once('/').unwrap().1);
        }
    }

    #[test]
    fn legacy_opencode_slug_keeps_the_models_own_dialect() {
        let mut providers = BTreeMap::new();
        providers.insert("opencode-go".to_string(), multi_dialect_provider());
        let cfg = AppConfig {
            providers,
            ..Default::default()
        };
        let (p, upstream) = resolve(&cfg, "opencode-go-chat/gpt-5.6-luna").unwrap();
        // The merged provider records the Responses dialect on the model, so
        // the chat-slug's old meaning is not resurrected by the alias.
        assert_eq!(model_protocol(p, &upstream), &ProviderProtocol::Responses);
    }
    #[test]
    fn merged_opencode_provider_ignores_unrelated_slugs() {
        let mut providers = BTreeMap::new();
        providers.insert("opencode-go".to_string(), multi_dialect_provider());
        let cfg = AppConfig {
            providers,
            ..Default::default()
        };
        // No legacy suffix: no alias.
        assert_eq!(merged_opencode_provider(&cfg, "opencode-go"), None);
        // A repointed provider id (not a gateway name) is not aliased.
        assert_eq!(merged_opencode_provider(&cfg, "opencode-go-custom"), None);
        // Missing merged provider: the alias must not invent one.
        assert_eq!(merged_opencode_provider(&cfg, "opencode-zen-chat"), None);
    }

    #[test]
    fn each_model_resolves_its_own_dialect() {
        let p = multi_dialect_provider();
        assert_eq!(model_protocol(&p, "kimi-k3"), &ProviderProtocol::OpenAI);
        assert_eq!(
            model_protocol(&p, "qwen3.8-max"),
            &ProviderProtocol::Anthropic
        );
        assert_eq!(
            model_protocol(&p, "gpt-5.6-luna"),
            &ProviderProtocol::Responses
        );
        // Untagged, and unknown to the provider entirely: the provider's own
        // dialect is the only answer available.
        assert_eq!(
            model_protocol(&p, "something-new"),
            &ProviderProtocol::OpenAI
        );
        assert_eq!(model_protocol(&p, "never-seen"), &ProviderProtocol::OpenAI);
    }

    #[test]
    fn the_shipped_zen_preset_does_not_capture_the_native_gpt_slugs() {
        // Not a hypothetical collision: the OpenCode Zen preset serves models
        // under the native names verbatim, so anyone who adds it and then asks
        // Codex for GPT-5.5 is asking a question the bare-name lookup used to
        // answer with OpenCode. Built from PRESETS so a future preset that
        // adds a native name fails here rather than in someone's session.
        let preset = crate::providers::PRESETS
            .iter()
            .find(|p| p.id == "opencode-zen")
            .expect("opencode-zen preset");
        let provider = Provider::from_preset(preset);
        let mut cfg = AppConfig::default();
        assert!(!cfg.native_slug_mode, "normal mode is the default");
        cfg.providers.insert(provider.id.clone(), provider);

        for bare in ["gpt-5.5", "gpt-5.4-mini", "gpt-5.4-nano", "grok-4.5"] {
            assert!(
                matches!(
                    resolve_effective(&cfg, bare, &json!({"model": bare}), None),
                    EffectiveRoute::Native
                ),
                "bare {bare} was captured by a routed provider"
            );
        }
        // The Zen copies stay reachable under their qualified slug, which is
        // what the picker publishes for them.
        let (p, upstream) = resolve(&cfg, "opencode-zen/gpt-5.5").unwrap();
        assert_eq!(p.id, "opencode-zen");
        assert_eq!(upstream, "gpt-5.5");
    }

    #[test]
    fn one_provider_dispatches_each_model_to_its_own_upstream() {
        // The whole point of merging the per-dialect providers: the same
        // provider, key and URL must still reach three different endpoints.
        let p = multi_dialect_provider();
        let payload = json!({"input": [], "stream": false});
        let route = |model: &str| {
            let (path, _body, kind) =
                build_upstream(&p, &payload, model, WireApi::Responses).unwrap();
            (path, kind)
        };
        assert_eq!(
            route("kimi-k3"),
            ("chat/completions", UpstreamKind::OpenAiChat)
        );
        assert_eq!(route("qwen3.8-max"), ("messages", UpstreamKind::Anthropic));
        assert_eq!(
            route("gpt-5.6-luna"),
            ("responses", UpstreamKind::Responses)
        );
    }

    #[test]
    fn opencode_go_deepseek_adapts_custom_tools_for_responses() {
        let provider = multi_dialect_provider();
        let payload = json!({
            "input": [
                {"type": "message", "id": "msg_previous", "role": "user", "content": "fix it"},
                {"type": "function_call", "id": "fc_previous", "call_id": "call_1", "name": "ping", "arguments": "{}", "internal_chat_message_metadata_passthrough": {"secret": true}},
                {"type": "function_call_output", "id": "fco_previous", "call_id": "call_1", "output": [{"type": "input_text", "text": "first"}, {"type": "output_text", "text": "second"}], "internal_chat_message_metadata_passthrough": {"secret": true}}
            ],
            "stream": true,
            "generate": true,
            "tools": [{
                "type": "custom",
                "name": "apply_patch",
                "description": "Apply a patch",
                "format": {"type": "grammar", "syntax": "lark", "definition": "start: \"ok\""}
            }]
        });

        let (path, body, kind) =
            build_upstream(&provider, &payload, "deepseek-v4-flash", WireApi::Responses).unwrap();

        assert_eq!(path, "responses");
        assert_eq!(kind, UpstreamKind::Responses);
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "apply_patch");
        assert_eq!(body["tools"][0]["parameters"]["type"], "object");
        assert_eq!(
            body["tools"][0]["parameters"]["properties"]["input"]["type"],
            "string"
        );
        assert!(body["tools"][0].get("format").is_none());
        assert!(body.get("generate").is_none());
        assert!(body["input"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item.get("id").is_none()));
        assert_eq!(body["input"][1]["call_id"], "call_1");
        assert_eq!(body["input"][2]["call_id"], "call_1");
        assert_eq!(body["input"][2]["output"], "first\nsecond");
        assert!(body["input"].as_array().unwrap().iter().all(|item| item
            .get("internal_chat_message_metadata_passthrough")
            .is_none()));
    }

    #[test]
    fn ws_translator_restores_freeform_tools_for_compat_responses_upstream() {
        let provider = multi_dialect_provider();
        let payload = json!({"tools": [{"type":"custom","name":"apply_patch","description":"p"}]});

        let compat = ws_translator_config(
            &provider,
            "deepseek-v4-flash",
            "opencode-go/deepseek-v4-flash",
            UpstreamKind::Responses,
            &payload,
        )
        .unwrap();
        assert_eq!(compat.0, UpstreamKind::Responses);
        assert_eq!(compat.1, "opencode-go/deepseek-v4-flash");
        assert!(compat.3.contains("apply_patch"));

        assert!(ws_translator_config(
            &provider,
            "gpt-5.6-luna",
            "gpt-5.6-luna",
            UpstreamKind::Responses,
            &payload,
        )
        .is_none());
    }

    #[test]
    fn opencode_go_deepseek_normalizes_an_empty_responses_input() {
        let provider = multi_dialect_provider();
        let payload = json!({
            "input": [],
            "instructions": "prewarm",
            "stream": true
        });

        let (_, body, _) =
            build_upstream(&provider, &payload, "deepseek-v4-flash", WireApi::Responses).unwrap();

        assert_eq!(body["input"], "");
        assert_eq!(body["instructions"], "prewarm");
    }

    #[test]
    fn opencode_go_deepseek_flattens_agent_messages_before_sending_responses() {
        let provider = multi_dialect_provider();
        let payload = json!({
            "input": [{
                "type": "agent_message",
                "author": "/root",
                "recipient": "/root/child",
                "content": [
                    {"type":"input_text","text":"Message Type: NEW_TASK\nTask name: /root/child\nSender: /root\nPayload:\n"},
                    {"type":"encrypted_content","encrypted_content":"Analyze the frontend."}
                ]
            }],
            "stream": true,
            "tools": []
        });

        let (_, body, _) =
            build_upstream(&provider, &payload, "deepseek-v4-flash", WireApi::Responses).unwrap();

        let item = &body["input"][0];
        assert_eq!(item["type"], "message");
        assert_eq!(item["role"], "user");
        assert_eq!(item["content"][0]["type"], "input_text");
        assert_eq!(item["content"][1]["type"], "input_text");
        assert_eq!(item["content"][1]["text"], "Analyze the frontend.");
    }

    #[test]
    fn opencode_go_flattens_encrypted_content_before_chat_completions() {
        let provider = multi_dialect_provider();
        let payload = json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type":"text","text":"Task:\n"},
                    {"type":"encrypted_content","encrypted_content":"Review the change."}
                ]
            }],
            "stream": false
        });

        let (_, body, kind) =
            build_upstream(&provider, &payload, "kimi-k3", WireApi::ChatCompletions).unwrap();

        assert_eq!(kind, UpstreamKind::OpenAiChat);
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert!(content
            .iter()
            .all(|part| part.get("type").and_then(Value::as_str) != Some("encrypted_content")));
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "Review the change.");
    }

    #[test]
    fn opencode_go_deepseek_groups_interleaved_calls_before_outputs() {
        let provider = multi_dialect_provider();
        let payload = json!({
            "input": [
                {"type": "message", "role": "user", "content": "run both"},
                {"type": "reasoning", "summary": [], "content": [{"type": "reasoning_text", "text": "plan"}]},
                {"type": "function_call", "call_id": "call_1", "name": "first", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "first result"},
                {"type": "function_call", "call_id": "call_2", "name": "second", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_2", "output": "second result"}
            ],
            "stream": true,
            "tools": []
        });

        let (_, body, _) =
            build_upstream(&provider, &payload, "deepseek-v4-flash", WireApi::Responses).unwrap();

        let item_types = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["type"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            item_types,
            vec![
                "message",
                "reasoning",
                "function_call",
                "function_call",
                "function_call_output",
                "function_call_output"
            ]
        );
        assert_eq!(body["input"][2]["call_id"], "call_1");
        assert_eq!(body["input"][3]["call_id"], "call_2");
        assert_eq!(body["input"][4]["call_id"], "call_1");
        assert_eq!(body["input"][5]["call_id"], "call_2");
    }

    #[test]
    fn opencode_go_deepseek_moves_interleaved_assistant_message_after_tool_output() {
        let provider = multi_dialect_provider();
        let payload = json!({
            "input": [
                {"type": "message", "role": "user", "content": "inspect it"},
                {"type": "reasoning", "summary": [], "content": [{"type": "reasoning_text", "text": "plan"}]},
                {"type": "function_call", "call_id": "call_1", "name": "inspect", "arguments": "{}"},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "checking"}]},
                {"type": "function_call_output", "call_id": "call_1", "output": "result"}
            ],
            "stream": true,
            "tools": []
        });

        let (_, body, _) =
            build_upstream(&provider, &payload, "deepseek-v4-flash", WireApi::Responses).unwrap();

        let items = body["input"].as_array().unwrap();
        let item_types = items
            .iter()
            .map(|item| item["type"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            item_types,
            vec![
                "message",
                "reasoning",
                "function_call",
                "function_call_output",
                "message"
            ]
        );
        assert_eq!(items[2]["call_id"], "call_1");
        assert_eq!(items[3]["call_id"], "call_1");
        assert_eq!(items[4]["role"], "assistant");
    }

    #[test]
    fn opencode_go_deepseek_moves_interleaved_developer_message_after_tool_output() {
        let provider = multi_dialect_provider();
        let payload = json!({
            "input": [
                {"type": "message", "role": "user", "content": "inspect it"},
                {"type": "reasoning", "summary": [], "content": [{"type": "reasoning_text", "text": "plan"}]},
                {"type": "function_call", "call_id": "call_1", "name": "inspect", "arguments": "{}"},
                {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "context update"}]},
                {"type": "function_call_output", "call_id": "call_1", "output": "result"}
            ],
            "stream": true,
            "tools": []
        });

        let (_, body, _) =
            build_upstream(&provider, &payload, "deepseek-v4-flash", WireApi::Responses).unwrap();

        let items = body["input"].as_array().unwrap();
        assert_eq!(items[2]["type"], "function_call");
        assert_eq!(items[3]["type"], "function_call_output");
        assert_eq!(items[4]["type"], "message");
        assert_eq!(items[4]["role"], "developer");
    }

    #[test]
    fn opencode_go_deepseek_replays_summary_only_reasoning_as_reasoning_text() {
        let provider = multi_dialect_provider();
        let payload = json!({
            "input": [
                {"type": "message", "role": "user", "content": "run the tool"},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "plan"}]},
                {"type": "function_call", "call_id": "call_1", "name": "ping", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
            ],
            "stream": true
        });

        let (_, body, _) =
            build_upstream(&provider, &payload, "deepseek-v4-flash", WireApi::Responses).unwrap();

        let reasoning = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "reasoning")
            .unwrap();
        assert_eq!(reasoning["content"][0]["type"], "reasoning_text");
        assert_eq!(reasoning["content"][0]["text"], "plan");
    }

    /// A Responses payload carrying Codex's turn-metadata marker, exactly as
    /// codex-rs emits it: client_metadata["x-codex-turn-metadata"] is a JSON
    /// string with a `request_kind` field.
    fn payload_with_kind(kind: &str) -> Value {
        json!({
            "model": "gpt-5.5",
            "input": [],
            "stream": true,
            "client_metadata": {
                "x-codex-turn-metadata": json!({"request_kind": kind}).to_string(),
            },
        })
    }

    fn headers_with_kind(kind: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-codex-turn-metadata",
            json!({"request_kind": kind})
                .to_string()
                .parse()
                .expect("header value"),
        );
        headers
    }

    #[test]
    fn auxiliary_kinds_are_side_calls() {
        for kind in ["compaction", "prewarm", "memory"] {
            assert!(
                is_side_call(&payload_with_kind(kind), None),
                "request_kind {kind} must be detected as a side call"
            );
        }
    }

    #[test]
    fn main_turn_is_never_a_side_call() {
        // Explicit main-turn marker.
        assert!(!is_side_call(&payload_with_kind("turn"), None));
        // No metadata at all (older Codex versions, third-party clients).
        assert!(!is_side_call(
            &json!({"model": "gpt-5.5", "input": []}),
            None
        ));
        // client_metadata without the Codex turn marker.
        assert!(!is_side_call(
            &json!({"model": "m", "client_metadata": {"session_id": "s"}}),
            None
        ));
        // Marker present but not valid JSON.
        assert!(!is_side_call(
            &json!({"model": "m", "client_metadata": {"x-codex-turn-metadata": "not json"}}),
            None
        ));
        // Marker JSON without request_kind.
        assert!(!is_side_call(
            &json!({"model": "m", "client_metadata": {
                "x-codex-turn-metadata": json!({"session_id": "s"}).to_string()
            }}),
            None
        ));
    }

    #[test]
    fn header_marker_is_detected() {
        let payload = json!({"model": "gpt-5.5", "input": []});
        assert!(is_side_call(
            &payload,
            Some(&headers_with_kind("compaction"))
        ));
        assert!(is_side_call(&payload, Some(&headers_with_kind("prewarm"))));
        assert!(!is_side_call(&payload, Some(&headers_with_kind("turn"))));
        // Body marker wins when both are present.
        assert!(is_side_call(
            &payload_with_kind("compaction"),
            Some(&headers_with_kind("turn"))
        ));
    }

    #[test]
    fn bare_native_model_is_not_captured_in_normal_mode() {
        let mut cfg = demo_config(None);
        cfg.providers.get_mut("cheap").unwrap().models[0].id = "gpt-5.5".into();

        assert!(resolve(&cfg, "gpt-5.5").is_err());
        assert!(matches!(
            resolve_effective(&cfg, "gpt-5.5", &json!({"model": "gpt-5.5"}), None),
            EffectiveRoute::Native
        ));
    }

    #[test]
    fn qualified_model_routes_despite_a_native_name_collision() {
        let mut cfg = demo_config(None);
        cfg.providers.get_mut("cheap").unwrap().models[0].id = "gpt-5.5".into();

        let (provider, upstream) = resolve(&cfg, "cheap/gpt-5.5").unwrap();
        assert_eq!(provider.id, "cheap");
        assert_eq!(upstream, "gpt-5.5");
    }

    #[test]
    fn bare_model_routes_when_native_slug_mode_is_enabled() {
        let mut cfg = demo_config(None);
        cfg.native_slug_mode = true;
        cfg.providers.get_mut("cheap").unwrap().models[0].id = "gpt-5.5".into();

        let (provider, upstream) = resolve(&cfg, "gpt-5.5").unwrap();
        assert_eq!(provider.id, "cheap");
        assert_eq!(upstream, "gpt-5.5");
    }

    #[test]
    fn fallback_routes_side_calls() {
        let cfg = demo_config(Some("cheap/mini"));
        // A native-model side call that would otherwise hit the ChatGPT
        // passthrough is rerouted to the fallback provider.
        match resolve_effective(&cfg, "gpt-5.5", &payload_with_kind("compaction"), None) {
            EffectiveRoute::Routed {
                provider,
                upstream_model,
                from_fallback,
            } => {
                assert_eq!(provider.id, "cheap");
                assert_eq!(upstream_model, "mini");
                assert!(from_fallback);
            }
            EffectiveRoute::Native => panic!("side call must take the fallback route"),
        }
        // Header-only marker (WS upgrade / compatibility projection) works too.
        match resolve_effective(
            &cfg,
            "gpt-5.5",
            &json!({"model": "gpt-5.5", "input": []}),
            Some(&headers_with_kind("prewarm")),
        ) {
            EffectiveRoute::Routed { from_fallback, .. } => assert!(from_fallback),
            EffectiveRoute::Native => panic!("header-marked side call must take the fallback"),
        }
    }

    #[test]
    fn fallback_never_touches_main_turns() {
        let cfg = demo_config(Some("cheap/mini"));
        // Native model, main turn: unchanged native passthrough.
        assert!(matches!(
            resolve_effective(&cfg, "gpt-5.5", &payload_with_kind("turn"), None),
            EffectiveRoute::Native
        ));
        assert!(matches!(
            resolve_effective(
                &cfg,
                "gpt-5.5",
                &json!({"model": "gpt-5.5", "input": []}),
                None
            ),
            EffectiveRoute::Native
        ));
        // Routed model, main turn: normal routing, not flagged as fallback.
        match resolve_effective(&cfg, "cheap/mini", &payload_with_kind("turn"), None) {
            EffectiveRoute::Routed {
                provider,
                from_fallback,
                ..
            } => {
                assert_eq!(provider.id, "cheap");
                assert!(!from_fallback);
            }
            EffectiveRoute::Native => panic!("cheap/mini must resolve normally"),
        }
    }

    #[test]
    fn upstream_request_diagnostics_describe_shape_without_request_contents() {
        let body = json!({
            "model": "deepseek-v4-flash",
            "stream": true,
            "reasoning": {"effort": "high"},
            "tools": [
                {
                    "type": "function",
                    "name": "secret_tool",
                    "description": "private tool description",
                    "parameters": {"type": "object", "properties": {}}
                },
                {
                    "type": "namespace",
                    "name": "private_namespace",
                    "tools": [{"type": "custom", "name": "secret_nested_tool"}]
                }
            ],
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "top-secret prompt"},
                        {"type": "input_text", "text": "<visual-evidence>private image analysis</visual-evidence>"}
                    ]
                },
                {"type": "reasoning", "summary": []}
            ]
        });

        let diagnostics = upstream_request_diagnostics(&body);

        assert_eq!(diagnostics.message_count, 2);
        assert_eq!(diagnostics.tool_count, 2);
        assert_eq!(diagnostics.input_item_types["message"], 1);
        assert_eq!(diagnostics.input_item_types["reasoning"], 1);
        assert_eq!(diagnostics.tool_types["function"], 1);
        assert_eq!(diagnostics.tool_types["namespace"], 1);
        assert_eq!(diagnostics.nested_tool_types["custom"], 1);
        assert_eq!(diagnostics.function_parameter_root_types["object"], 1);
        assert!(diagnostics.top_level_fields.contains("reasoning"));
        assert!(diagnostics.has_visual_evidence);
        assert!(diagnostics.has_reasoning_effort);
        let rendered = format!("{diagnostics:?}");
        assert!(!rendered.contains("top-secret prompt"));
        assert!(!rendered.contains("private image analysis"));
        assert!(!rendered.contains("secret_tool"));
        assert!(!rendered.contains("secret_nested_tool"));
        assert!(!rendered.contains("private_namespace"));
        assert!(!rendered.contains("private tool description"));
    }

    #[test]
    fn upstream_request_diagnostics_count_tool_call_pairing_without_exposing_ids() {
        let body = json!({
            "input": [
                {"type": "function_call", "call_id": "matched-secret", "name": "first"},
                {"type": "function_call_output", "call_id": "matched-secret", "output": "private output"},
                {"type": "function_call", "call_id": "orphan-call-secret", "name": "second"},
                {"type": "function_call_output", "call_id": "orphan-output-secret", "output": "private output"},
                {"type": "function_call_output", "call_id": "out-of-order-secret", "output": "private output"},
                {"type": "function_call", "call_id": "out-of-order-secret", "name": "third"}
            ]
        });

        let diagnostics = upstream_request_diagnostics(&body);

        assert_eq!(diagnostics.matched_function_call_count, 2);
        assert_eq!(diagnostics.unmatched_function_call_count, 1);
        assert_eq!(diagnostics.unmatched_function_output_count, 1);
        assert_eq!(diagnostics.function_output_before_call_count, 1);
        assert_eq!(diagnostics.function_output_value_types["string"], 3);
        assert_eq!(diagnostics.function_call_field_sets["call_id,name,type"], 3);
        assert_eq!(
            diagnostics.function_output_field_sets["call_id,output,type"],
            3
        );
        let rendered = format!("{diagnostics:?}");
        for secret in [
            "matched-secret",
            "orphan-call-secret",
            "orphan-output-secret",
            "out-of-order-secret",
            "private output",
        ] {
            assert!(!rendered.contains(secret));
        }
    }

    #[test]
    fn upstream_request_diagnostics_describe_reasoning_shape_without_exposing_text() {
        let body = json!({
            "input": [
                {"type": "message", "role": "user", "content": "question"},
                {
                    "type": "reasoning",
                    "summary": [{"type": "summary_text", "text": "private summary"}],
                    "content": [
                        {"type": "reasoning_text", "text": "private reasoning"},
                        {"type": "text", "text": "private auxiliary text"}
                    ],
                    "encrypted_content": "private encrypted reasoning"
                },
                {"type": "function_call", "call_id": "private-call", "name": "tool"}
            ]
        });

        let diagnostics = upstream_request_diagnostics(&body);

        assert_eq!(diagnostics.reasoning_positions, vec![1]);
        assert_eq!(
            diagnostics.reasoning_field_sets["content,encrypted_content,summary,type"],
            1
        );
        assert_eq!(
            diagnostics.reasoning_content_part_types["reasoning_text"],
            1
        );
        assert_eq!(diagnostics.reasoning_content_part_types["text"], 1);
        assert_eq!(diagnostics.reasoning_content_text_bytes, 39);
        assert_eq!(diagnostics.reasoning_summary_part_types["summary_text"], 1);
        assert_eq!(diagnostics.reasoning_summary_text_bytes, 15);
        assert_eq!(diagnostics.reasoning_encrypted_content_count, 1);
        let rendered = format!("{diagnostics:?}");
        for secret in [
            "private summary",
            "private reasoning",
            "private auxiliary text",
            "private encrypted reasoning",
            "private-call",
        ] {
            assert!(!rendered.contains(secret));
        }
    }

    #[test]
    fn disabled_fallback_leaves_routing_unchanged() {
        let cfg = demo_config(None);
        // Side call on a native model: still the native passthrough.
        assert!(matches!(
            resolve_effective(&cfg, "gpt-5.5", &payload_with_kind("compaction"), None),
            EffectiveRoute::Native
        ));
        // Side call on a routed model: normal routing, no fallback flag.
        match resolve_effective(&cfg, "cheap/mini", &payload_with_kind("compaction"), None) {
            EffectiveRoute::Routed { from_fallback, .. } => assert!(!from_fallback),
            EffectiveRoute::Native => panic!("cheap/mini must resolve normally"),
        }
    }

    #[test]
    fn unknown_or_disabled_fallback_slug_is_ignored() {
        // Unknown provider in the slug.
        let cfg = demo_config(Some("nope/missing"));
        assert!(matches!(
            resolve_effective(&cfg, "gpt-5.5", &payload_with_kind("compaction"), None),
            EffectiveRoute::Native
        ));
        // Known provider but disabled.
        let mut cfg = demo_config(Some("cheap/mini"));
        cfg.providers.get_mut("cheap").unwrap().enabled = false;
        assert!(matches!(
            resolve_effective(&cfg, "gpt-5.5", &payload_with_kind("compaction"), None),
            EffectiveRoute::Native
        ));
    }

    #[test]
    fn finds_responses_data_and_remote_images_without_text_only_parts() {
        let payload = json!({
            "input": [
                {"role": "user", "content": [
                    {"type": "input_text", "text": "compare these"},
                    {"type": "input_image", "image_url": "data:image/png;base64,aGVsbG8="},
                    {"type": "input_image", "image_url": "https://images.example/diagram.jpg"}
                ]},
                {"role": "user", "content": [{"type": "input_text", "text": "no image here"}]}
            ]
        });

        let images = image_parts_in_payload(&payload, WireApi::Responses);

        assert_eq!(images.len(), 2);
        assert_eq!(images[0].image.url, "data:image/png;base64,aGVsbG8=");
        assert_eq!(images[0].image.mime_type.as_deref(), Some("image/png"));
        assert_eq!(images[1].image.url, "https://images.example/diagram.jpg");
        assert_eq!(images[1].image.mime_type, None);
    }

    #[test]
    fn finds_multiple_chat_image_url_parts() {
        let payload = json!({
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "What changed?"},
                {"type": "image_url", "image_url": {"url": "https://images.example/before.png"}},
                {"type": "image_url", "image_url": {"url": "https://images.example/after.webp"}}
            ]}]
        });

        let images = image_parts_in_payload(&payload, WireApi::ChatCompletions);

        assert_eq!(images.len(), 2);
        assert_eq!(images[0].image.url, "https://images.example/before.png");
        assert_eq!(images[1].image.url, "https://images.example/after.webp");
    }

    #[tokio::test]
    async fn rejects_non_user_images_before_visual_provider_preparation() {
        // A missing visual model makes this a useful ordering assertion: the
        // role check must win before the visual provider chain is resolved.
        let mut cfg = demo_config(None);
        cfg.visual_assistance.enabled = true;
        let mut payload = json!({
            "input": [{"role": "developer", "content": [
                {"type": "input_image", "image_url": "data:image/png;base64,aGVsbG8="}
            ]}]
        });
        let original = payload.clone();

        let error = prepare_visual_assistance(
            &reqwest::Client::new(),
            &cfg,
            &mut payload,
            WireApi::Responses,
            "cheap/mini",
        )
        .await
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("visual assistance only supports image parts in user messages"));
        assert_eq!(payload, original);
    }

    #[test]
    fn keeps_each_user_images_evidence_with_its_own_message() {
        let mut payload = json!({
            "input": [
                {"role": "user", "content": [
                    {"type": "input_text", "text": "first image"},
                    {"type": "input_image", "image_url": "https://images.example/first.png"}
                ]},
                {"role": "user", "content": [
                    {"type": "input_text", "text": "second image"},
                    {"type": "input_image", "image_url": "https://images.example/second.png"}
                ]}
            ]
        });
        let evidence = vec![
            (
                0,
                "<untrusted-image-evidence>first</untrusted-image-evidence>".to_string(),
            ),
            (
                1,
                "<untrusted-image-evidence>second</untrusted-image-evidence>".to_string(),
            ),
        ];

        enrich_payload_with_evidence(&mut payload, WireApi::Responses, &evidence).unwrap();

        assert_eq!(payload["input"][0]["content"][1]["text"], evidence[0].1);
        assert_eq!(payload["input"][1]["content"][1]["text"], evidence[1].1);
        assert!(image_parts_in_payload(&payload, WireApi::Responses).is_empty());
    }

    #[test]
    fn enriches_only_user_content_and_removes_responses_images() {
        let mut payload = json!({
            "instructions": "do not change this system instruction",
            "input": [
                {"role": "developer", "content": [{"type": "input_text", "text": "developer text"}]},
                {"role": "user", "content": [
                    {"type": "input_text", "text": "describe it"},
                    {"type": "input_image", "image_url": "https://images.example/diagram.png"}
                ]}
            ]
        });
        let evidence = "<untrusted-image-evidence>OCR: Chart</untrusted-image-evidence>";

        enrich_payload_with_evidence(
            &mut payload,
            WireApi::Responses,
            &[(1, evidence.to_string())],
        )
        .unwrap();

        assert_eq!(
            payload["instructions"],
            "do not change this system instruction"
        );
        assert_eq!(payload["input"][0]["content"][0]["text"], "developer text");
        assert_eq!(payload["input"][1]["content"].as_array().unwrap().len(), 2);
        assert_eq!(payload["input"][1]["content"][0]["text"], "describe it");
        assert_eq!(payload["input"][1]["content"][1]["text"], evidence);
        assert!(image_parts_in_payload(&payload, WireApi::Responses).is_empty());
    }

    #[test]
    fn enriches_chat_user_text_and_removes_only_image_parts() {
        let mut payload = json!({
            "messages": [
                {"role": "system", "content": "keep system"},
                {"role": "user", "content": [
                    {"type": "text", "text": "read this"},
                    {"type": "image_url", "image_url": {"url": "https://images.example/doc.png"}}
                ]}
            ]
        });
        let evidence = "<untrusted-image-evidence>OCR: Hello</untrusted-image-evidence>";

        enrich_payload_with_evidence(
            &mut payload,
            WireApi::ChatCompletions,
            &[(1, evidence.to_string())],
        )
        .unwrap();

        assert_eq!(payload["messages"][0]["content"], "keep system");
        assert_eq!(payload["messages"][1]["content"][0]["text"], "read this");
        assert_eq!(payload["messages"][1]["content"][1]["text"], evidence);
        assert!(image_parts_in_payload(&payload, WireApi::ChatCompletions).is_empty());
    }

    #[test]
    fn visual_capability_uses_the_routed_model_configuration() {
        let mut cfg = demo_config(None);
        cfg.providers.get_mut("cheap").unwrap().models[0].supports_vision = true;

        assert!(model_supports_vision(&cfg, "cheap/mini").unwrap());
        cfg.providers.get_mut("cheap").unwrap().models[0].supports_vision = false;
        assert!(!model_supports_vision(&cfg, "cheap/mini").unwrap());
    }

    #[tokio::test]
    async fn native_vision_destination_bypasses_an_unconfigured_visual_chain() {
        let mut cfg = demo_config(None);
        cfg.visual_assistance.enabled = true;
        cfg.providers.get_mut("cheap").unwrap().models[0].supports_vision = true;
        let mut payload = json!({
            "input": [{"role": "user", "content": [
                {"type": "input_image", "image_url": "https://images.example/native.png"}
            ]}]
        });
        let original = payload.clone();

        prepare_visual_assistance(
            &reqwest::Client::new(),
            &cfg,
            &mut payload,
            WireApi::Responses,
            "cheap/mini",
        )
        .await
        .unwrap();

        assert_eq!(payload, original);
    }

    #[tokio::test]
    async fn disabled_assistance_preserves_a_text_only_request() {
        let cfg = demo_config(None);
        let mut payload = json!({
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "describe"},
                {"type": "image_url", "image_url": {"url": "https://images.example/disabled.png"}}
            ]}]
        });
        let original = payload.clone();

        prepare_visual_assistance(
            &reqwest::Client::new(),
            &cfg,
            &mut payload,
            WireApi::ChatCompletions,
            "cheap/mini",
        )
        .await
        .unwrap();

        assert_eq!(payload, original);
    }

    #[tokio::test]
    async fn disabled_assistance_preserves_images_for_an_uncatalogued_routed_model() {
        let cfg = demo_config(None);
        let mut payload = json!({
            "input": [{"role": "user", "content": [
                {"type": "input_image", "image_url": "https://images.example/uncatalogued.png"}
            ]}]
        });
        let original = payload.clone();

        prepare_visual_assistance(
            &reqwest::Client::new(),
            &cfg,
            &mut payload,
            WireApi::Responses,
            "cheap/not-in-models",
        )
        .await
        .unwrap();

        assert_eq!(payload, original);
    }

    #[tokio::test]
    async fn exhausted_visual_chain_returns_before_the_text_only_payload_is_built() {
        let mut cfg = demo_config(None);
        cfg.visual_assistance.enabled = true;
        let mut payload = json!({
            "input": [{"role": "user", "content": [
                {"type": "input_image", "image_url": "https://images.example/failure.png"}
            ]}]
        });
        let original = payload.clone();

        let error = prepare_visual_assistance(
            &reqwest::Client::new(),
            &cfg,
            &mut payload,
            WireApi::Responses,
            "cheap/mini",
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("no primary model configured"));
        assert_eq!(payload, original);
    }

    #[tokio::test]
    async fn visual_chain_errors_are_redacted_before_logs_and_gateway_responses() {
        let image_url = "https://private.example/secret-image.png";
        let prompt = "customer roadmap: do not disclose";
        let api_key = "sk-visual-test-secret";
        let chain_error = anyhow::Error::new(visual::VisualAnalysisFailure::new(
            format!(
                "visual assistance exhausted configured fallbacks: provider returned 503 for {image_url}; prompt={prompt}; authorization={api_key}"
            ),
            vec![visual::VisionAttempt {
                model: "vision/fallback".into(),
                retryable: true,
                status: Some(503),
                duration_ms: 1_700,
                error: format!("provider returned 503 for {image_url}; authorization={api_key}"),
            }],
        ));

        let stats = std::sync::Arc::new(tokio::sync::RwLock::new(crate::stats::Stats::in_memory()));
        let failure = visual_preparation_failure(
            &stats,
            "vision-provider",
            "vision/model",
            "http",
            std::time::Instant::now(),
            &chain_error,
        );
        let gateway = structured_error_response(StatusCode::BAD_GATEWAY, failure.to_string());
        let gateway_body = String::from_utf8(
            axum::body::to_bytes(gateway.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        let log = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if let Some(entry) = stats.read().await.recent(1).into_iter().next() {
                    return entry;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("visual preparation failure should reach request logs");

        for sensitive in [image_url, prompt, api_key] {
            assert!(!log.error.as_deref().unwrap_or_default().contains(sensitive));
            assert!(!gateway_body.contains(sensitive));
        }
        assert_eq!(
            log.error.as_deref(),
            Some("visual assistance exhausted configured fallbacks")
        );
        assert!(gateway_body.contains("visual assistance exhausted configured fallbacks"));
        let attempt = &log
            .visual_assistance
            .as_ref()
            .expect("exhausted visual chains retain attempt metadata")
            .attempts[0];
        assert_eq!(attempt.model, "vision/fallback");
        assert!(attempt.retryable);
        assert_eq!(attempt.status, Some(503));
        assert_eq!(attempt.duration_ms, 1_700);
        assert_eq!(attempt.error, "provider returned HTTP 503");
        assert!(!attempt.error.contains(image_url));
        assert!(!attempt.error.contains(api_key));
    }

    fn item(role: &str, text: &str) -> Value {
        serde_json::json!({
            "role": role,
            "content": [{"type": "input_text", "text": text}],
        })
    }

    #[test]
    fn clamp_keeps_a_conversation_that_fits_untouched() {
        let items = vec![item("user", "short")];
        let (fit, dropped) = clamp_to_window(items.clone(), 1_000_000);
        assert!(dropped.is_empty());
        assert_eq!(fit, items);
    }

    #[test]
    fn clamp_drops_oldest_never_the_recent_tail() {
        let items = vec![
            item("user", "a".repeat(10_000).as_str()),
            item("assistant", "b".repeat(10_000).as_str()),
            item("user", "c".repeat(10_000).as_str()),
            item("user", "the actual question"),
        ];
        let (fit, dropped) = clamp_to_window(items, 26_000);
        assert_eq!(dropped.len(), 2);
        assert!(dropped
            .iter()
            .any(|v| v.to_string().contains("a".repeat(10_000).as_str())));
        assert!(!dropped
            .iter()
            .any(|v| v.to_string().contains("the actual question")));
        // The most recent user turn must survive the cut.
        assert!(fit
            .last()
            .unwrap()
            .to_string()
            .contains("the actual question"));
        // The oldest turns are gone; the tail is preserved in order.
        assert!(!fit
            .iter()
            .any(|v| v.to_string().contains("a".repeat(10_000).as_str())));
    }

    #[test]
    fn clamp_never_empties_the_conversation() {
        let items = vec![item("user", "keep me at any cost")];
        let (fit, dropped) = clamp_to_window(items.clone(), 10);
        assert!(dropped.is_empty());
        assert_eq!(fit.len(), 1);
        assert!(fit[0].to_string().contains("keep me at any cost"));
    }

    #[test]
    fn clamp_reports_every_dropped_turn() {
        let items = vec![
            item("user", "x".repeat(20_000).as_str()),
            item("assistant", "y".repeat(20_000).as_str()),
            item("user", "z".repeat(20_000).as_str()),
            item("user", "tail"),
        ];
        let (fit, dropped) = clamp_to_window(items, 5_000);
        assert_eq!(fit.len(), 1);
        // Dropped and kept are complementary: nothing is lost or duplicated.
        assert_eq!(dropped.len(), 3);
        assert!(fit[0].to_string().contains("tail"));
    }

    #[test]
    fn compaction_falls_back_to_truncated_text_for_oversized_item() {
        let mut provider = multi_dialect_provider();
        provider.models.iter_mut().for_each(|m| {
            if m.id == "deepseek-v4-flash" {
                m.context_window = Some(1_000_000);
            }
        });
        let payload = json!({
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "x".repeat(4_000_000)}]},
                {"type": "compaction_trigger"}
            ],
            "stream": true
        });

        let prepared = fit_compaction_input(&provider, "deepseek-v4-flash", &payload);
        let items = prepared["input"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        let text = items[0]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.len() < 3_000_000,
            "oversized item must be truncated: {}",
            text.len()
        );
        assert_eq!(
            prepared["instructions"],
            "You are a conversation summarizer."
        );
        let estimated = estimate_tokens(items) + estimate_non_input_tokens(&prepared, items);
        let budget = 1_000_000 - CONTEXT_RESERVE_TOKENS * 2;
        assert!(
            estimated <= budget,
            "compaction payload estimate {estimated} exceeds {budget}"
        );
    }

    #[test]
    fn render_items_as_text_flattens_responses_blocks_with_roles() {
        let items = vec![
            item("user", "first turn"),
            item("assistant", "second turn"),
            item("user", "third turn"),
        ];
        let text = render_items_as_text(&items);
        assert!(text.contains("user: first turn"));
        assert!(text.contains("assistant: second turn"));
        assert!(text.contains("user: third turn"));
        assert_eq!(text.matches("user:").count(), 2);
    }

    #[test]
    fn render_items_as_text_skips_blocks_without_text() {
        let items = vec![
            serde_json::json!({"role": "user", "content": [{"type": "input_text", "text": "kept"}]}),
            serde_json::json!({"role": "assistant", "content": [{"type": "function_call", "name": "x"}]}),
        ];
        let text = render_items_as_text(&items);
        assert!(text.contains("user: kept"));
        assert!(!text.contains("function_call"));
    }

    /// A minimal Responses-wire input item carrying a stable `id`, for the
    /// WS history tests. The clamp tests above use `item(role, text)`; the
    /// history rebuild keys off `id`, so it needs its own fixture.
    fn history_item(label: &str) -> Value {
        json!({"id": label, "type": "message", "role": "user", "content": label})
    }

    #[test]
    fn follow_up_turn_appends_delta_to_the_cached_base() {
        let mut history = WsHistory::new();
        history.insert(
            "resp-1".into(),
            vec![history_item("a"), history_item("b"), history_item("c")],
            None,
        );
        let rebuilt = rebuild_input(&history, Some("resp-1"), vec![history_item("d")]);
        assert_eq!(
            rebuilt
                .iter()
                .map(|v| v["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["a", "b", "c", "d"]
        );
    }

    #[test]
    fn unknown_previous_response_id_degrades_to_delta_alone() {
        // A fresh conversation, or an id the cache already evicted: the old
        // pre-cache behavior, delta-only.
        let history = WsHistory::new();
        let rebuilt = rebuild_input(&history, Some("never-cached"), vec![history_item("d")]);
        assert_eq!(rebuilt.len(), 1);
        assert_eq!(rebuilt[0]["id"], "d");
    }

    #[test]
    fn native_follow_up_replaces_previous_response_id_with_rebuilt_input() {
        let mut payload = json!({
            "model": "gpt-5.6-sol",
            "previous_response_id": "resp_1",
            "input": [history_item("new")],
        });
        let full_input = vec![history_item("old"), history_item("new")];

        replace_incremental_input(&mut payload, full_input.clone());

        assert_eq!(payload["input"], json!(full_input));
        assert!(payload.get("previous_response_id").is_none());
    }

    #[test]
    fn native_payload_strips_the_unsupported_generate_flag() {
        let mut payload = json!({"model": "gpt-5.6-terra", "generate": true});

        sanitize_responses_payload(&mut payload);

        assert!(payload.get("generate").is_none());
        assert_eq!(payload["model"], "gpt-5.6-terra");
    }

    #[test]
    fn a_reconnect_rebuilds_input_from_the_shared_history() {
        // The regression: Codex reconnects mid-conversation, starting a new
        // WS session. The history is shared per-process (not per-session), so
        // the new session's follow-up turn still finds the prior turns.
        let shared = Arc::new(Mutex::new(WsHistory::new()));

        // Session 1 completes a turn; the full input + output is cached under
        // the response id it echoed to Codex.
        let record = {
            let mut r = vec![history_item("a"), history_item("b")];
            r.push(
                json!({"id": "asst-1", "type": "message", "role": "assistant",
                          "content": "oi"}),
            );
            r
        };
        shared.lock().unwrap().insert("resp-1".into(), record, None);

        // Session 2 (post-reconnect) sends only the delta + previous_response_id.
        let items = {
            let history = shared.lock().unwrap_or_else(|e| e.into_inner());
            rebuild_input(&history, Some("resp-1"), vec![history_item("c")])
        };
        assert_eq!(
            items
                .iter()
                .map(|v| v["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["a", "b", "asst-1", "c"]
        );
    }

    #[test]
    fn ws_history_evicts_oldest_first() {
        let mut history = WsHistory::new();
        for i in 0..(WS_HISTORY_MAX_ENTRIES + 10) {
            history.insert(
                format!("resp-{i}"),
                vec![history_item(&format!("m{i}"))],
                None,
            );
        }
        assert!(history.get("resp-0").is_none(), "oldest must be evicted");
        assert!(
            history
                .get(&format!("resp-{}", WS_HISTORY_MAX_ENTRIES + 9))
                .is_some(),
            "newest must survive"
        );
        assert_eq!(history.order.len(), WS_HISTORY_MAX_ENTRIES);
    }

    #[test]
    fn an_entry_larger_than_the_byte_budget_survives() {
        // The regression behind "context reset at 304k": a long conversation's
        // rebuilt input alone serializes past WS_HISTORY_MAX_BYTES. The old
        // eviction loop treated the just-inserted entry as the oldest when it
        // was the only one, removed it, and the next turn resolved
        // `previous_response_id` against nothing - delta-only, context to zero.
        let mut history = WsHistory::new();
        let big = vec![json!({
            "id": "big",
            "type": "message",
            "role": "user",
            "content": "x".repeat(WS_HISTORY_MAX_BYTES),
        })];
        assert!(
            big.iter().map(|v| v.to_string().len()).sum::<usize>() > WS_HISTORY_MAX_BYTES,
            "fixture must exceed the byte budget"
        );
        history.insert("resp-1".into(), big, None);
        assert!(
            history.get("resp-1").is_some(),
            "the stored turn must survive"
        );
        assert_eq!(history.order.len(), 1);
    }

    #[test]
    fn a_follow_up_replaces_the_turn_it_was_built_on() {
        // Each entry contains the whole conversation so far, so the entry the
        // follow-up was rebuilt from is fully subsumed: inserting the new turn
        // drops the old one, keeping exactly one entry per conversation.
        let mut history = WsHistory::new();
        history.insert("resp-1".into(), vec![history_item("a")], None);
        history.insert(
            "resp-2".into(),
            vec![history_item("a"), history_item("b")],
            Some("resp-1"),
        );
        assert!(
            history.get("resp-1").is_none(),
            "subsumed turn must be dropped"
        );
        assert_eq!(
            history.get("resp-2").unwrap().iter().count(),
            2,
            "the newest entry keeps the full conversation"
        );
    }

    #[test]
    fn the_just_inserted_turn_is_never_evicted_by_its_own_insert() {
        // A burst of small entries leaves the cache at the entry cap, then a
        // single oversized turn (the conversation's newest) lands: the insert
        // that stores it must not evict it to make room. FIFO keeps it and
        // drops the oldest of the small ones instead.
        let mut history = WsHistory::new();
        for i in 0..(WS_HISTORY_MAX_ENTRIES + 10) {
            history.insert(
                format!("small-{i}"),
                vec![history_item(&format!("s{i}"))],
                None,
            );
        }
        assert_eq!(history.order.len(), WS_HISTORY_MAX_ENTRIES);
        let big = vec![json!({
            "id": "conv-1",
            "type": "message",
            "role": "user",
            "content": "y".repeat(WS_HISTORY_MAX_BYTES),
        })];
        history.insert("conv-1".into(), big, None);
        assert!(
            history.get("conv-1").is_some(),
            "a just-stored conversation turn must never be evicted by its own insert"
        );
        // The oversized turn alone exceeds the byte budget, so everything
        // older is evicted down to that one entry; the newest survives.
        assert_eq!(history.order.len(), 1);
    }

    #[test]
    fn two_large_conversations_coexist_without_evicting_each_other() {
        // The multi-conversation gap: a byte budget of 512KB meant one
        // 304k-token conversation (~1.2MB serialized) already blew the cap,
        // so a second long conversation's insert evicted the first one's
        // entry and reset it. With the raised budget each conversation keeps
        // its own newest turn; both must survive side by side.
        let mut history = WsHistory::new();
        let conv_a = vec![json!({
            "id": "a",
            "type": "message",
            "role": "user",
            "content": "a".repeat(2 * 1024 * 1024),
        })];
        let conv_b = vec![json!({
            "id": "b",
            "type": "message",
            "role": "user",
            "content": "b".repeat(2 * 1024 * 1024),
        })];
        history.insert("resp-a".into(), conv_a, None);
        history.insert("resp-b".into(), conv_b, None);
        assert!(
            history.get("resp-a").is_some(),
            "conversation A was evicted"
        );
        assert!(
            history.get("resp-b").is_some(),
            "conversation B was evicted"
        );
        assert_eq!(history.order.len(), 2);
    }
}
