//! Local proxy: receives requests from the coding agent and dispatches
//! them to the right provider based on the `model` field, translating
//! both the request and the response (including SSE streams).
//!
//! Endpoints (all bound to 127.0.0.1):
//!   POST /v1/responses        — Codex Responses API
//!   POST /v1/chat/completions — OpenAI-compatible clients
//!   GET  /health              — liveness for the UI

use crate::config::{
    AppConfig, Provider, DEFAULT_MAX_REQUEST_BODY_BYTES, MAX_REQUEST_BODY_BYTES_HARD_LIMIT,
};
use crate::keypool::KeyPools;
use crate::sse::{frame_data, frame_done, frame_with_event, SseParser};
use crate::state::SharedConfig;
use crate::stats::{SharedStats, VisualAssistanceMetadata, VisualAttemptProvenance};
use crate::translate::{self, DownstreamKind, StreamTranslator, UpstreamKind};
use crate::visual;
use anyhow::{anyhow, bail};
use axum::{
    body::Body,
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        DefaultBodyLimit, State as AxState,
    },
    http::{HeaderMap, StatusCode},
    middleware::{self},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

mod auth;
mod dispatch;
mod realtime;
mod routing;
mod streaming;
mod upstream;
mod visuals;

use realtime::*;
use streaming::*;
use visuals::*;
#[cfg(test)]
#[path = "proxy/tests_characterization.rs"]
mod tests_characterization;
#[cfg(test)]
#[path = "proxy/tests_keys.rs"]
mod tests_keys;
#[cfg(test)]
#[path = "proxy/tests_realtime.rs"]
mod tests_realtime;
#[cfg(test)]
#[path = "proxy/tests_routing.rs"]
mod tests_routing;
#[cfg(test)]
#[path = "proxy/tests_visual.rs"]
mod tests_visual;

use auth::auth_gate;
pub use auth::local_token;
pub use routing::{family_of, model_protocol, ProviderFamily};
#[cfg(test)]
use routing::{is_side_call, merged_opencode_provider};
use routing::{resolve, resolve_effective, RoutePlan};
pub use upstream::apply_provider_auth;
#[cfg(test)]
use upstream::classify_status;
use upstream::{build_upstream, needs_responses_function_tool_compat, send};

type EffectiveRoute = RoutePlan;

/// Environment override for [`resolve_max_request_body_bytes`]. Useful for
/// users with an unusually large Codex transcript who do not want to edit
/// `config.json` or wait for the next config-aware server start.
const MAX_REQUEST_BODY_BYTES_ENV: &str = "LOOM_ROUTER_MAX_REQUEST_BODY_BYTES";

/// Body ceiling for OpenAI-compatible `/v1/chat/completions` requests.
///
/// These requests do not replay a Codex conversation, so they keep the
/// previous 16 MiB bound instead of inheriting the larger compaction limit.
const CHAT_COMPLETIONS_MAX_REQUEST_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Resolve the per-route body limit for Codex Responses and compaction.
///
/// A fixed 16 MiB limit turned out to be too small for real automatic
/// compaction payloads, which can carry a full multi-megabyte transcript
/// after JSON encoding and decompression. The default is deliberately
/// generous and is bounded by a hard ceiling so a bad config or environment
/// value cannot create an unbounded local allocation.
fn resolve_max_request_body_bytes(config: &SharedConfig) -> usize {
    let configured = config
        .try_read()
        .map(|cfg| cfg.max_request_body_bytes)
        .unwrap_or(DEFAULT_MAX_REQUEST_BODY_BYTES);
    let env = std::env::var(MAX_REQUEST_BODY_BYTES_ENV).ok();
    resolve_max_request_body_bytes_value(configured, env.as_deref())
}

fn resolve_max_request_body_bytes_value(configured: usize, env: Option<&str>) -> usize {
    let from_env = env.and_then(|raw| raw.trim().parse::<usize>().ok());
    let value = from_env.unwrap_or(configured);
    if value == 0 {
        DEFAULT_MAX_REQUEST_BODY_BYTES
    } else {
        value.min(MAX_REQUEST_BODY_BYTES_HARD_LIMIT)
    }
}

