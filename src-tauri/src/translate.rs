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
/// Flatten the Responses tool list into Chat Completions functions, and
/// report which namespace each flattened name came from.
///
/// Codex sends five tool shapes: `function`, `custom`, `namespace`,
/// `tool_search` and `web_search`. Chat Completions has exactly one, and no
/// field to carry a namespace — the Responses protocol keeps `namespace` and
/// `name` as separate fields on a function call, which is why the round trip
/// needs this map instead of a naming convention.
///
/// Encoding the namespace into the name does not work here. A real request
/// carries `namespace[mcp__codex_apps__codex_document_control]` holding
/// `_get_docum_83c7f0565c0f`: the namespace already contains `__` and the
/// tool name opens with `_`, so any concatenation produces a run of
/// underscores that no split can undo. Names stay untouched, and collisions
/// between namespaces are the only case that gets a prefix.
///
/// Returns the chat-shaped tools and a `flattened name -> namespace` map.
/// `tool_search` and `web_search` are dropped: they are executed by the
/// Responses backend, not by the model, and have no Chat equivalent.
fn flatten_tools(tools: &[Value]) -> (Vec<Value>, BTreeMap<String, String>) {
    let as_chat = |name: &str, t: &Value| {
        json!({
            "type": "function",
            "function": {
                "name": name,
                "description": t.get("description").cloned().unwrap_or(Value::Null),
                "parameters": t.get("parameters").cloned().unwrap_or(json!({})),
            }
        })
    };

    // Which bare names appear more than once across namespaces. Computed up
    // front so both the request and the response derive the same names from
    // the same payload.
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for t in tools {
        match t.get("type").and_then(Value::as_str) {
            Some("function") | Some("custom") => {
                if let Some(n) = t.get("name").and_then(Value::as_str) {
                    *seen.entry(n).or_insert(0) += 1;
                }
            }
            Some("namespace") => {
                for inner in t
                    .get("tools")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(n) = inner.get("name").and_then(Value::as_str) {
                        *seen.entry(n).or_insert(0) += 1;
                    }
                }
            }
            _ => {}
        }
    }

    let mut out = Vec::new();
    let mut namespaces = BTreeMap::new();
    for t in tools {
        match t.get("type").and_then(Value::as_str) {
            // `custom` is a freeform tool (apply_patch ships as one). It was
            // being dropped alongside the namespaces.
            Some("function") | Some("custom") => {
                if let Some(n) = t.get("name").and_then(Value::as_str) {
                    out.push(as_chat(n, t));
                }
            }
            Some("namespace") => {
                let ns = t.get("name").and_then(Value::as_str).unwrap_or_default();
                for inner in t
                    .get("tools")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let Some(n) = inner.get("name").and_then(Value::as_str) else {
                        continue;
                    };
                    let flat = if seen.get(n).copied().unwrap_or(0) > 1 {
                        format!("{ns}_{n}")
                    } else {
                        n.to_string()
                    };
                    out.push(as_chat(&flat, inner));
                    if !ns.is_empty() {
                        namespaces.insert(flat, ns.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    (out, namespaces)
}

/// `flattened tool name -> namespace` for a Responses request, so the reply
/// can restore the namespace Chat Completions cannot carry. Derived from the
/// same payload `responses_to_chat` flattens, so both sides agree without
/// having to thread state through the call.
pub fn tool_namespace_map(payload: &Value) -> BTreeMap<String, String> {
    payload
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| flatten_tools(tools).1)
        .unwrap_or_default()
}

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

    let mut out = json!({
        "model": model,
        "messages": messages,
        "stream": payload.get("stream").and_then(Value::as_bool).unwrap_or(false),
    });
    if let Some(max) = payload.get("max_output_tokens") {
        out["max_tokens"] = max.clone();
    }
    if let Some(tools) = payload.get("tools").and_then(Value::as_array) {
        // Namespaced and freeform tools used to be filtered out here, which
        // silently removed the whole multi-agent surface, apply_patch, and
        // every MCP server: a real request carries 23 tools and only 12 were
        // of type `function`. A dropped tool looks exactly like a tool the
        // model was never given, so this failed as "the model can't use MCP"
        // rather than as an error.
        let (chat_tools, namespaces) = flatten_tools(tools);
        // TEMPORARY — revert this commit once routed multi-agent and MCP are
        // confirmed working end to end.
        //
        // Counts and namespace names only: no parameters, no descriptions, so
        // request bodies stay out of the logs. It is `warn` so it shows in a
        // plain `tauri dev` run, which is also why it should not ship — every
        // routed request logs a line.
        {
            let mut unpacked: Vec<&str> = namespaces.values().map(String::as_str).collect();
            unpacked.sort_unstable();
            unpacked.dedup();
            tracing::warn!(
                "TOOLS-DEBUG {} entries in -> {} functions out, namespaces unpacked: [{}]",
                tools.len(),
                chat_tools.len(),
                unpacked.join(", ")
            );
        }
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
                        // Inter-agent task payloads travel in this part, and
                        // the text sits under `encrypted_content` rather than
                        // `text`. Skipping it delivered a spawned agent the
                        // header without its body — the child received
                        // "Message Type: NEW_TASK / Task name: ... / Payload:"
                        // and nothing after it, then reported having no task.
                        //
                        // The name describes the field's role in the native
                        // path, where the backend encrypts it; a routed model
                        // produces the payload itself, so what arrives here is
                        // the plaintext the parent wrote.
                        Some("encrypted_content") => {
                            if let Some(t) = p.get("encrypted_content").and_then(Value::as_str) {
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
            let tool_call = json!({
                "id": item.get("call_id").or(item.get("id")).cloned().unwrap_or(Value::Null),
                "type": "function",
                "function": {
                    "name": item.get("name").cloned().unwrap_or(Value::Null),
                    "arguments": item.get("arguments").cloned().unwrap_or(json!("")),
                }
            });
            // Parallel tool calls arrive as consecutive function_call items
            // (Codex's exec tool ids them exec_command:1, exec_command:2...).
            // Chat Completions models them as ONE assistant message with
            // several tool_calls, and strict providers (Kimi) 400 on the
            // one-message-per-call form because each assistant-with-tool_calls
            // must be followed immediately by ITS tool messages.
            let last_is_pending_calls = messages
                .last()
                .map(|m| {
                    m.get("role").and_then(Value::as_str) == Some("assistant")
                        && m.get("tool_calls").and_then(Value::as_array).is_some()
                })
                .unwrap_or(false);
            if last_is_pending_calls {
                if let Some(calls) = messages
                    .last_mut()
                    .and_then(|m| m.get_mut("tool_calls"))
                    .and_then(Value::as_array_mut)
                {
                    calls.push(tool_call);
                }
            } else {
                let mut msg = json!({
                    "role": "assistant",
                    "content": Value::Null,
                    "tool_calls": [tool_call]
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

/// Restore the namespace a flattened tool came from, if any.
///
/// Codex builds its tool call as `ToolName::new(namespace, name)` from two
/// separate protocol fields and then applies the default namespace when the
/// first is absent. Emitting only `name` therefore resolves a
/// `collaboration` or `mcp__*` tool against `functions`, where no handler is
/// registered, and the call is rejected as unknown.
fn apply_namespace(mut item: Value, namespaces: &BTreeMap<String, String>, name: &str) -> Value {
    if let Some(ns) = namespaces.get(name) {
        item["namespace"] = json!(ns);
    }
    item
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
    /// `flattened tool name -> namespace`, from the request this stream is
    /// answering. Chat Completions has no namespace field, so a call coming
    /// back from the model carries only the flattened name; Codex parses
    /// `namespace` and `name` as separate fields and resolves an unnamespaced
    /// call against the default namespace, which never matches a handler
    /// registered under `collaboration` or `mcp__*`. Empty for requests that
    /// sent no namespaced tools.
    tool_namespaces: BTreeMap<String, String>,
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
            tool_namespaces: BTreeMap::new(),
        }
    }

    /// Carry the request's `flattened name -> namespace` map into the reply.
    /// Build it with [`tool_namespace_map`] from the same payload that was
    /// translated, so both directions agree on the flattened names.
    pub fn with_tool_namespaces(mut self, namespaces: BTreeMap<String, String>) -> Self {
        self.tool_namespaces = namespaces;
        self
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
                    let item = apply_namespace(
                        json!({"id":item_id,"type":"function_call","status":"in_progress",
                               "call_id":tool_call_id,"name":tool_name,"arguments":""}),
                        &self.tool_namespaces,
                        &tool_name,
                    );
                    out.push(OutFrame {
                        event: Some("response.output_item.added".into()),
                        data: json!({
                            "type":"response.output_item.added","sequence_number":seq,
                            "output_index":output_index,
                            "item":item
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
                    let item = apply_namespace(
                        json!({"id":item_id,"type":"function_call","status":"completed",
                               "call_id":call_id,"name":name,"arguments":arguments}),
                        &self.tool_namespaces,
                        &name,
                    );
                    out.push(OutFrame {
                        event: Some("response.output_item.done".into()),
                        data: json!({
                            "type":"response.output_item.done","sequence_number":seq,
                            "output_index":output_index,
                            "item":item
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

    /// A request shaped like the real one: 23 entries of which only 12 were
    /// plain functions. Everything else — the whole multi-agent surface,
    /// apply_patch, and every MCP server — used to be filtered out, so a
    /// routed model silently had no MCP tools at all.
    fn namespaced_tools_payload() -> Value {
        json!({
            "input": [{"role":"user","content":[{"type":"input_text","text":"hi"}]}],
            "tools": [
                {"type":"function","name":"exec_command","description":"e","parameters":{"type":"object"}},
                {"type":"custom","name":"apply_patch","description":"p","parameters":{"type":"object"}},
                {"type":"namespace","name":"collaboration","tools":[
                    {"type":"function","name":"spawn_agent","description":"s","parameters":{"type":"object"}},
                    {"type":"function","name":"wait_agent","description":"w","parameters":{"type":"object"}}
                ]},
                // Namespace containing `__`, tool name opening with `_`:
                // concatenating the two produces a run of underscores that no
                // split can undo, which is why the namespace travels in a map.
                {"type":"namespace","name":"mcp__codex_apps__codex_document_control","tools":[
                    {"type":"function","name":"_get_docum_83c7f0565c0f","description":"d","parameters":{"type":"object"}}
                ]},
                {"type":"web_search"}
            ]
        })
    }

    #[test]
    fn spawned_agent_receives_the_task_payload_not_just_its_header() {
        // Shape taken from a real child thread: the parent's task travels as
        // an `agent_message` whose body is an `encrypted_content` part. The
        // header is `input_text` and arrived fine, so the child saw
        // "Payload:" followed by nothing and answered that it had no task.
        let payload = json!({
            "input": [{
                "role": "user",
                "content": [
                    {"type":"input_text","text":"Message Type: NEW_TASK\nTask name: /root/analyze_frontend\nSender: /root\nPayload:\n"},
                    {"type":"encrypted_content","encrypted_content":"Analyze the FRONTEND of the project at F:\\loom-router."}
                ]
            }]
        });
        let out = responses_to_chat(&payload, "k3", false).unwrap();
        let content = out["messages"][0]["content"].as_str().unwrap();
        assert!(content.contains("NEW_TASK"), "header: {content}");
        assert!(
            content.contains("Analyze the FRONTEND"),
            "payload body missing: {content}"
        );
    }

    #[test]
    fn namespaced_and_freeform_tools_reach_the_model() {
        let out = responses_to_chat(&namespaced_tools_payload(), "k3", false).unwrap();
        let names: Vec<&str> = out["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"exec_command"));
        assert!(names.contains(&"apply_patch"), "freeform tool: {names:?}");
        assert!(names.contains(&"spawn_agent"), "namespaced tool: {names:?}");
        assert!(names.contains(&"wait_agent"));
        assert!(names.contains(&"_get_docum_83c7f0565c0f"));
        // web_search is executed by the Responses backend, not the model.
        assert_eq!(names.len(), 5, "{names:?}");
    }

    #[test]
    fn tool_namespace_map_round_trips_names_that_no_separator_could_split() {
        let map = tool_namespace_map(&namespaced_tools_payload());
        assert_eq!(
            map.get("spawn_agent").map(String::as_str),
            Some("collaboration")
        );
        assert_eq!(
            map.get("_get_docum_83c7f0565c0f").map(String::as_str),
            Some("mcp__codex_apps__codex_document_control")
        );
        // Plain and freeform tools carry no namespace; Codex applies its
        // default, which is where their handlers are registered.
        assert!(!map.contains_key("exec_command"));
        assert!(!map.contains_key("apply_patch"));
    }

    #[test]
    fn streamed_tool_call_restores_the_namespace_codex_resolves_against() {
        // Without this the call comes back namespace-less, Codex resolves it
        // against `functions`, and no collaboration handler is registered
        // there — the call fails as an unknown tool.
        let mut t =
            StreamTranslator::new(UpstreamKind::OpenAiChat, DownstreamKind::Responses, "k3")
                .with_tool_namespaces(tool_namespace_map(&namespaced_tools_payload()));
        let frames = t.push_event(
            None,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"spawn_agent","arguments":"{}"}}]},"finish_reason":null}]}"#,
        );
        let added = frames
            .iter()
            .find(|f| f.event.as_deref() == Some("response.output_item.added"))
            .expect("output_item.added frame");
        assert_eq!(added.data["item"]["name"], "spawn_agent");
        assert_eq!(added.data["item"]["namespace"], "collaboration");
    }

    #[test]
    fn parallel_function_calls_share_one_assistant_message() {
        // Codex ids its exec tool calls exec_command:1, exec_command:2...
        // Emitted as one assistant message per call, strict chat providers
        // (Kimi) reject the sequence: the first assistant-with-tool_calls is
        // followed by another assistant instead of its tool messages.
        let payload = json!({
            "input": [
                {"role":"user","content":[{"type":"input_text","text":"run both"}]},
                {"type":"function_call","call_id":"exec_command:1","name":"exec_command","arguments":"{\"cmd\":\"ls\"}"},
                {"type":"function_call","call_id":"exec_command:2","name":"exec_command","arguments":"{\"cmd\":\"pwd\"}"},
                {"type":"function_call_output","call_id":"exec_command:1","output":"a\nb"},
                {"type":"function_call_output","call_id":"exec_command:2","output":"/repo"}
            ]
        });
        let out = responses_to_chat(&payload, "kimi-for-coding", false).unwrap();
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(
            msgs.len(),
            4,
            "user + 1 grouped assistant + 2 tool messages: {msgs:?}"
        );
        let calls = msgs[1]["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["id"], "exec_command:1");
        assert_eq!(calls[1]["id"], "exec_command:2");
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["tool_call_id"], "exec_command:1");
        assert_eq!(msgs[3]["tool_call_id"], "exec_command:2");
    }

    #[test]
    fn separated_function_calls_stay_in_separate_assistant_messages() {
        // Grouping must only merge CONSECUTIVE calls; a call answered before
        // the next one starts a fresh assistant message, or the tool
        // messages land on the wrong turn.
        let payload = json!({
            "input": [
                {"type":"function_call","call_id":"c1","name":"exec","arguments":"{}"},
                {"type":"function_call_output","call_id":"c1","output":"one"},
                {"type":"function_call","call_id":"c2","name":"exec","arguments":"{}"},
                {"type":"function_call_output","call_id":"c2","output":"two"}
            ]
        });
        let out = responses_to_chat(&payload, "kimi-for-coding", false).unwrap();
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 4, "{msgs:?}");
        assert_eq!(msgs[0]["tool_calls"].as_array().unwrap().len(), 1);
        assert_eq!(msgs[2]["tool_calls"].as_array().unwrap().len(), 1);
        assert_eq!(msgs[1]["tool_call_id"], "c1");
        assert_eq!(msgs[3]["tool_call_id"], "c2");
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
