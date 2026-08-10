use super::dispatch::BUILD_LABEL;
use super::streaming::sse_values_stream;
use super::*;

/// Headers relayed to the native backend so ChatGPT auth and session
/// telemetry keep working through the proxy.
pub(super) const NATIVE_FORWARD_HEADERS: &[&str] = &[
    "authorization",
    "chatgpt-account-id",
    "openai-beta",
    "originator",
    "session_id",
    "user-agent",
    "accept",
    "version",
];

pub(super) async fn native_send(
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
pub(super) fn is_trusted_ws_origin(headers: &HeaderMap) -> bool {
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

pub(super) async fn handle_responses_ws(
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
/// entry it was built on — one entry per conversation, never O(n²) growth.
/// The cache is shared across connections (a reconnect keeps the thread
/// alive) and capped by entry count and total serialized size, evicting the
/// oldest first. The entry just stored is never evicted: it is what the next
/// turn's `previous_response_id` resolves against, and dropping it would
/// reset the conversation to a delta-only turn. A single entry may therefore
/// exceed the byte budget — a long conversation keeps its newest turn even
/// when that one entry alone is larger than the cap.
///
/// The byte budget is large on purpose: each live conversation contributes
/// exactly one entry (the rebuild input of its newest turn, which is a full
/// transcription of the conversation up to that point). At ~304k tokens that
/// serializes to ~1.2MB, so several long conversations must coexist without
/// evicting each other — a small cap turns a second long conversation into a
/// context reset for the first.
pub(super) const WS_HISTORY_MAX_ENTRIES: usize = 100;
pub(super) const WS_HISTORY_MAX_BYTES: usize = 16 * 1024 * 1024;

pub(super) struct WsHistory {
    map: std::collections::HashMap<String, Vec<Value>>,
    /// Insertion order with per-entry serialized size, for FIFO eviction.
    pub(super) order: VecDeque<(String, usize)>,
    total_bytes: usize,
}

impl WsHistory {
    pub(super) fn new() -> Self {
        Self {
            map: std::collections::HashMap::new(),
            order: VecDeque::new(),
            total_bytes: 0,
        }
    }

    pub(super) fn get(&self, id: &str) -> Option<&Vec<Value>> {
        self.map.get(id)
    }

    /// Record a completed turn under `rid`. `prev` is the response id this
    /// turn was rebuilt from (when it was a follow-up): its entry is fully
    /// contained in the new one, so it is dropped to keep exactly one entry
    /// per conversation.
    pub(super) fn insert(&mut self, rid: String, record: Vec<Value>, prev: Option<&str>) {
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

    pub(super) fn remove(&mut self, id: &str) {
        if let Some(pos) = self.order.iter().position(|(rid, _)| rid == id) {
            if let Some((_, size)) = self.order.remove(pos) {
                self.total_bytes = self.total_bytes.saturating_sub(size);
            }
        }
        self.map.remove(id);
    }
}

/// Conservative estimate of the token count of a rebuilt Responses input item
/// list. The proxy has no tokenizer; chars/4 is the same heuristic opencode's
/// v2 uses (see packages/core/src/util/token.ts). Only used to decide where
/// to clamp, so an off-by-a-factor just shifts the cut, never breaks a request.
pub(super) fn estimate_tokens(items: &[Value]) -> usize {
    items.iter().map(|v| v.to_string().len() / 3).sum()
}

/// Same heuristic for the parts of a Responses payload outside the input
/// items: instructions, tool definitions, structured fields and delimiters.
/// Codex's own context accounting does not always include these, so the proxy
/// subtracts them from the budget before deciding what fits.
pub(super) fn estimate_non_input_tokens(payload: &Value, items: &[Value]) -> usize {
    (payload.to_string().len() / 3).saturating_sub(estimate_tokens(items))
}

/// How many tokens to keep free for the destination model's reply when
/// clamping a routed turn, mirroring opencode's COMPACTION_BUFFER (20k).
/// Without the reserve, a full-window input is rejected before the model
/// can answer.
pub(super) const CONTEXT_RESERVE_TOKENS: usize = 20_000;

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

/// Cut a conversation down to fit the destination model's window, dropping
/// the OLDEST items and never touching the recent tail (the model needs it
/// to answer). Returns the surviving items and the dropped ones (empty when
/// nothing was removed).
#[cfg(test)]
pub(super) fn clamp_to_window(items: Vec<Value>, window_tokens: i64) -> (Vec<Value>, Vec<Value>) {
    clamp_to_window_with_overhead(items, window_tokens, 0)
}

/// Flatten Responses-wire input items (role + content blocks) to plain text.
/// `render_prompt` cannot be used directly: it reads string `content`, while
/// these items carry `content` as an array of blocks.
pub(super) fn render_items_as_text(items: &[Value]) -> String {
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
/// the timeout — the caller then degrades to the plain truncation marker.
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
pub(super) async fn clamp_routed_input(
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
pub(super) fn rebuild_input(
    history: &WsHistory,
    prev: Option<&str>,
    delta: Vec<Value>,
) -> Vec<Value> {
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
pub(super) fn replace_incremental_input(payload: &mut Value, input: Vec<Value>) {
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
pub(super) type WsEvents = futures::stream::BoxStream<'static, Result<Value, String>>;
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
    if super::dispatch::is_remote_compaction_v2(payload) {
        return super::dispatch::routed_compaction_events(ctx, provider, upstream_model, payload)
            .await;
    }
    if super::routing::codex_request_kind(payload).as_deref() == Some("compaction") {
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
    let translator = super::streaming::ws_translator_config(
        provider,
        upstream_model,
        model,
        upstream_kind,
        payload,
    );
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
    if super::dispatch::is_remote_compaction_v2(payload) {
        return super::dispatch::routed_compaction_events(ctx, provider, upstream_model, payload)
            .await;
    }
    if super::routing::codex_request_kind(payload).as_deref() == Some("compaction") {
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
