//! Protocol translation between wire formats, including streaming.
//!
//! Supported conversions:
//!   Requests : Responses API -> Chat Completions -> Anthropic Messages
//!   Responses: Chat Completions -> Responses API (JSON + SSE)
//!              Anthropic Messages -> Responses API (JSON + SSE)
//!              Anthropic Messages -> Chat Completions (JSON + SSE)
//!
//! Tool calls are translated in both directions; reasoning blocks and
//! provider-specific extensions are dropped (best effort).

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Request translation
// ---------------------------------------------------------------------------

/// Responses API payload -> Chat Completions payload.
pub fn responses_to_chat(payload: &Value, model: &str, unified_reasoning: bool) -> Result<Value> {
    let mut messages: Vec<Value> = Vec::new();

    if let Some(instructions) = payload.get("instructions").and_then(Value::as_str) {
        messages.push(json!({"role": "system", "content": instructions}));
    }

    match payload.get("input") {
        Some(Value::String(text)) => {
            messages.push(json!({"role": "user", "content": text}));
        }
        Some(Value::Array(items)) => {
            // DeepSeek thinking mode: when tools are in play, prior
            // reasoning_content MUST be sent back or the API returns 400.
            // Responses input carries it as reasoning items; collect the
            // text and re-attach it to the next assistant message.
            let mut pending_reasoning = String::new();
            for item in items {
                if item.get("type").and_then(Value::as_str) == Some("reasoning") {
                    if let Some(parts) = item.get("summary").and_then(Value::as_array) {
                        for p in parts {
                            if let Some(t) = p.get("text").and_then(Value::as_str) {
                                pending_reasoning.push_str(t);
                            }
                        }
                    }
                    continue;
                }
                convert_response_input_item(item, &mut messages, &mut pending_reasoning)?;
            }
        }
        _ => return Err(anyhow!("Responses payload has no usable 'input'")),
    }

    hoist_interleaved_system(&mut messages);

    let mut out = json!({
        "model": model,
        "messages": messages,
        "stream": payload.get("stream").and_then(Value::as_bool).unwrap_or(false),
    });
    if let Some(max) = payload.get("max_output_tokens") {
        out["max_tokens"] = max.clone();
    }
    if let Some(tools) = payload.get("tools").and_then(Value::as_array) {
        let chat_tools: Vec<Value> = tools
            .iter()
            .filter(|t| t.get("type").and_then(Value::as_str) == Some("function"))
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.get("name").cloned().unwrap_or(Value::Null),
                        "description": t.get("description").cloned().unwrap_or(Value::Null),
                        "parameters": t.get("parameters").cloned().unwrap_or(json!({})),
                    }
                })
            })
            .collect();
        if !chat_tools.is_empty() {
            out["tools"] = Value::Array(chat_tools);
        }
    }
    if let Some(tc) = payload.get("tool_choice") {
        out["tool_choice"] = tc.clone();
    }
    // Codex sends reasoning effort inside a Responses-only object. Each
    // upstream gets exactly ONE dialect (sending both makes OpenRouter
    // reject the request as conflicting):
    // - unified: reasoning:{effort} with Codex's native tiers
    //   (low/medium/high/xhigh) — OpenRouter normalizes them per model.
    // - mapped: reasoning_effort collapsed to low/high/max — Kimi and
    //   DeepSeek thinking only accept those.
    if let Some(effort) = payload
        .get("reasoning")
        .and_then(|r| r.get("effort"))
        .and_then(Value::as_str)
    {
        if unified_reasoning {
            out["reasoning"] = json!({"effort": effort});
        } else {
            let mapped = match effort {
                "xhigh" => "max",
                "medium" => "high",
                other => other,
            };
            out["reasoning_effort"] = Value::String(mapped.to_string());
        }
    }
    // Ask upstream to report usage on the final chunk when streaming.
    if out["stream"].as_bool() == Some(true) {
        out["stream_options"] = json!({"include_usage": true});
    }
    Ok(out)
}

/// Hoist `system` messages that landed inside a tool-call block.
///
/// Codex (macOS desktop) interleaves `developer` items between the assistant's
/// `function_call` items and their `function_call_output`s. Those become
/// `system` messages sitting between an `assistant(tool_calls)` message and
/// the `tool` messages that answer it. Strict upstreams (Console Go/DeepSeek)
/// reject that with "an assistant message with 'tool_calls' must be followed
/// by tool messages responding to each 'tool_call_id'"; the system message is
/// hoisted to just before the assistant message so the tool sequence stays
/// contiguous.
fn hoist_interleaved_system(messages: &mut Vec<Value>) {
    let is_tool_msg = |m: &Value| m.get("role").and_then(Value::as_str) == Some("tool");
    let is_system = |m: &Value| m.get("role").and_then(Value::as_str) == Some("system");
    let has_calls = |m: &Value| m.get("tool_calls").is_some();

    let mut i = 0;
    while i < messages.len() {
        if has_calls(&messages[i]) {
            let mut j = i + 1;
            let mut hoisted: Vec<Value> = Vec::new();
            while j < messages.len() {
                if is_tool_msg(&messages[j]) {
                    j += 1;
                } else if is_system(&messages[j]) {
                    hoisted.push(messages.remove(j));
                } else {
                    break;
                }
            }
            if !hoisted.is_empty() {
                for (k, sys) in hoisted.into_iter().enumerate() {
                    messages.insert(i + k, sys);
                }
            }
        }
        i += 1;
    }
}

