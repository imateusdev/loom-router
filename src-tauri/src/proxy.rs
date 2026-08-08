//! Local proxy: receives requests from the coding agent and dispatches
//! them to the right provider based on the `model` field, translating
//! both the request and the response (including SSE streams).
//!
//! Endpoints (all bound to 127.0.0.1):
//!   POST /v1/responses        — Codex Responses API
//!   POST /v1/chat/completions — OpenAI-compatible clients
//!   GET  /health              — liveness for the UI

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
use std::fmt;
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

/// The wire dialect one model is served in.
///
/// A provider's `protocol` is only the default. OpenCode puts three dialects
/// behind a single URL and key, so a model that names its own wins — and
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
/// non-Anthropic URL — and they do it for some of their models only, which
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
    visual_assistance: Option<&VisualAssistanceMetadata>,
) {
    if usage.is_null() {
        return;
    }
    let latency_ms = started.map(|s| s.elapsed().as_millis() as u64);
    let Some(entry) = crate::stats::RequestEntry::ok(provider, model, transport, latency_ms, usage)
    else {
        return;
    };
    let entry = entry.with_visual_assistance(visual_assistance.cloned());
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
/// calls them) lives in exactly one module. A payload with no usage yet —
/// the normal case for every streaming frame before the terminal one — is
/// simply not recorded.
#[allow(clippy::too_many_arguments)] // why: one flat recorder all dialects share
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
    let Some(usage) = translate::normalize_usage(kind, payload) else {
        return false;
    };
    record_usage(
        stats,
        provider,
        model,
        transport,
        started,
        &usage,
        visual_assistance,
    );
    true
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
    // Per model, not per provider: OpenCode serves Chat Completions,
    // Anthropic Messages and Responses behind one URL.
    match (model_protocol(provider, upstream_model), wire) {
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
// carries `client_metadata["x-codex-turn-metadata"]` — a JSON *string* whose
// `request_kind` field is "turn" (main turns), "prewarm" (connection warmup
// probes), "compaction" (inline compaction), or "memory" (memory
// consolidation). The same JSON is mirrored in the `x-codex-turn-metadata`
// HTTP header (a bounded compatibility projection), which also rides the WS
// upgrade request. A request is a side call only when this marker is present
// and request_kind is something other than "turn".
//
// This is deliberately conservative:
// - False negatives (a side call we miss) fall through to the original
//   destination — the pre-feature behavior. Known miss: thread-title
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
    tracing::warn!(%model, fallback_provider = %provider.id, "side-call fallback failed; retrying original destination");
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
    let mut prepared_payload = payload.clone();
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

    // Same-format pass-through: the payload needs no translation, but usage
    // still has to be recorded.
    //
    // This branch used to return before any tap ran, so every request from
    // an OpenAI-compatible client to an OpenAI-compatible provider was
    // missing from the dashboard — the single largest gap in the stats,
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
/// synthesized in Anthropic's wire shape — which the rest of the pipeline
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
    payload: Value,
) -> anyhow::Result<Response> {
    let upstream = native_send(ctx, wire, headers, &payload).await?;
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
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

    // The thread may have passed through a routed model, whose reply the
    // translator had to give invented item ids. The native backend resolves
    // ids it issued itself and 404s the rest, so they come out here.
    let mut payload = payload.clone();
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
        while self.order.len() > WS_HISTORY_MAX_ENTRIES || self.total_bytes > WS_HISTORY_MAX_BYTES {
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
            match resolve_effective(&cfg, &model, &payload, Some(&headers)) {
                EffectiveRoute::Routed {
                    provider,
                    upstream_model,
                    ..
                } => Some((provider, upstream_model)),
                EffectiveRoute::Native => None,
            }
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
                                // Canonical Responses frames on this transport.
                                record_payload_usage(
                                    &ctx.stats,
                                    &label,
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

    let route = {
        let cfg = ctx.config.read().await;
        resolve_effective(&cfg, &model, &payload, Some(headers))
    };
    match route {
        // Native GPT model: relay the backend's SSE events as WS frames.
        EffectiveRoute::Native => ws_native_events(ctx, headers, payload).await,
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
                return attempt;
            }
            // A failed fallback must never break a side call: retry against
            // the request's original destination (same rule as HTTP).
            tracing::warn!(%model, fallback_provider = %provider.id, "side-call fallback failed; retrying original destination");
            let original = {
                let cfg = ctx.config.read().await;
                resolve(&cfg, &model).map(|(p, m)| (p.clone(), m))
            };
            match original {
                Ok((p, upstream_model)) => {
                    if p.id == crate::providers::CLAUDE_CODE_PROVIDER_ID {
                        ws_claude_cli_events(ctx, &p, &upstream_model, &model, &payload).await
                    } else {
                        ws_routed_events(ctx, &p, &upstream_model, &model, &payload).await
                    }
                }
                Err(_) => ws_native_events(ctx, headers, payload).await,
            }
        }
    }
}

/// Relay a native-model turn: the backend's SSE events become WS frames.
async fn ws_native_events(
    ctx: &ProxyCtx,
    headers: &HeaderMap,
    payload: Value,
) -> anyhow::Result<futures::stream::BoxStream<'static, Result<Value, String>>> {
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
    let (path, body, upstream_kind) =
        build_upstream(provider, payload, upstream_model, WireApi::Responses)?;
    // The namespace map is derived from the request being sent, because Chat
    // Completions has no field to carry a tool's namespace and Codex resolves
    // a namespace-less call against `functions` — where none of the
    // collaboration or MCP handlers live.
    let translator = match upstream_kind {
        UpstreamKind::Responses => None,
        kind => Some((
            kind,
            model.to_string(),
            translate::tool_namespace_map(payload),
            translate::freeform_tool_names(payload),
        )),
    };
    let upstream = send(ctx, provider, path, &body).await?;
    let status = upstream.status();
    if !status.is_success() {
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
    _ctx: &ProxyCtx,
    _provider: &Provider,
    upstream_model: &str,
    model: &str,
    payload: &Value,
) -> anyhow::Result<futures::stream::BoxStream<'static, Result<Value, String>>> {
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
                    // finalize(), which emitted `response.completed` — the
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

    /// The OpenCode shape: one URL, one key, three dialects — the dialect
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
                // Turned up by discovery, never given a dialect.
                model("something-new", None),
            ],
            enabled: true,
        }
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
}
