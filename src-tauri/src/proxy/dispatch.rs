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
    let plan = {
        let config = ctx.config.read().await;
        resolve_effective(&config, &model, &payload, Some(&headers))
    };
    let EffectiveRoute::Routed {
        provider,
        upstream_model,
        from_fallback,
    } = plan
    else {
        return forward_native(&ctx, wire, &headers, payload).await;
    };

    let response = dispatch_routed(&ctx, &provider, &upstream_model, &model, &payload, wire).await;
    let visual_failure = response
        .as_ref()
        .err()
        .is_some_and(|error| error.downcast_ref::<VisualAssistanceFailure>().is_some());
    let failed = match &response {
        Ok(response) => !response.status().is_success(),
        Err(_) => true,
    };
    if visual_failure || !from_fallback || !failed {
        return response;
    }

    tracing::warn!(%model, fallback_provider = %provider.id, "side-call fallback failed; retrying original destination");
    let original = {
        let config = ctx.config.read().await;
        resolve(&config, &model)
            .map(|(provider, upstream_model)| (provider.clone(), upstream_model))
    };
    match original {
        Ok((provider, upstream_model)) => {
            dispatch_routed(&ctx, &provider, &upstream_model, &model, &payload, wire).await
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

    if provider.id == crate::providers::CLAUDE_CODE_PROVIDER_ID {
        return dispatch_claude_cli(ctx, provider, upstream_model, model, payload, wire).await;
    }

    tracing::info!(%model, provider = %provider.id, %upstream_model, stream = wants_stream, "routing request");
    let (path, body, upstream_kind) =
        build_upstream(provider, &prepared_payload, upstream_model, wire)?;
    let upstream = send(ctx, provider, path, &body).await?;
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);

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
            Err(error) => {
                tracing::warn!(error = %error, len = raw.len(), "pass-through body was not JSON; usage not recorded");
            }
        }
        return Ok(Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(Body::from(raw))?);
    }

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