fn convert_response_input_item(
    item: &Value,
    messages: &mut Vec<Value>,
    pending_reasoning: &mut String,
) -> Result<()> {
    let item_type = item.get("type").and_then(Value::as_str);
    // Attach collected reasoning to the next assistant message, then clear.
    let take_reasoning = |msg: &mut Value, role: &str, pending: &mut String| {
        if role == "assistant" && !pending.is_empty() {
            msg["reasoning_content"] = Value::String(std::mem::take(pending));
        }
    };
    // Plain message items carry role+content; typed items are tool IO.
    if item_type.is_none() || item_type == Some("message") {
        let raw_role = item.get("role").and_then(Value::as_str).unwrap_or("user");
        // Some providers (Kimi) reject the Responses-era "developer" role;
        // it is semantically the system prompt, so downgrade it.
        let role = if raw_role == "developer" {
            "system"
        } else {
            raw_role
        };
        match item.get("content") {
            Some(Value::String(s)) => {
                if !s.is_empty() {
                    let mut msg = json!({"role": role, "content": s});
                    take_reasoning(&mut msg, role, pending_reasoning);
                    messages.push(msg);
                }
            }
            Some(Value::Array(parts)) => {
                let mut text = String::new();
                let mut media: Vec<Value> = Vec::new();
                for p in parts {
                    match p.get("type").and_then(Value::as_str) {
                        Some("input_text") | Some("output_text") => {
                            if let Some(t) = p.get("text").and_then(Value::as_str) {
                                text.push_str(t);
                            }
                        }
                        // Vision: Responses input_image -> OpenAI image_url
                        // (data URLs pass straight through to Kimi K3).
                        Some("input_image") => {
                            let url = p
                                .get("image_url")
                                .and_then(Value::as_str)
                                .map(str::to_string);
                            if let Some(url) = url {
                                media.push(json!({
                                    "type": "image_url",
                                    "image_url": {"url": url},
                                }));
                            }
                        }
                        _ => {}
                    }
                }
                // Providers reject empty messages (e.g. Kimi: "the message
                // with role 'developer' must not be empty"). Codex emits
                // empty developer placeholders, so drop contentless items.
                if media.is_empty() {
                    if !text.is_empty() {
                        let mut msg = json!({"role": role, "content": text});
                        take_reasoning(&mut msg, role, pending_reasoning);
                        messages.push(msg);
                    }
                } else {
                    let mut content = vec![json!({"type": "text", "text": text})];
                    content.extend(media);
                    let mut msg = json!({"role": role, "content": content});
                    take_reasoning(&mut msg, role, pending_reasoning);
                    messages.push(msg);
                }
            }
            _ => {}
        }
        return Ok(());
    }
    match item_type {
        Some("function_call") => {
            let new_call = json!({
                "id": item.get("call_id").or(item.get("id")).cloned().unwrap_or(Value::Null),
                "type": "function",
                "function": {
                    "name": item.get("name").cloned().unwrap_or(Value::Null),
                    "arguments": item.get("arguments").cloned().unwrap_or(json!("")),
                }
            });
            // Parallel tool calls from one assistant turn arrive as
            // consecutive `function_call` items. Merge them into a single
            // assistant message: strict upstreams (e.g. Console Go) 400 a
            // request whose tool_calls message is split across consecutive
            // assistant messages. Reasoning is left pending (opening a fresh
            // message) rather than glued onto a call mid-batch.
            let merged = pending_reasoning.is_empty()
                && match messages.last_mut() {
                    Some(Value::Object(m))
                        if m.get("role").and_then(Value::as_str) == Some("assistant")
                            && m.get("content").is_none_or(Value::is_null)
                            && m.contains_key("tool_calls") =>
                    {
                        m.get_mut("tool_calls")
                            .and_then(Value::as_array_mut)
                            .map(|calls| {
                                calls.push(new_call.clone());
                                true
                            })
                            .unwrap_or(false)
                    }
                    _ => false,
                };
            if !merged {
                let mut msg = json!({
                    "role": "assistant",
                    "content": Value::Null,
                    "tool_calls": [new_call],
                });
                take_reasoning(&mut msg, "assistant", pending_reasoning);
                messages.push(msg);
            }
        }
        Some("function_call_output") => {
            messages.push(json!({
                "role": "tool",
                "tool_call_id": item.get("call_id").cloned().unwrap_or(Value::Null),
                "content": item.get("output").cloned().unwrap_or(json!("")),
            }));
        }
        _ => {} // reasoning and friends: dropped
    }
    Ok(())
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
        match role {
            "system" => {
                if let Some(s) = content.as_str() {
                    system_parts.push(s.to_string());
                }
            }
            "assistant" => {
                let mut blocks: Vec<Value> = Vec::new();
                if let Some(text) = content.as_str() {
                    if !text.is_empty() {
                        blocks.push(json!({"type": "text", "text": text}));
                    }
                }
                if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
                    for c in calls {
                        let f = c.get("function").cloned().unwrap_or(json!({}));
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": c.get("id").cloned().unwrap_or(Value::Null),
                            "name": f.get("name").cloned().unwrap_or(Value::Null),
                            "input": f.get("arguments")
                                .and_then(Value::as_str)
                                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                                .unwrap_or(json!({})),
                        }));
                    }
                }
                messages.push(json!({"role": "assistant", "content": blocks}));
            }
            "tool" => {
                messages.push(json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": m.get("tool_call_id").cloned().unwrap_or(Value::Null),
                        "content": content,
                    }]
                }));
            }
            _ => messages.push(json!({"role": "user", "content": content})),
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
    if let Some(tools) = payload.get("tools").and_then(Value::as_array) {
        let anth_tools: Vec<Value> = tools
            .iter()
            .filter_map(|t| t.get("function"))
            .map(|f| {
                json!({
                    "name": f.get("name").cloned().unwrap_or(Value::Null),
                    "description": f.get("description").cloned().unwrap_or(Value::Null),
                    "input_schema": f.get("parameters").cloned().unwrap_or(json!({"type":"object"})),
                })
            })
            .collect();
        if !anth_tools.is_empty() {
            out["tools"] = Value::Array(anth_tools);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Non-streaming response translation
// ---------------------------------------------------------------------------

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn map_usage_chat(u: &Value) -> Value {
    // Surface automatic context-cache hits so clients can see the
    // discounted input share. OpenAI-style providers nest it under
    // prompt_tokens_details; DeepSeek reports flat prompt_cache_hit_tokens.
    let cached = u
        .pointer("/prompt_tokens_details/cached_tokens")
        .cloned()
        .or_else(|| u.get("prompt_cache_hit_tokens").cloned())
        .unwrap_or(json!(0));
    json!({
        "input_tokens": u.get("prompt_tokens").cloned().unwrap_or(json!(0)),
        "output_tokens": u.get("completion_tokens").cloned().unwrap_or(json!(0)),
        "total_tokens": u.get("total_tokens").cloned().unwrap_or(json!(0)),
        "input_tokens_details": {"cached_tokens": cached},
    })
}

fn map_usage_anthropic(u: &Value) -> Value {
    let input = u.get("input_tokens").cloned().unwrap_or(json!(0));
    let output = u.get("output_tokens").cloned().unwrap_or(json!(0));
    let total = input.as_u64().unwrap_or(0) + output.as_u64().unwrap_or(0);
    // Anthropic prompt caching: cache_read is the discounted share.
    let cached = u
        .get("cache_read_input_tokens")
        .cloned()
        .unwrap_or(json!(0));
    json!({
        "input_tokens": input,
        "output_tokens": output,
        "total_tokens": total,
        "input_tokens_details": {"cached_tokens": cached},
    })
}

/// Fill in the canonical shape for a usage object that already uses the
/// Responses field names but may omit `total_tokens` or the cache details.
/// Keeps the three dialects symmetric, so consumers never have to special-case
/// a missing key.
fn map_usage_responses(u: &Value) -> Value {
    let input = u.get("input_tokens").cloned().unwrap_or(json!(0));
    let output = u.get("output_tokens").cloned().unwrap_or(json!(0));
    let total = u
        .get("total_tokens")
        .cloned()
        .unwrap_or_else(|| json!(input.as_u64().unwrap_or(0) + output.as_u64().unwrap_or(0)));
    let cached = u
        .pointer("/input_tokens_details/cached_tokens")
        .cloned()
        .unwrap_or(json!(0));
    json!({
        "input_tokens": input,
        "output_tokens": output,
        "total_tokens": total,
        "input_tokens_details": {"cached_tokens": cached},
    })
}

/// Find the raw usage object inside an upstream payload, wherever that
/// upstream chose to put it.
///
/// Placement is not consistent across providers, so every known location is
/// tried in turn:
///   - `/response/usage` — Responses `response.completed` events;
///   - `/usage`          — the common case for every dialect;
///   - `/choices/*/usage` — Chat Completions streams from providers such as
///     Kimi, which attach usage to the final choice instead of the top level
///     (OpenAI itself only emits top-level usage, via `stream_options`).
fn locate_usage(kind: UpstreamKind, payload: &Value) -> Option<Value> {
    let non_null = |v: Option<&Value>| v.filter(|u| !u.is_null()).cloned();

    if let Some(u) = non_null(payload.pointer("/response/usage")) {
        return Some(u);
    }
    if let Some(u) = non_null(payload.get("usage")) {
        return Some(u);
    }
    if kind == UpstreamKind::OpenAiChat {
        return payload
            .get("choices")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find_map(|c| non_null(c.get("usage")));
    }
    None
}

/// Locate and normalize an upstream payload's usage into the canonical
/// Responses shape (`input_tokens`, `output_tokens`, `total_tokens`,
/// `input_tokens_details.cached_tokens`).
///
/// This is the single source of truth for usage dialects. The stats layer
/// and every pass-through path go through here rather than re-deriving
/// field names, so a provider quirk is fixed in exactly one place.
/// Returns `None` when the payload carries no usage yet — the normal case
/// for every streaming frame before the terminal one.
pub fn normalize_usage(kind: UpstreamKind, payload: &Value) -> Option<Value> {
    let raw = locate_usage(kind, payload)?;
    Some(match kind {
        UpstreamKind::OpenAiChat => map_usage_chat(&raw),
        UpstreamKind::Anthropic => map_usage_anthropic(&raw),
        // Already the canonical field names; only the optional keys are
        // filled in, so all three dialects return the same shape.
        UpstreamKind::Responses => map_usage_responses(&raw),
    })
}

/// Chat Completions JSON response -> Responses API response object.
pub fn chat_completion_to_responses(chat: &Value, model: &str) -> Value {
    let id = chat
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("resp_{}", uuid::Uuid::new_v4().simple()));
    let mut output: Vec<Value> = Vec::new();
    if let Some(choice) = chat
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
    {
        let msg = choice.get("message").cloned().unwrap_or(json!({}));
        // Thinking models (Kimi) return reasoning_content alongside content.
        if let Some(thinking) = msg.get("reasoning_content").and_then(Value::as_str) {
            if !thinking.is_empty() {
                output.push(json!({
                    "id": format!("rs_{}", uuid::Uuid::new_v4().simple()),
                    "type": "reasoning",
                    "status": "completed",
                    "summary": [{"type": "summary_text", "text": thinking}],
                }));
            }
        }
        if let Some(text) = msg.get("content").and_then(Value::as_str) {
            if !text.is_empty() {
                output.push(message_item(text));
            }
        }
        if let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) {
            for c in calls {
                let f = c.get("function").cloned().unwrap_or(json!({}));
                output.push(function_call_item(
                    c.get("id").and_then(Value::as_str).unwrap_or(""),
                    f.get("name").and_then(Value::as_str).unwrap_or(""),
                    f.get("arguments").and_then(Value::as_str).unwrap_or(""),
                ));
            }
        }
    }
    json!({
        "id": id,
        "object": "response",
        "created_at": chat.get("created").and_then(Value::as_u64).unwrap_or_else(now_unix),
        "status": "completed",
        "model": model,
        "output": output,
        "usage": chat.get("usage").map(map_usage_chat).unwrap_or(Value::Null),
    })
}

