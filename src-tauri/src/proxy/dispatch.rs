use super::*;
use anyhow::anyhow;
use axum::{
    body::Body,
    http::{HeaderMap, StatusCode},
    response::Response,
};
use serde_json::Value;

/// Execute the pure route plan and keep fallback retry policy at the HTTP
/// boundary, where it can distinguish upstream failures from visual failures.
pub(super) async fn dispatch(
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

/// Run a routed HTTP turn, including visual preparation, upstream translation,
/// status preservation, response headers, and usage/failure accounting.
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
    if super::routing::codex_request_kind(payload).as_deref() == Some("compaction") {
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
        let fit = super::realtime::clamp_routed_input(
            ctx,
            provider,
            upstream_model,
            &prepared_payload,
            items,
        )
        .await;
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
    let (result, id) = super::run_claude_turn(payload, upstream_model, wire).await?;
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

fn translate_json(
    upstream_kind: UpstreamKind,
    downstream_kind: DownstreamKind,
    json: &Value,
    model: &str,
    payload: &Value,
) -> Value {
    match (upstream_kind, downstream_kind) {
        (UpstreamKind::OpenAiChat, DownstreamKind::Responses) => {
            let mut response = translate::chat_completion_to_responses(json, model);
            if let Some(output) = response.get_mut("output").and_then(Value::as_array_mut) {
                translate::apply_namespaces_to_output(
                    output,
                    &translate::tool_namespace_map(payload),
                );
                translate::unwrap_freeform_to_output(
                    output,
                    &translate::freeform_tool_names(payload),
                );
            }
            response
        }
        (UpstreamKind::Anthropic, DownstreamKind::Responses) => {
            let mut response = translate::anthropic_to_responses(json, model);
            if let Some(output) = response.get_mut("output").and_then(Value::as_array_mut) {
                translate::apply_namespaces_to_output(
                    output,
                    &translate::tool_namespace_map(payload),
                );
                translate::unwrap_freeform_to_output(
                    output,
                    &translate::freeform_tool_names(payload),
                );
            }
            response
        }
        (UpstreamKind::Anthropic, DownstreamKind::ChatCompletions) => {
            translate::anthropic_to_chat(json, model)
        }
        (UpstreamKind::Responses, DownstreamKind::Responses) => {
            let mut response = json.clone();
            if let Some(output) = response.get_mut("output").and_then(Value::as_array_mut) {
                translate::unwrap_freeform_to_output(
                    output,
                    &translate::freeform_tool_names(payload),
                );
            }
            response
        }
        _ => json.clone(),
    }
}

/// Forward non-routed models unchanged to the native Codex backend, retaining
/// its status and headers/body handling independently from routed providers.
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

/// Build label for log diagnostics. Kept in the row text so reports from
/// different installed builds are comparable without a separate schema field.
pub(super) const BUILD_LABEL: &str = concat!("loom-router/", env!("CARGO_PKG_VERSION"));

/// Codex remote compaction v2 is a normal Responses turn whose input ends in
/// `{"type":"compaction_trigger"}`. Native GPT goes to the ChatGPT backend,
/// which returns the encrypted compaction item; routed providers cannot, so
/// this path asks the routed model for a plain summary and wraps it in the
/// transparent envelope the translator can decode on the next replay.
pub(super) fn is_remote_compaction_v2(payload: &Value) -> bool {
    payload
        .get("input")
        .and_then(Value::as_array)
        .and_then(|items| items.last())
        .is_some_and(|item| item.get("type").and_then(Value::as_str) == Some("compaction_trigger"))
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
        // `item_parts_mut` also reaches a tool result's `output`, where the
        // view_image tool puts its screenshot. Reading only `content` left a
        // 2.2MB base64 image in the summarizer's input - one item larger than
        // the whole window, so compaction could never succeed and the
        // conversation became unusable.
        if let Some(parts) = item_parts_mut(&mut item, WireApi::Responses) {
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
pub(super) fn fit_compaction_input(
    provider: &Provider,
    upstream_model: &str,
    payload: &Value,
) -> Value {
    let mut prepared = build_compaction_payload(payload);
    let Some(items) = prepared.get("input").and_then(Value::as_array).cloned() else {
        return prepared;
    };
    let budget = (crate::codex::context_window_for(provider, upstream_model).window as usize)
        .saturating_sub(super::realtime::CONTEXT_RESERVE_TOKENS * 2);
    let estimated = super::realtime::estimate_tokens(&items)
        + super::realtime::estimate_non_input_tokens(&prepared, &items);
    if estimated <= budget {
        return prepared;
    }

    let (prompt, history) = items.split_last().expect("compaction prompt is appended");
    let mut transcript = super::realtime::render_items_as_text(history);
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

pub(super) async fn routed_compaction_events(
    ctx: &ProxyCtx,
    provider: &Provider,
    upstream_model: &str,
    payload: &Value,
) -> anyhow::Result<super::realtime::WsEvents> {
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