#[derive(Clone)]
struct ProxyCtx {
    config: SharedConfig,
    stats: SharedStats,
    key_pools: KeyPools,
    client: reqwest::Client,
    /// Routed-turn history shared across WebSocket connections. Routed
    /// providers are stateless, so each incremental follow-up turn replays
    /// the full item list; the cache is what lets that rebuild happen. It is
    /// connection-scoped for capacity reasons but *shared* because a Codex
    /// reconnect (idle timeout, network blip) creates a new WS session with
    /// the conversation's thread still alive — a per-session cache would
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

// ---------------------------------------------------------------------------
// Provider families (D3/D4): coarse grouping derived from the base URL,
// used for routing quirks (e.g. OpenRouter's unified reasoning object) and
// shared with the other modules via the `pub` API below.
// ---------------------------------------------------------------------------

/// The wire dialect one model is served in.
///
/// A provider's `protocol` is only the default. OpenCode puts three dialects
/// behind a single URL and key, so a model that names its own wins — and
/// anything untagged (every ordinary endpoint, and every model discovery
/// turned up before someone said otherwise) falls back to the provider's.
/// Apply the provider's upstream authentication to an outgoing request.
/// The scheme follows the wire protocol, not the URL family: gateways like
/// OpenCode Zen speak the Anthropic protocol (and expect `x-api-key`) on a
/// non-Anthropic URL — and they do it for some of their models only, which
/// is why the scheme is resolved per model. `None` (catalog fetches, balance
/// probes: requests that belong to no model) uses the provider's own.
pub fn router(config: SharedConfig, stats: SharedStats) -> Router {
    router_with_pools(config, stats, KeyPools::new())
}

pub fn router_with_pools(config: SharedConfig, stats: SharedStats, key_pools: KeyPools) -> Router {
    // Materialize the local token at startup so the first request never
    // pays (or races) initialization.
    let _ = local_token();
    let max_responses_body_bytes = resolve_max_request_body_bytes(&config);
    let ctx = ProxyCtx {
        config,
        stats,
        key_pools,
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
            get(handle_responses_ws)
                .post(handle_responses)
                .layer(DefaultBodyLimit::max(max_responses_body_bytes)),
        )
        .route(
            "/v1/responses/compact",
            post(handle_compact).layer(DefaultBodyLimit::max(max_responses_body_bytes)),
        )
        .route(
            "/v1/chat/completions",
            post(handle_chat_completions).layer(DefaultBodyLimit::max(
                CHAT_COMPLETIONS_MAX_REQUEST_BODY_BYTES,
            )),
        )
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

/// Who served one turn: the identity every recorder needs, carried together.
///
/// These five used to travel as positional arguments, which left `key_id` and
/// `finish_reason` adjacent bare `Option`s that swap without the compiler
/// noticing, and pushed the recorders past clippy's argument limit.
#[derive(Clone)]
struct Turn {
    provider: String,
    model: String,
    transport: &'static str,
    started: Option<std::time::Instant>,
    key_id: Option<String>,
}

fn routed_stats_model(provider: &Provider, upstream_model: &str) -> String {
    format!("{}/{}", provider.id, upstream_model)
}

impl Turn {
    fn new(
        provider: &str,
        model: &str,
        transport: &'static str,
        started: Option<std::time::Instant>,
    ) -> Self {
        Self {
            provider: provider.to_string(),
            model: model.to_string(),
            transport,
            started,
            key_id: None,
        }
    }

    fn with_key(mut self, key_id: Option<&str>) -> Self {
        self.key_id = key_id.map(str::to_string);
        self
    }

    fn latency_ms(&self) -> Option<u64> {
        self.started.map(|s| s.elapsed().as_millis() as u64)
    }
}

/// Record a completed turn's usage in the background (SQLite insert).
fn record_usage_with_kind(
    stats: &SharedStats,
    turn: &Turn,
    usage: &Value,
    visual_assistance: Option<&VisualAssistanceMetadata>,
    kind: &str,
    finish_reason: Option<String>,
) {
    if usage.is_null() {
        return;
    }
    let latency_ms = turn.latency_ms();
    let entry = if let Some(key_id) = turn.key_id.as_deref() {
        crate::stats::RequestEntry::ok_with_key(
            &turn.provider,
            &turn.model,
            turn.transport,
            latency_ms,
            key_id,
            usage,
        )
    } else {
        let Some(entry) = crate::stats::RequestEntry::ok(
            &turn.provider,
            &turn.model,
            turn.transport,
            latency_ms,
            usage,
        ) else {
            return;
        };
        entry
    };
    let entry = entry
        .with_kind(kind)
        .with_visual_assistance(visual_assistance.cloned())
        .with_finish_reason(finish_reason);
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
fn record_payload_usage_with_kind(
    stats: &SharedStats,
    turn: &Turn,
    wire_kind: UpstreamKind,
    payload: &Value,
    visual_assistance: Option<&VisualAssistanceMetadata>,
    log_kind: &str,
) -> bool {
    let Some(usage) = translate::normalize_usage(wire_kind, payload) else {
        return false;
    };
    let finish_reason = turn_finish_reason(payload);
    record_usage_with_kind(
        stats,
        turn,
        &usage,
        visual_assistance,
        log_kind,
        finish_reason,
    );
    true
}

/// How the turn ended.
///
/// A Chat Completions payload states it outright, so that is the upstream's
/// own word. A Responses payload does not carry one, so it is derived from
/// what the turn actually produced - the distinction that matters is whether
/// the model answered with a tool call or just talked and handed control back,
/// which is exactly the case where an agent announces an action and stops.
fn turn_finish_reason(payload: &Value) -> Option<String> {
    if let Some(reason) = payload
        .pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
    {
        return Some(reason.to_string());
    }
    let response = payload.get("response").unwrap_or(payload);
    let output = response.get("output").and_then(Value::as_array)?;
    let called_a_tool = output.iter().any(|item| {
        matches!(
            item.get("type").and_then(Value::as_str),
            Some("function_call") | Some("custom_tool_call") | Some("local_shell_call")
        )
    });
    let status = response
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("completed");
    Some(match (called_a_tool, status) {
        (true, _) => "tool_calls".to_string(),
        (false, "completed") => "stop".to_string(),
        (false, other) => other.to_string(),
    })
}

fn record_payload_usage(
    stats: &SharedStats,
    turn: &Turn,
    kind: UpstreamKind,
    payload: &Value,
    visual_assistance: Option<&VisualAssistanceMetadata>,
) -> bool {
    record_payload_usage_with_kind(stats, turn, kind, payload, visual_assistance, "request")
}

/// Record a failed turn (upstream error, routing failure) in the background.
fn record_failure(stats: &SharedStats, turn: &Turn, error: &str) {
    record_failure_with_visual(stats, turn, error, None);
}

fn record_failure_with_visual(
    stats: &SharedStats,
    turn: &Turn,
    error: &str,
    visual_assistance: Option<VisualAssistanceMetadata>,
) {
    let entry = crate::stats::RequestEntry::error(
        &turn.provider,
        &turn.model,
        turn.transport,
        turn.latency_ms(),
        error,
    )
    .with_visual_assistance(visual_assistance)
    .with_key_id(turn.key_id.clone());
    let stats = stats.clone();
    tokio::spawn(async move {
        stats.read().await.record_entry(entry);
    });
}

fn record_problem(stats: &SharedStats, turn: &Turn, kind: &str, error: &str) {
    let entry = crate::stats::RequestEntry::problem(
        &turn.provider,
        &turn.model,
        turn.transport,
        turn.latency_ms(),
        kind,
        error,
    );
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

/// Resolve `provider/model` (or a bare upstream id in native-slug mode) to
/// (provider, upstream model).
/// Borrows the provider from the config; callers clone only the single
/// resolved provider instead of the whole AppConfig (P1).
/// Map a legacy per-dialect OpenCode provider id to the merged one:
/// `opencode-go-chat`/`-claude`/`-responses` → `opencode-go`, and the Zen
/// equivalents → `opencode-zen`. Only when the merged provider still exists;
/// a provider the user repointed to a URL of their own is left alone.
/// Send a prepared JSON body upstream and return the raw response.
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

#[derive(Clone, Copy, PartialEq)]
enum WireApi {
    Responses,
    ChatCompletions,
}

impl WireApi {
    /// The translator dialect this wire speaks — the one mapping both routing
    /// paths must agree on, so it lives here instead of being re-matched at
    /// each call site.
    fn downstream(self) -> DownstreamKind {
        match self {
            WireApi::Responses => DownstreamKind::Responses,
            WireApi::ChatCompletions => DownstreamKind::ChatCompletions,
        }
    }
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
pub(super) fn upstream_unreachable_error(
    url: &str,
    error: &reqwest::Error,
    label: &str,
) -> anyhow::Error {
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
    dispatch::dispatch(ctx, headers, payload, WireApi::Responses)
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
    dispatch::dispatch(ctx, headers, payload, WireApi::ChatCompletions)
        .await
        .map_err(|error| structured_error_response(StatusCode::BAD_GATEWAY, error.to_string()))
}

/// Build the upstream request (path, body, upstream kind) for a routed
/// provider — the single translation pipeline shared by the HTTP `dispatch`
/// and the WS `ws_turn_events` paths (D2). Covers every
/// (provider protocol × downstream wire) combination, including
/// Responses-protocol + ChatCompletions-wire, which the WS path used to
/// miss.
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

/// Bridge one request of either wire to a single `claude -p` turn: flatten
/// to a chat transcript, render the prompt, run the CLI, and mint the
/// synthetic message id. Shared by the HTTP and WS bridges so the pipeline
/// shape lives in exactly one place.
async fn run_claude_turn(
    payload: &Value,
    upstream_model: &str,
    wire: WireApi,
) -> anyhow::Result<(crate::claude_cli::ClaudePrintResult, String)> {
    let messages = claude_turn_messages(payload, upstream_model, wire)?;
    let messages = messages.as_array().map(Vec::as_slice).unwrap_or_default();
    let result = if crate::claude_cli::messages_have_images(messages) {
        crate::claude_cli::run_print_turn_stream_json(messages, upstream_model, None).await?
    } else {
        let prompt = crate::claude_cli::render_prompt(messages);
        crate::claude_cli::run_print_turn(&prompt, upstream_model, None).await?
    };
    Ok((result, claude_turn_id()))
}

/// The transcript one turn feeds to the CLI, in either wire's shape.
fn claude_turn_messages(
    payload: &Value,
    upstream_model: &str,
    wire: WireApi,
) -> anyhow::Result<Value> {
    Ok(match wire {
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
    })
}

fn claude_turn_id() -> String {
    format!(
        "msg_cli_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    )
}

/// Prepare one turn's CLI input without running it, for the streaming bridge.
/// Picks the same protocol `run_claude_turn` would: stream-json carries image
/// blocks, a flat prompt covers everything else.
fn claude_turn_input(
    payload: &Value,
    upstream_model: &str,
    wire: WireApi,
) -> anyhow::Result<(crate::claude_cli::ClaudeTurnInput, String)> {
    let messages = claude_turn_messages(payload, upstream_model, wire)?;
    let messages = messages.as_array().map(Vec::as_slice).unwrap_or_default();
    let input = if crate::claude_cli::messages_have_images(messages) {
        crate::claude_cli::ClaudeTurnInput::StreamJson(crate::claude_cli::render_stream_json(
            messages,
        )?)
    } else {
        crate::claude_cli::ClaudeTurnInput::Text(crate::claude_cli::render_prompt(messages))
    };
    Ok((input, claude_turn_id()))
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
    translate::repair_tool_exchange_items(input);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_limit_zero_means_generous_default() {
        assert_eq!(
            resolve_max_request_body_bytes_value(0, None),
            DEFAULT_MAX_REQUEST_BODY_BYTES
        );
    }

    #[test]
    fn body_limit_honors_configured_value_and_clamps_hard_ceiling() {
        assert_eq!(
            resolve_max_request_body_bytes_value(1024 * 1024, None),
            1024 * 1024
        );
        assert_eq!(
            resolve_max_request_body_bytes_value(MAX_REQUEST_BODY_BYTES_HARD_LIMIT + 1, None),
            MAX_REQUEST_BODY_BYTES_HARD_LIMIT
        );
    }

    #[test]
    fn body_limit_env_override_wins_and_invalid_env_falls_back_to_config() {
        assert_eq!(
            resolve_max_request_body_bytes_value(1024 * 1024, Some("4096")),
            4096
        );
        assert_eq!(
            resolve_max_request_body_bytes_value(42, Some("not-a-number")),
            42
        );
    }
}