/// Anthropic Messages JSON response -> Responses API response object.
pub fn anthropic_to_responses(msg: &Value, model: &str) -> Value {
    let id = msg
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("resp_{}", uuid::Uuid::new_v4().simple()));
    let mut output: Vec<Value> = Vec::new();
    if let Some(blocks) = msg.get("content").and_then(Value::as_array) {
        for b in blocks {
            match b.get("type").and_then(Value::as_str) {
                Some("text") => output.push(message_item(
                    b.get("text").and_then(Value::as_str).unwrap_or(""),
                )),
                Some("tool_use") => output.push(function_call_item(
                    b.get("id").and_then(Value::as_str).unwrap_or(""),
                    b.get("name").and_then(Value::as_str).unwrap_or(""),
                    &b.get("input").cloned().unwrap_or(json!({})).to_string(),
                )),
                _ => {}
            }
        }
    }
    json!({
        "id": id,
        "object": "response",
        "created_at": now_unix(),
        "status": "completed",
        "model": model,
        "output": output,
        "usage": msg.get("usage").map(map_usage_anthropic).unwrap_or(Value::Null),
    })
}

/// Anthropic Messages JSON response -> Chat Completions JSON response.
pub fn anthropic_to_chat(msg: &Value, model: &str) -> Value {
    let mut text = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    if let Some(blocks) = msg.get("content").and_then(Value::as_array) {
        for b in blocks {
            match b.get("type").and_then(Value::as_str) {
                Some("text") => text.push_str(b.get("text").and_then(Value::as_str).unwrap_or("")),
                Some("tool_use") => tool_calls.push(json!({
                    "id": b.get("id").cloned().unwrap_or(Value::Null),
                    "type": "function",
                    "function": {
                        "name": b.get("name").cloned().unwrap_or(Value::Null),
                        "arguments": b.get("input").cloned().unwrap_or(json!({})).to_string(),
                    }
                })),
                _ => {}
            }
        }
    }
    let mut message = json!({"role": "assistant", "content": text});
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    let finish = if message.get("tool_calls").is_some() {
        "tool_calls"
    } else {
        "stop"
    };
    let usage = msg.get("usage").cloned().unwrap_or(json!({}));
    json!({
        "id": msg.get("id").cloned().unwrap_or(json!("chatcmpl-loom")),
        "object": "chat.completion",
        "created": now_unix(),
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish,
        }],
        "usage": {
            "prompt_tokens": usage.get("input_tokens").cloned().unwrap_or(json!(0)),
            "completion_tokens": usage.get("output_tokens").cloned().unwrap_or(json!(0)),
            "total_tokens": usage.get("input_tokens").and_then(Value::as_u64).unwrap_or(0)
                + usage.get("output_tokens").and_then(Value::as_u64).unwrap_or(0),
        },
    })
}

fn message_item(text: &str) -> Value {
    json!({
        "id": format!("msg_{}", uuid::Uuid::new_v4().simple()),
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{"type": "output_text", "text": text, "annotations": []}],
    })
}

fn function_call_item(call_id: &str, name: &str, arguments: &str) -> Value {
    json!({
        "id": format!("fc_{}", uuid::Uuid::new_v4().simple()),
        "type": "function_call",
        "status": "completed",
        "call_id": call_id,
        "name": name,
        "arguments": arguments,
    })
}

// ---------------------------------------------------------------------------
// Streaming translation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamKind {
    OpenAiChat,
    Anthropic,
    /// Responses-format upstream: events pass through untouched, so no
    /// StreamTranslator is ever built with this kind (marker only).
    Responses,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownstreamKind {
    Responses,
    ChatCompletions,
}

/// One translated SSE frame ready to send downstream.
pub struct OutFrame {
    /// Event name for Responses-style frames; None for chat-style data-only.
    pub event: Option<String>,
    pub data: Value,
    pub done_marker: bool,
}

struct ToolCallState {
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
    opened: bool,
    output_index: usize,
}

/// Incremental translator state for one streaming request.
pub struct StreamTranslator {
    upstream: UpstreamKind,
    downstream: DownstreamKind,
    model: String,
    response_id: String,
    chat_id: String,
    created: u64,
    seq: u64,
    started: bool,
    completed: bool,
    // message item (text)
    msg_item_id: String,
    msg_open: bool,
    msg_output_index: usize,
    text_acc: String,
    // reasoning item (thinking summaries, e.g. Kimi reasoning_content)
    rs_item_id: String,
    rs_open: bool,
    rs_closed: bool,
    rs_text_acc: String,
    // tool calls keyed by upstream index
    tools: BTreeMap<usize, ToolCallState>,
    next_tool_output_index: usize,
    usage: Option<Value>,
    finish_reason: Option<String>,
}

