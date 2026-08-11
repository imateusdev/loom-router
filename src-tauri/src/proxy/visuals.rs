use super::*;
use crate::config::AppConfig;
use crate::stats::{SharedStats, VisualAssistanceMetadata, VisualImageProvenance};
use crate::visual::{self, ImagePart};
use anyhow::{anyhow, bail};
use serde_json::{json, Value};
use std::fmt;

#[derive(Debug)]
pub(super) struct VisualAssistanceFailure(String);

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

pub(super) struct PayloadImagePart {
    // why: the tool-result scan test asserts which item the image came from,
    // which is the whole point of reaching past `content` into `output`.
    pub(super) message_index: usize,
    pub(super) image: ImagePart,
}

/// Content parts of one payload item. A Responses message keeps them under
/// `content`, but a tool result keeps them under `output` - and Codex's
/// view_image tool answers there, so a scan that reads only `content` never
/// sees the image and lets it through to an upstream that rejects it.
fn item_parts(item: &Value, wire: WireApi) -> Option<&Vec<Value>> {
    if let Some(parts) = item.get("content").and_then(Value::as_array) {
        return Some(parts);
    }
    match wire {
        WireApi::Responses => item.get("output").and_then(Value::as_array),
        WireApi::ChatCompletions => None,
    }
}

/// Mutable twin of `item_parts`.
pub(super) fn item_parts_mut(item: &mut Value, wire: WireApi) -> Option<&mut Vec<Value>> {
    let field = if item.get("content").and_then(Value::as_array).is_some() {
        "content"
    } else if matches!(wire, WireApi::Responses)
        && item.get("output").and_then(Value::as_array).is_some()
    {
        "output"
    } else {
        return None;
    };
    item.get_mut(field).and_then(Value::as_array_mut)
}

/// A tool result carries no role, so it cannot be checked for `role: user`.
fn is_tool_output_item(item: &Value) -> bool {
    matches!(
        item.get("type").and_then(Value::as_str),
        Some("function_call_output") | Some("custom_tool_call_output")
    )
}

/// Extract client-supplied image references without retaining or logging their
/// bytes. Both wire formats keep image URLs inside content arrays.
pub(super) fn image_parts_in_payload(payload: &Value, wire: WireApi) -> Vec<PayloadImagePart> {
    let messages = match wire {
        WireApi::Responses => payload.get("input").and_then(Value::as_array),
        WireApi::ChatCompletions => payload.get("messages").and_then(Value::as_array),
    };
    messages
        .into_iter()
        .flatten()
        .enumerate()
        .flat_map(|(message_index, message)| {
            item_parts(message, wire)
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

pub(super) fn validate_image_part_roles(payload: &Value, wire: WireApi) -> anyhow::Result<()> {
    let messages = match wire {
        WireApi::Responses => payload.get("input").and_then(Value::as_array),
        WireApi::ChatCompletions => payload.get("messages").and_then(Value::as_array),
    };
    let image_type = match wire {
        WireApi::Responses => "input_image",
        WireApi::ChatCompletions => "image_url",
    };
    for message in messages.into_iter().flatten() {
        let has_image = item_parts(message, wire).is_some_and(|content| {
            content
                .iter()
                .any(|part| part.get("type").and_then(Value::as_str) == Some(image_type))
        });
        let allowed = message.get("role").and_then(Value::as_str) == Some("user")
            || is_tool_output_item(message);
        if has_image && !allowed {
            bail!("visual assistance only supports image parts in user messages and tool results");
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
pub(super) fn model_supports_vision(config: &AppConfig, slug: &str) -> anyhow::Result<bool> {
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
pub(super) fn enrich_payload_with_evidence(
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
        let allowed = message.get("role").and_then(Value::as_str) == Some("user")
            || is_tool_output_item(message);
        let Some(content) = item_parts_mut(message, wire) else {
            continue;
        };
        let has_image = content
            .iter()
            .any(|part| part.get("type").and_then(Value::as_str) == Some(image_type));
        if !has_image {
            continue;
        }
        if !allowed {
            bail!("visual assistance only supports image parts in user messages and tool results");
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
pub(super) async fn prepare_visual_assistance(
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

/// Safe suffix describing how the visual provider failed. A status code and a
/// duration carry none of the image, prompt or credentials, and without them
/// the redacted message cannot tell a provider refusal (HTTP 4xx/5xx) from a
/// network blip that never got a response at all.
pub(super) fn visual_failure_detail(metadata: Option<&VisualAssistanceMetadata>) -> String {
    let Some(metadata) = metadata else {
        return String::new();
    };
    let Some(last) = metadata.attempts.last() else {
        return String::new();
    };
    let outcome = match last.status {
        Some(status) => format!("HTTP {status}"),
        None => "no response".to_string(),
    };
    let attempts = metadata.attempts.len();
    if attempts > 1 {
        format!(
            " ({outcome} after {}ms, {attempts} attempts)",
            last.duration_ms
        )
    } else {
        format!(" ({outcome} after {}ms)", last.duration_ms)
    }
}

/// Shared HTTP/WS visual-preparation failure path. Persist only the redacted
/// summary before returning the same safe diagnostic to the gateway caller.
pub(super) fn visual_preparation_failure(
    stats: &SharedStats,
    provider: &str,
    model: &str,
    transport: &'static str,
    started: std::time::Instant,
    error: &anyhow::Error,
) -> VisualAssistanceFailure {
    let visual_assistance = visual_failure_metadata(error);
    let error = format!(
        "{}{}",
        redacted_visual_assistance_error(error),
        visual_failure_detail(visual_assistance.as_ref()),
    );
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
