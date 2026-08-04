//! Protocol translation between wire formats.
//!
//! Milestone 1: best-effort, non-streaming-friendly conversion of the most
//! common fields. Milestone 2 will add full SSE translation (Responses API
//! events <-> chat.completion.chunk) and tool-call shape preservation.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

/// Responses API payload -> Chat Completions payload.
pub fn responses_to_chat(payload: &Value, model: &str) -> Result<Value> {
    let mut messages: Vec<Value> = Vec::new();

    if let Some(instructions) = payload.get("instructions").and_then(Value::as_str) {
        messages.push(json!({"role": "system", "content": instructions}));
    }

    match payload.get("input") {
        Some(Value::String(text)) => {
            messages.push(json!({"role": "user", "content": text}));
        }
        Some(Value::Array(items)) => {
            for item in items {
                messages.push(convert_response_item(item)?);
            }
        }
        _ => return Err(anyhow!("Responses payload has no usable 'input'")),
    }

    let mut out = json!({
        "model": model,
        "messages": messages,
        "stream": payload.get("stream").and_then(Value::as_bool).unwrap_or(false),
    });
    if let Some(max) = payload.get("max_output_tokens") {
        out["max_tokens"] = max.clone();
    }
    Ok(out)
}

fn convert_response_item(item: &Value) -> Result<Value> {
    let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
    match item.get("content") {
        Some(Value::String(s)) => Ok(json!({"role": role, "content": s})),
        Some(Value::Array(parts)) => {
            let text: String = parts
                .iter()
                .filter_map(|p| {
                    if p.get("type").and_then(Value::as_str) == Some("input_text") {
                        p.get("text").and_then(Value::as_str).map(str::to_string)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("");
            Ok(json!({"role": role, "content": text}))
        }
        _ => Ok(json!({"role": role, "content": ""})),
    }
}

/// Chat Completions payload -> Anthropic Messages payload.
pub fn chat_to_anthropic(payload: &Value, model: &str) -> Result<Value> {
    let msgs = payload
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing 'messages'"))?;

    let mut system_parts: Vec<String> = Vec::new();
    let mut messages: Vec<Value> = Vec::new();
    for m in msgs {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
        let content = m.get("content").cloned().unwrap_or(Value::Null);
        if role == "system" {
            if let Some(s) = content.as_str() {
                system_parts.push(s.to_string());
            }
        } else {
            let role = if role == "assistant" { "assistant" } else { "user" };
            messages.push(json!({"role": role, "content": content}));
        }
    }

    let mut out = json!({
        "model": model,
        "messages": messages,
        "max_tokens": payload.get("max_tokens").cloned().unwrap_or(json!(8192)),
        "stream": payload.get("stream").and_then(Value::as_bool).unwrap_or(false),
    });
    if !system_parts.is_empty() {
        out["system"] = Value::String(system_parts.join("\n\n"));
    }
    Ok(out)
}