impl StreamTranslator {
    pub fn new(upstream: UpstreamKind, downstream: DownstreamKind, model: &str) -> Self {
        Self {
            upstream,
            downstream,
            model: model.to_string(),
            response_id: format!("resp_{}", uuid::Uuid::new_v4().simple()),
            chat_id: format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
            created: now_unix(),
            seq: 0,
            started: false,
            completed: false,
            msg_item_id: format!("msg_{}", uuid::Uuid::new_v4().simple()),
            msg_open: false,
            msg_output_index: 0,
            text_acc: String::new(),
            rs_item_id: format!("rs_{}", uuid::Uuid::new_v4().simple()),
            rs_open: false,
            rs_closed: false,
            rs_text_acc: String::new(),
            tools: BTreeMap::new(),
            next_tool_output_index: 1,
            usage: None,
            finish_reason: None,
        }
    }

    fn seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// Translate one upstream SSE event into zero or more downstream frames.
    pub fn push_event(&mut self, event_name: Option<&str>, data: &str) -> Vec<OutFrame> {
        if data.trim() == "[DONE]" {
            return self.finalize();
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            return Vec::new();
        };
        match self.upstream {
            UpstreamKind::OpenAiChat => self.push_chat_chunk(&chunk),
            UpstreamKind::Anthropic => self.push_anthropic_event(event_name.unwrap_or(""), &chunk),
            UpstreamKind::Responses => Vec::new(), // passthrough; never translated
        }
    }

    /// Flush terminal frames (called when upstream closes without a
    /// finish signal so downstream never hangs).
    pub fn finalize(&mut self) -> Vec<OutFrame> {
        if self.completed {
            return match self.downstream {
                DownstreamKind::ChatCompletions => vec![OutFrame {
                    event: None,
                    data: Value::Null,
                    done_marker: true,
                }],
                DownstreamKind::Responses => Vec::new(),
            };
        }
        self.close_all_and_complete()
    }

    // ---- OpenAI chat.completion.chunk upstream ----

    fn push_chat_chunk(&mut self, chunk: &Value) -> Vec<OutFrame> {
        let mut out = Vec::new();
        if let Some(u) = chunk.get("usage") {
            if !u.is_null() {
                self.usage = Some(u.clone());
            }
        }
        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        else {
            return out;
        };
        let delta = choice.get("delta").cloned().unwrap_or(json!({}));

        // Kimi thinking streams as delta.reasoning_content.
        if let Some(text) = delta.get("reasoning_content").and_then(Value::as_str) {
            if !text.is_empty() {
                self.on_reasoning_delta(text, &mut out);
            }
        }
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            if !text.is_empty() {
                self.on_text_delta(text, &mut out);
            }
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for tc in calls {
                let idx = tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let name = tc
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let args = tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let call_id = tc.get("id").and_then(Value::as_str).unwrap_or("");
                self.on_tool_delta(idx, call_id, name, args, &mut out);
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_string());
            out.extend(self.close_all_and_complete());
        }
        out
    }

    // ---- Anthropic Messages SSE upstream ----

    fn push_anthropic_event(&mut self, event: &str, data: &Value) -> Vec<OutFrame> {
        let mut out = Vec::new();
        match event {
            "message_start" => {
                self.ensure_started(&mut out);
                if let Some(u) = data.pointer("/message/usage") {
                    self.usage = Some(u.clone());
                }
            }
            "content_block_start" => {
                let block = data.get("content_block").cloned().unwrap_or(json!({}));
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => self.ensure_message_open(&mut out),
                    Some("tool_use") => {
                        let idx = data.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                        self.on_tool_delta(
                            idx,
                            block.get("id").and_then(Value::as_str).unwrap_or(""),
                            block.get("name").and_then(Value::as_str).unwrap_or(""),
                            "",
                            &mut out,
                        );
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let delta = data.get("delta").cloned().unwrap_or(json!({}));
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        let text = delta.get("text").and_then(Value::as_str).unwrap_or("");
                        if !text.is_empty() {
                            self.on_text_delta(text, &mut out);
                        }
                    }
                    Some("input_json_delta") => {
                        let idx = data.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                        let partial = delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        self.on_tool_delta(idx, "", "", partial, &mut out);
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(u) = data.get("usage") {
                    let existing = self.usage.take().unwrap_or(json!({}));
                    let mut merged = existing;
                    merged["output_tokens"] = u.get("output_tokens").cloned().unwrap_or(json!(0));
                    self.usage = Some(merged);
                }
                if data.pointer("/delta/stop_reason").is_some() {
                    self.finish_reason = data
                        .pointer("/delta/stop_reason")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
            }
            "message_stop" => {
                out.extend(self.close_all_and_complete());
            }
            _ => {}
        }
        out
    }

    // ---- shared state machine ----

    fn ensure_started(&mut self, out: &mut Vec<OutFrame>) {
        if self.started {
            return;
        }
        self.started = true;
        match self.downstream {
            DownstreamKind::Responses => {
                let response = json!({
                    "id": self.response_id,
                    "object": "response",
                    "created_at": self.created,
                    "status": "in_progress",
                    "model": self.model,
                    "output": [],
                });
                let seq = self.seq();
                out.push(OutFrame {
                    event: Some("response.created".into()),
                    data: json!({"type":"response.created","sequence_number":seq,"response":response}),
                    done_marker: false,
                });
                let seq = self.seq();
                out.push(OutFrame {
                    event: Some("response.in_progress".into()),
                    data: json!({"type":"response.in_progress","sequence_number":seq,"response":response}),
                    done_marker: false,
                });
            }
            DownstreamKind::ChatCompletions => {
                out.push(OutFrame {
                    event: None,
                    data: self.chat_chunk(json!({"role": "assistant"}), Value::Null),
                    done_marker: false,
                });
            }
        }
    }

    fn ensure_message_open(&mut self, out: &mut Vec<OutFrame>) {
        self.ensure_started(out);
        if self.msg_open || self.downstream != DownstreamKind::Responses {
            return;
        }
        self.msg_open = true;
        // The reasoning item, when present, owns output_index 0.
        self.msg_output_index = if self.rs_open { 1 } else { 0 };
        let index = self.msg_output_index;
        let seq = self.seq();
        out.push(OutFrame {
            event: Some("response.output_item.added".into()),
            data: json!({
                "type":"response.output_item.added","sequence_number":seq,
                "output_index":index,
                "item":{"id":self.msg_item_id,"type":"message","status":"in_progress","role":"assistant","content":[]}
            }),
            done_marker: false,
        });
        let seq = self.seq();
        out.push(OutFrame {
            event: Some("response.content_part.added".into()),
            data: json!({
                "type":"response.content_part.added","sequence_number":seq,
                "item_id":self.msg_item_id,"output_index":index,"content_index":0,
                "part":{"type":"output_text","text":"","annotations":[]}
            }),
            done_marker: false,
        });
    }

    /// Open the reasoning item lazily on the first thinking delta and stream
    /// it as a Responses reasoning summary.
    fn on_reasoning_delta(&mut self, text: &str, out: &mut Vec<OutFrame>) {
        self.rs_text_acc.push_str(text);
        if self.downstream != DownstreamKind::Responses {
            // Chat Completions downstream: surface thinking as plain content
            // would corrupt tool flow; drop it there.
            return;
        }
        self.ensure_started(out);
        if !self.rs_open {
            self.rs_open = true;
            let seq = self.seq();
            out.push(OutFrame {
                event: Some("response.output_item.added".into()),
                data: json!({
                    "type":"response.output_item.added","sequence_number":seq,
                    "output_index":0,
                    "item":{"id":self.rs_item_id,"type":"reasoning","status":"in_progress","summary":[]}
                }),
                done_marker: false,
            });
            let seq = self.seq();
            out.push(OutFrame {
                event: Some("response.reasoning_summary_part.added".into()),
                data: json!({
                    "type":"response.reasoning_summary_part.added","sequence_number":seq,
                    "item_id":self.rs_item_id,"output_index":0,"summary_index":0,
                    "part":{"type":"summary_text","text":""}
                }),
                done_marker: false,
            });
        }
        let seq = self.seq();
        out.push(OutFrame {
            event: Some("response.reasoning_summary_text.delta".into()),
            data: json!({
                "type":"response.reasoning_summary_text.delta","sequence_number":seq,
                "item_id":self.rs_item_id,"output_index":0,"summary_index":0,
                "delta":text
            }),
            done_marker: false,
        });
    }

    /// Close the reasoning item (done events) before the message completes.
    fn close_reasoning(&mut self, out: &mut Vec<OutFrame>) {
        if !self.rs_open || self.rs_closed || self.downstream != DownstreamKind::Responses {
            return;
        }
        self.rs_closed = true;
        let seq = self.seq();
        out.push(OutFrame {
            event: Some("response.reasoning_summary_text.done".into()),
            data: json!({
                "type":"response.reasoning_summary_text.done","sequence_number":seq,
                "item_id":self.rs_item_id,"output_index":0,"summary_index":0,
                "text":self.rs_text_acc
            }),
            done_marker: false,
        });
        let seq = self.seq();
        out.push(OutFrame {
            event: Some("response.reasoning_summary_part.done".into()),
            data: json!({
                "type":"response.reasoning_summary_part.done","sequence_number":seq,
                "item_id":self.rs_item_id,"output_index":0,"summary_index":0,
                "part":{"type":"summary_text","text":self.rs_text_acc}
            }),
            done_marker: false,
        });
        let seq = self.seq();
        out.push(OutFrame {
            event: Some("response.output_item.done".into()),
            data: json!({
                "type":"response.output_item.done","sequence_number":seq,
                "output_index":0,
                "item":{"id":self.rs_item_id,"type":"reasoning","status":"completed",
                        "summary":[{"type":"summary_text","text":self.rs_text_acc}]}
            }),
            done_marker: false,
        });
    }

    fn on_text_delta(&mut self, text: &str, out: &mut Vec<OutFrame>) {
        self.text_acc.push_str(text);
        match self.downstream {
            DownstreamKind::Responses => {
                self.ensure_message_open(out);
                let index = self.msg_output_index;
                let seq = self.seq();
                out.push(OutFrame {
                    event: Some("response.output_text.delta".into()),
                    data: json!({
                        "type":"response.output_text.delta","sequence_number":seq,
                        "item_id":self.msg_item_id,"output_index":index,"content_index":0,
                        "delta":text
                    }),
                    done_marker: false,
                });
            }
            DownstreamKind::ChatCompletions => {
                self.ensure_started(out);
                out.push(OutFrame {
                    event: None,
                    data: self.chat_chunk(json!({"content": text}), Value::Null),
                    done_marker: false,
                });
            }
        }
    }

    fn on_tool_delta(
        &mut self,
        idx: usize,
        call_id: &str,
        name: &str,
        args: &str,
        out: &mut Vec<OutFrame>,
    ) {
        self.ensure_started(out);
        if !self.tools.contains_key(&idx) {
            // Reserve 0 for reasoning (when present) and the next slot for
            // the message item; tools come after both.
            let output_index = if self.tools.is_empty() {
                let base = if self.rs_open { 2 } else { 1 };
                self.next_tool_output_index = base + 1;
                base
            } else {
                let i = self.next_tool_output_index;
                self.next_tool_output_index += 1;
                i
            };
            let state = ToolCallState {
                item_id: format!("fc_{}", uuid::Uuid::new_v4().simple()),
                call_id: call_id.to_string(),
                name: name.to_string(),
                arguments: String::new(),
                opened: false,
                output_index,
            };
            self.tools.insert(idx, state);
        }
        // Mutate state in a narrow scope, then emit frames with plain data
        // (avoids holding a &mut borrow across self.seq()/self.chat_chunk()).
        let (item_id, tool_call_id, tool_name, output_index, just_opened) = {
            let tool = self.tools.get_mut(&idx).unwrap();
            if !call_id.is_empty() && tool.call_id.is_empty() {
                tool.call_id = call_id.to_string();
            }
            if !name.is_empty() && tool.name.is_empty() {
                tool.name = name.to_string();
            }
            let just_opened = !tool.opened;
            if just_opened {
                tool.opened = true;
            }
            if !args.is_empty() {
                tool.arguments.push_str(args);
            }
            (
                tool.item_id.clone(),
                tool.call_id.clone(),
                tool.name.clone(),
                tool.output_index,
                just_opened,
            )
        };

        match self.downstream {
            DownstreamKind::Responses => {
                if just_opened {
                    let seq = self.seq();
                    out.push(OutFrame {
                        event: Some("response.output_item.added".into()),
                        data: json!({
                            "type":"response.output_item.added","sequence_number":seq,
                            "output_index":output_index,
                            "item":{"id":item_id,"type":"function_call","status":"in_progress",
                                    "call_id":tool_call_id,"name":tool_name,"arguments":""}
                        }),
                        done_marker: false,
                    });
                }
                if !args.is_empty() {
                    let seq = self.seq();
                    out.push(OutFrame {
                        event: Some("response.function_call_arguments.delta".into()),
                        data: json!({
                            "type":"response.function_call_arguments.delta","sequence_number":seq,
                            "item_id":item_id,"output_index":output_index,
                            "delta":args
                        }),
                        done_marker: false,
                    });
                }
            }
            DownstreamKind::ChatCompletions => {
                if just_opened {
                    out.push(OutFrame {
                        event: None,
                        data: self.chat_chunk(
                            json!({"tool_calls":[{
                                "index":idx,"id":tool_call_id,"type":"function",
                                "function":{"name":tool_name,"arguments":""}
                            }]}),
                            Value::Null,
                        ),
                        done_marker: false,
                    });
                }
                if !args.is_empty() {
                    out.push(OutFrame {
                        event: None,
                        data: self.chat_chunk(
                            json!({"tool_calls":[{"index":idx,"function":{"arguments":args}}]}),
                            Value::Null,
                        ),
                        done_marker: false,
                    });
                }
            }
        }
    }

    fn close_all_and_complete(&mut self) -> Vec<OutFrame> {
        let mut out = Vec::new();
        if self.completed {
            return out;
        }
        self.completed = true;
        self.ensure_started(&mut out);

        match self.downstream {
            DownstreamKind::Responses => {
                self.close_reasoning(&mut out);
                if self.msg_open {
                    let index = self.msg_output_index;
                    let seq = self.seq();
                    out.push(OutFrame {
                        event: Some("response.output_text.done".into()),
                        data: json!({
                            "type":"response.output_text.done","sequence_number":seq,
                            "item_id":self.msg_item_id,"output_index":index,"content_index":0,
                            "text":self.text_acc
                        }),
                        done_marker: false,
                    });
                    let seq = self.seq();
                    out.push(OutFrame {
                        event: Some("response.content_part.done".into()),
                        data: json!({
                            "type":"response.content_part.done","sequence_number":seq,
                            "item_id":self.msg_item_id,"output_index":index,"content_index":0,
                            "part":{"type":"output_text","text":self.text_acc,"annotations":[]}
                        }),
                        done_marker: false,
                    });
                    let seq = self.seq();
                    out.push(OutFrame {
                        event: Some("response.output_item.done".into()),
                        data: json!({
                            "type":"response.output_item.done","sequence_number":seq,
                            "output_index":index,
                            "item":{"id":self.msg_item_id,"type":"message","status":"completed","role":"assistant",
                                    "content":[{"type":"output_text","text":self.text_acc,"annotations":[]}]}
                        }),
                        done_marker: false,
                    });
                }
                let tools_snapshot: Vec<(String, usize, String, String, String)> = self
                    .tools
                    .values()
                    .filter(|t| t.opened)
                    .map(|t| {
                        (
                            t.item_id.clone(),
                            t.output_index,
                            t.call_id.clone(),
                            t.name.clone(),
                            t.arguments.clone(),
                        )
                    })
                    .collect();
                for (item_id, output_index, call_id, name, arguments) in tools_snapshot {
                    let seq = self.seq();
                    out.push(OutFrame {
                        event: Some("response.function_call_arguments.done".into()),
                        data: json!({
                            "type":"response.function_call_arguments.done","sequence_number":seq,
                            "item_id":item_id,"output_index":output_index,
                            "arguments":arguments
                        }),
                        done_marker: false,
                    });
                    let seq = self.seq();
                    out.push(OutFrame {
                        event: Some("response.output_item.done".into()),
                        data: json!({
                            "type":"response.output_item.done","sequence_number":seq,
                            "output_index":output_index,
                            "item":{"id":item_id,"type":"function_call","status":"completed",
                                    "call_id":call_id,"name":name,"arguments":arguments}
                        }),
                        done_marker: false,
                    });
                }
                let usage = self.usage.clone().unwrap_or(Value::Null);
                let usage = if usage.is_null() {
                    Value::Null
                } else if self.upstream == UpstreamKind::OpenAiChat {
                    map_usage_chat(&usage)
                } else {
                    map_usage_anthropic(&usage)
                };
                let response = json!({
                    "id": self.response_id,
                    "object": "response",
                    "created_at": self.created,
                    "status": "completed",
                    "model": self.model,
                    "usage": usage,
                });
                let seq = self.seq();
                out.push(OutFrame {
                    event: Some("response.completed".into()),
                    data: json!({"type":"response.completed","sequence_number":seq,"response":response}),
                    done_marker: false,
                });
            }
            DownstreamKind::ChatCompletions => {
                let reason = match self.finish_reason.as_deref() {
                    Some("tool_calls") | Some("tool_use") => "tool_calls",
                    Some("length") | Some("max_tokens") => "length",
                    _ => "stop",
                };
                out.push(OutFrame {
                    event: None,
                    data: self.chat_chunk(json!({}), json!(reason)),
                    done_marker: false,
                });
                if let Some(u) = self.usage.clone() {
                    let usage = if self.upstream == UpstreamKind::OpenAiChat {
                        u
                    } else {
                        json!({
                            "prompt_tokens": u.get("input_tokens").cloned().unwrap_or(json!(0)),
                            "completion_tokens": u.get("output_tokens").cloned().unwrap_or(json!(0)),
                            "total_tokens": u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0)
                                + u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0),
                        })
                    };
                    out.push(OutFrame {
                        event: None,
                        data: json!({
                            "id": self.chat_id, "object": "chat.completion.chunk",
                            "created": self.created, "model": self.model,
                            "choices": [], "usage": usage,
                        }),
                        done_marker: false,
                    });
                }
                out.push(OutFrame {
                    event: None,
                    data: Value::Null,
                    done_marker: true,
                });
            }
        }
        out
    }

    fn chat_chunk(&self, delta: Value, finish: Value) -> Value {
        let mut choice = json!({"index": 0, "delta": delta});
        if !finish.is_null() {
            choice["finish_reason"] = finish;
        } else {
            choice["finish_reason"] = Value::Null;
        }
        json!({
            "id": self.chat_id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [choice],
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_stream_reasoning_produces_summary_events() {
        let mut t =
            StreamTranslator::new(UpstreamKind::OpenAiChat, DownstreamKind::Responses, "k3");
        let chunks = [
            json!({"choices":[{"delta":{"reasoning_content":"thinking "},"finish_reason":null}]}),
            json!({"choices":[{"delta":{"reasoning_content":"hard"},"finish_reason":null}]}),
            json!({"choices":[{"delta":{"content":"answer"},"finish_reason":null}]}),
            json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}),
        ];
        let mut types = Vec::new();
        for c in chunks {
            for f in t.push_event(None, &c.to_string()) {
                types.push((f.event.unwrap_or_default(), f.data));
            }
        }
        for f in t.finalize() {
            types.push((f.event.unwrap_or_default(), f.data));
        }
        let names: Vec<&str> = types.iter().map(|(e, _)| e.as_str()).collect();
        // Reasoning item opens before the message item, gets deltas, and
        // closes before the message done events.
        let rs_added = names
            .iter()
            .position(|e| *e == "response.reasoning_summary_text.delta")
            .unwrap();
        let msg_added = names
            .iter()
            .position(|e| *e == "response.output_text.delta")
            .unwrap();
        assert!(
            rs_added < msg_added,
            "reasoning should stream first: {names:?}"
        );
        // Reasoning owns output_index 0; the message shifts to 1.
        let msg_delta = &types[msg_added].1;
        assert_eq!(msg_delta["output_index"], 1);
        assert!(names.contains(&"response.reasoning_summary_text.done"));
        assert!(names.contains(&"response.completed"));
        // Accumulated thinking text lands in the done event.
        let done = types
            .iter()
            .find(|(e, _)| e == "response.reasoning_summary_text.done")
            .unwrap();
        assert_eq!(done.1["text"], "thinking hard");
    }

    #[test]
    fn responses_request_converts_tools_and_input() {
        let payload = json!({
            "model": "deepseek/deepseek-chat",
            "instructions": "Be brief",
            "input": [{"role":"user","content":[{"type":"input_text","text":"hi"}]}],
            "tools": [{"type":"function","name":"get_weather","description":"w","parameters":{"type":"object"}}],
            "stream": true
        });
        let out = responses_to_chat(&payload, "deepseek-chat", false).unwrap();
        assert_eq!(out["model"], "deepseek-chat");
        assert_eq!(out["messages"][0]["role"], "system");
        assert_eq!(out["messages"][1]["content"], "hi");
        assert_eq!(out["tools"][0]["function"]["name"], "get_weather");
        assert_eq!(out["stream_options"]["include_usage"], true);
    }

    #[test]
    fn parallel_function_calls_merge_into_one_assistant_message() {
        // Codex emits parallel tool calls as consecutive `function_call`
        // items, then the matching outputs. Splitting them into separate
        // assistant messages breaks strict upstreams (Console Go 400s with
        // "an assistant message with 'tool_calls' must be followed by tool
        // messages responding to each 'tool_call_id'").
        let payload = json!({
            "model": "deepseek-v4-flash",
            "input": [
                {"role":"user","content":[{"type":"input_text","text":"do both"}]},
                {"type":"function_call","call_id":"c1","name":"read_file","arguments":"{\"path\":\"a\"}"},
                {"type":"function_call","call_id":"c2","name":"read_file","arguments":"{\"path\":\"b\"}"},
                {"type":"function_call_output","call_id":"c1","output":"a"},
                {"type":"function_call_output","call_id":"c2","output":"b"},
                {"role":"assistant","content":[{"type":"output_text","text":"done"}]}
            ]
        });
        let out = responses_to_chat(&payload, "deepseek-v4-flash", false).unwrap();
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[1]["role"], "assistant");
        let calls = msgs[1]["tool_calls"].as_array().unwrap();
        assert_eq!(
            calls.len(),
            2,
            "parallel calls must share one assistant message"
        );
        assert_eq!(calls[0]["id"], "c1");
        assert_eq!(calls[1]["id"], "c2");
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["tool_call_id"], "c1");
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "c2");
        assert_eq!(msgs[4]["role"], "assistant");
    }

    #[test]
    fn interleaved_developer_message_does_not_break_tool_sequence() {
        // macOS Codex injects a developer (-> system) item between the
        // assistant's function_call items and their outputs. That must be
        // hoisted before the tool_calls message, not left in the middle.
        let payload = json!({
            "model": "deepseek-v4-flash",
            "input": [
                {"role":"developer","content":[{"type":"input_text","text":"app-context"}]},
                {"role":"user","content":[{"type":"input_text","text":"analise"}]},
                {"role":"assistant","content":[{"type":"output_text","text":"vou ler"}]},
                {"type":"function_call","call_id":"call_00_x","name":"shell","arguments":"{\"cmd\":\"ls\"}"},
                {"type":"function_call","call_id":"call_01_y","name":"shell","arguments":"{\"cmd\":\"pwd\"}"},
                {"role":"developer","content":[{"type":"input_text","text":"<context_guidance>hint"}]},
                {"type":"function_call_output","call_id":"call_00_x","output":"a"},
                {"type":"function_call_output","call_id":"call_01_y","output":"b"}
            ]
        });
        let out = responses_to_chat(&payload, "deepseek-v4-flash", false).unwrap();
        let msgs = out["messages"].as_array().unwrap();
        let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
        // The hoisted system must sit right before the tool_calls message.
        assert_eq!(
            roles,
            [
                "system",
                "user",
                "assistant",
                "system",
                "assistant",
                "tool",
                "tool"
            ]
        );
        let tc = &msgs[4];
        assert_eq!(tc["role"], "assistant");
        assert_eq!(tc["tool_calls"].as_array().unwrap().len(), 2);
        assert_eq!(msgs[5]["tool_call_id"], "call_00_x");
        assert_eq!(msgs[6]["tool_call_id"], "call_01_y");
    }

    #[test]
    fn deepseek_cache_tokens_map_to_responses_usage() {
        let chat = json!({
            "id":"chatcmpl-1","created":1,
            "choices":[{"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":100,"completion_tokens":10,"total_tokens":110,
                     "prompt_cache_hit_tokens":64,"prompt_cache_miss_tokens":36}
        });
        let resp = chat_completion_to_responses(&chat, "deepseek-chat");
        assert_eq!(resp["usage"]["input_tokens_details"]["cached_tokens"], 64);
    }

    #[test]
    fn reasoning_items_round_trip_as_reasoning_content() {
        // DeepSeek thinking + tools: the API 400s unless prior
        // reasoning_content is sent back with the assistant message.
        let payload = json!({
            "model": "deepseek-v4-pro",
            "input": [
                {"role":"user","content":[{"type":"input_text","text":"weather?"}]},
                {"type":"reasoning","summary":[{"type":"summary_text","text":"need the tool"}]},
                {"type":"function_call","call_id":"c1","name":"get_weather","arguments":"{}"},
                {"type":"function_call_output","call_id":"c1","output":"sunny"},
                {"type":"reasoning","summary":[{"type":"summary_text","text":"got it"}]},
                {"role":"assistant","content":[{"type":"output_text","text":"Sunny today"}]},
                {"role":"user","content":[{"type":"input_text","text":"thanks"}]}
            ]
        });
        let out = responses_to_chat(&payload, "deepseek-v4-pro", false).unwrap();
        let msgs = out["messages"].as_array().unwrap();
        let tool_call_msg = &msgs[1];
        assert_eq!(tool_call_msg["reasoning_content"], "need the tool");
        assert!(tool_call_msg["tool_calls"].is_array());
        let assistant_msg = &msgs[3];
        assert_eq!(assistant_msg["reasoning_content"], "got it");
        // User messages never get reasoning attached.
        assert!(msgs[4].get("reasoning_content").is_none());
    }

    #[test]
    fn reasoning_effort_uses_one_dialect_per_provider() {
        let payload = json!({
            "model": "m",
            "input": "hi",
            "reasoning": {"effort": "medium"}
        });
        // Mapped dialect (Kimi/DeepSeek): reasoning_effort only, medium->high.
        let mapped = responses_to_chat(&payload, "m", false).unwrap();
        assert_eq!(mapped["reasoning_effort"], "high");
        assert!(mapped.get("reasoning").is_none());
        // Unified dialect (OpenRouter): reasoning:{effort} only, verbatim.
        let unified = responses_to_chat(&payload, "m", true).unwrap();
        assert_eq!(unified["reasoning"]["effort"], "medium");
        assert!(unified.get("reasoning_effort").is_none());
    }

    #[test]
    fn chat_json_to_responses() {
        let chat = json!({
            "id":"chatcmpl-1","created":1,
            "choices":[{"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}
        });
        let resp = chat_completion_to_responses(&chat, "m");
        assert_eq!(resp["status"], "completed");
        assert_eq!(resp["output"][0]["content"][0]["text"], "hello");
        assert_eq!(resp["usage"]["input_tokens"], 3);
    }

    #[test]
    fn chat_stream_text_produces_responses_events() {
        let mut t = StreamTranslator::new(UpstreamKind::OpenAiChat, DownstreamKind::Responses, "m");
        let f1 = t.push_event(
            None,
            r#"{"choices":[{"delta":{"content":"Hel"},"finish_reason":null}]}"#,
        );
        let f2 = t.push_event(
            None,
            r#"{"choices":[{"delta":{"content":"lo"},"finish_reason":null}]}"#,
        );
        let f3 = t.push_event(None, r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#);
        let all: Vec<_> = f1.into_iter().chain(f2).chain(f3).collect();
        let events: Vec<String> = all.iter().filter_map(|f| f.event.clone()).collect();
        assert!(events.contains(&"response.created".to_string()));
        assert!(events.contains(&"response.output_text.delta".to_string()));
        assert!(events.contains(&"response.output_text.done".to_string()));
        assert!(events.contains(&"response.completed".to_string()));
    }

    #[test]
    fn chat_stream_tool_call_accumulates_arguments() {
        let mut t = StreamTranslator::new(UpstreamKind::OpenAiChat, DownstreamKind::Responses, "m");
        t.push_event(None, r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"run","arguments":""}}]},"finish_reason":null}]}"#);
        t.push_event(None, r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"a\":"}}]},"finish_reason":null}]}"#);
        t.push_event(None, r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]},"finish_reason":null}]}"#);
        let done = t.push_event(
            None,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        );
        let args_done = done
            .iter()
            .find(|f| f.event.as_deref() == Some("response.function_call_arguments.done"))
            .expect("arguments.done frame");
        assert_eq!(args_done.data["arguments"], "{\"a\":1}");
        assert!(done
            .iter()
            .any(|f| f.event.as_deref() == Some("response.completed")));
    }

    #[test]
    fn anthropic_stream_text_produces_chat_chunks() {
        let mut t = StreamTranslator::new(
            UpstreamKind::Anthropic,
            DownstreamKind::ChatCompletions,
            "m",
        );
        t.push_event(
            Some("message_start"),
            r#"{"type":"message_start","message":{"usage":{"input_tokens":5}}}"#,
        );
        t.push_event(
            Some("content_block_start"),
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text"}}"#,
        );
        let deltas = t.push_event(
            Some("content_block_delta"),
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
        );
        assert!(deltas
            .iter()
            .any(|f| f.data["choices"][0]["delta"]["content"] == "hi"));
        let end = t.push_event(Some("message_stop"), r#"{"type":"message_stop"}"#);
        assert!(end.iter().any(|f| f.done_marker));
    }

    #[test]
    fn anthropic_json_to_chat() {
        let msg = json!({
            "id":"msg_1",
            "content":[{"type":"text","text":"yo"},{"type":"tool_use","id":"tu_1","name":"run","input":{"x":1}}],
            "usage":{"input_tokens":4,"output_tokens":2}
        });
        let chat = anthropic_to_chat(&msg, "m");
        assert_eq!(chat["choices"][0]["message"]["content"], "yo");
        assert_eq!(
            chat["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
            "run"
        );
        assert_eq!(chat["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(chat["usage"]["total_tokens"], 6);
    }

    // -----------------------------------------------------------------
    // normalize_usage — the single source of truth for usage dialects.
    //
    // Regression cover for the stats gap: any dialect that reached the
    // recorder un-normalized reported zero tokens and was silently
    // dropped, so the dashboard stayed empty for everything except
    // Codex. Each case below asserts the canonical Responses shape.
    // -----------------------------------------------------------------

    /// Assert the canonical shape in one place, so a change to the
    /// contract fails every dialect test at once rather than one.
    fn assert_canonical(u: &Value, input: u64, output: u64, cached: u64) {
        assert_eq!(u["input_tokens"], input, "input_tokens");
        assert_eq!(u["output_tokens"], output, "output_tokens");
        assert_eq!(
            u["input_tokens_details"]["cached_tokens"], cached,
            "cached_tokens"
        );
    }

    #[test]
    fn normalize_usage_reads_chat_completions_top_level() {
        let payload = json!({
            "choices": [{"finish_reason": "stop"}],
            "usage": {
                "prompt_tokens": 108,
                "completion_tokens": 111,
                "total_tokens": 219,
                "prompt_tokens_details": {"cached_tokens": 64}
            }
        });
        let u = normalize_usage(UpstreamKind::OpenAiChat, &payload).expect("usage");
        assert_canonical(&u, 108, 111, 64);
    }

    #[test]
    fn normalize_usage_reads_kimi_per_choice_placement() {
        // Kimi attaches usage to the final choice instead of the top
        // level; OpenAI only ever emits it top-level.
        let chunk = json!({
            "object": "chat.completion.chunk",
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "stop",
                "usage": {"prompt_tokens": 90, "completion_tokens": 46, "total_tokens": 136}
            }]
        });
        let u = normalize_usage(UpstreamKind::OpenAiChat, &chunk).expect("usage");
        assert_canonical(&u, 90, 46, 0);
    }

    #[test]
    fn normalize_usage_reads_deepseek_flat_cache_field() {
        let payload = json!({
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "prompt_cache_hit_tokens": 7
            }
        });
        let u = normalize_usage(UpstreamKind::OpenAiChat, &payload).expect("usage");
        assert_canonical(&u, 10, 5, 7);
    }

    #[test]
    fn normalize_usage_reads_anthropic_cache_read() {
        let payload = json!({
            "usage": {
                "input_tokens": 30,
                "output_tokens": 12,
                "cache_read_input_tokens": 25
            }
        });
        let u = normalize_usage(UpstreamKind::Anthropic, &payload).expect("usage");
        assert_canonical(&u, 30, 12, 25);
        assert_eq!(u["total_tokens"], 42);
    }

    #[test]
    fn normalize_usage_passes_responses_through_unchanged() {
        let payload = json!({
            "usage": {
                "input_tokens": 7,
                "output_tokens": 3,
                "input_tokens_details": {"cached_tokens": 2}
            }
        });
        let u = normalize_usage(UpstreamKind::Responses, &payload).expect("usage");
        assert_canonical(&u, 7, 3, 2);
    }

    #[test]
    fn normalize_usage_finds_streaming_completed_event() {
        // Responses streams nest the final usage under the completed event.
        let frame = json!({
            "type": "response.completed",
            "response": {"usage": {"input_tokens": 5, "output_tokens": 9}}
        });
        let u = normalize_usage(UpstreamKind::Responses, &frame).expect("usage");
        assert_canonical(&u, 5, 9, 0);
    }

    #[test]
    fn normalize_usage_is_none_before_the_terminal_frame() {
        // Every frame before the last carries no usage; the tap must treat
        // that as "not yet", not as a zero-token turn.
        let delta = json!({"choices": [{"delta": {"content": "hi"}, "finish_reason": null}]});
        assert!(normalize_usage(UpstreamKind::OpenAiChat, &delta).is_none());
        assert!(normalize_usage(UpstreamKind::Responses, &json!({})).is_none());
        // An explicit null must not be mistaken for a usage object.
        assert!(normalize_usage(UpstreamKind::OpenAiChat, &json!({"usage": null})).is_none());
    }

    #[test]
    fn per_choice_lookup_stays_out_of_the_other_dialects() {
        // Only Chat Completions puts usage inside choices; looking there
        // for an Anthropic payload would be a false positive.
        let payload = json!({"choices": [{"usage": {"input_tokens": 1, "output_tokens": 1}}]});
        assert!(normalize_usage(UpstreamKind::Anthropic, &payload).is_none());
    }

    #[test]
    fn normalized_chat_usage_survives_the_recorder() {
        // The end of the bug: chat-shaped usage reached RequestEntry::ok
        // as 0/0 and was dropped. Normalized first, it is recorded.
        let raw = json!({"usage": {"prompt_tokens": 108, "completion_tokens": 111}});
        assert!(
            crate::stats::RequestEntry::ok("p", "m", "http", None, &raw["usage"]).is_none(),
            "raw chat usage is still rejected by the recorder"
        );
        let normalized = normalize_usage(UpstreamKind::OpenAiChat, &raw).expect("usage");
        let entry = crate::stats::RequestEntry::ok("p", "m", "http", None, &normalized)
            .expect("normalized usage must be recordable");
        assert_eq!(entry.input_tokens, 108);
        assert_eq!(entry.output_tokens, 111);
    }
}
