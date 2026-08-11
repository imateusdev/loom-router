use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

use super::stream::UpstreamKind;
use super::tools::{synthetic_id, unwrap_freeform_arguments, TOOL_SEARCH_NAME};

pub(crate) fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) fn map_usage_chat(u: &Value) -> Value {
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

pub(crate) fn map_usage_anthropic(u: &Value) -> Value {
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

/// Extract thinking text from a Chat Completions message/delta. MiniMax with
/// `reasoning_split: true` may return it as `reasoning_content` and, in
/// streams, also as `reasoning_details`; prefer the former so a chunk that
/// carries both fields is not duplicated.
pub(super) fn chat_reasoning_text(msg: &Value) -> Option<String> {
    if let Some(text) = msg.get("reasoning_content").and_then(Value::as_str) {
        return (!text.is_empty()).then(|| text.to_string());
    }
    msg.get("reasoning_details")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| {
                    if part.get("type").and_then(Value::as_str) == Some("reasoning.text") {
                        part.get("text").and_then(Value::as_str)
                    } else {
                        None
                    }
                })
                .collect()
        })
        .filter(|text: &String| !text.is_empty())
}

pub(super) fn is_minimax_model(model: &str) -> bool {
    model.to_ascii_lowercase().contains("minimax")
}

/// Chat Completions JSON response -> Responses API response object.
pub fn chat_completion_to_responses(chat: &Value, model: &str) -> Value {
    let id = chat
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| synthetic_id("resp"));
    let mut output: Vec<Value> = Vec::new();
    if let Some(choice) = chat
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
    {
        let msg = choice.get("message").cloned().unwrap_or(json!({}));
        // Thinking models return reasoning_content/reasoning_details alongside
        // content; keep it out of the visible message.
        if let Some(thinking) = chat_reasoning_text(&msg) {
            if !thinking.is_empty() {
                let mut item = json!({
                    "id": synthetic_id("rs"),
                    "type": "reasoning",
                    "status": "completed",
                    "summary": [{"type": "summary_text", "text": thinking}],
                });
                if is_minimax_model(model) {
                    if let Some(details) = msg.get("reasoning_details").and_then(Value::as_array) {
                        if !details.is_empty() {
                            item["minimax_reasoning_details"] = json!(details.clone());
                        }
                    }
                }
                output.push(item);
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
                output.push(tool_call_output_item(
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
        .unwrap_or_else(|| synthetic_id("resp"));
    let mut output: Vec<Value> = Vec::new();
    if let Some(blocks) = msg.get("content").and_then(Value::as_array) {
        for b in blocks {
            match b.get("type").and_then(Value::as_str) {
                Some("text") => output.push(message_item(
                    b.get("text").and_then(Value::as_str).unwrap_or(""),
                )),
                Some("tool_use") => output.push(tool_call_output_item(
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

/// Extract the assistant's plain text from a non-streaming upstream response,
/// whichever dialect produced it. Returns `None` when the payload has no text
/// (an error envelope, a tool-only turn, or an unparseable body).
pub fn extract_text(kind: UpstreamKind, payload: &Value) -> Option<String> {
    match kind {
        UpstreamKind::OpenAiChat => payload
            .get("choices")
            .and_then(Value::as_array)?
            .first()?
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)
            .map(str::to_string),
        UpstreamKind::Anthropic => payload
            .get("content")
            .and_then(Value::as_array)?
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n")
            .into_opt_string(),
        UpstreamKind::Responses => payload
            .get("output")
            .and_then(Value::as_array)?
            .iter()
            .filter_map(|item| {
                item.get("content")
                    .and_then(Value::as_array)?
                    .iter()
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
                    .into_opt_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
            .into_opt_string(),
    }
}

/// `join` of text blocks that yields `None` when the result is empty, so an
/// empty body is an envelope with no text rather than an empty answer.
trait IntoOptString {
    fn into_opt_string(self) -> Option<String>;
}

impl IntoOptString for String {
    fn into_opt_string(self) -> Option<String> {
        if self.is_empty() {
            None
        } else {
            Some(self)
        }
    }
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

pub(crate) fn message_item(text: &str) -> Value {
    json!({
        "id": synthetic_id("msg"),
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
pub(crate) fn apply_namespace(
    mut item: Value,
    namespaces: &BTreeMap<String, String>,
    name: &str,
) -> Value {
    if let Some(ns) = namespaces.get(name) {
        item["namespace"] = json!(ns);
    }
    item
}

pub(crate) fn function_call_item(call_id: &str, name: &str, arguments: &str) -> Value {
    json!({
        "id": synthetic_id("fc"),
        "type": "function_call",
        "status": "completed",
        "call_id": call_id,
        "name": name,
        "arguments": arguments,
    })
}

/// A `tool_search` call comes back from the Chat upstream as an ordinary
/// function call; Codex only recognizes it as the client-side discovery
/// trigger in its own shape. `execution: "client"` is what routes it to the
/// local BM25 handler, and `arguments` is a JSON object here, not the string
/// a `function_call` carries.
pub(crate) fn tool_search_call_item(call_id: &str, arguments: &str) -> Value {
    json!({
        "id": synthetic_id("tsc"),
        "type": "tool_search_call",
        "status": "completed",
        "call_id": call_id,
        "execution": "client",
        "arguments": serde_json::from_str::<Value>(arguments).unwrap_or(json!({})),
    })
}

/// The output item for a model tool call, in whichever shape the tool name
/// implies.
pub(crate) fn tool_call_output_item(call_id: &str, name: &str, arguments: &str) -> Value {
    if name == TOOL_SEARCH_NAME {
        tool_search_call_item(call_id, arguments)
    } else {
        function_call_item(call_id, name, arguments)
    }
}

/// Restore namespaces on the function_call items of an already-built
/// response output. The streaming translator applies them frame by frame;
/// the non-streaming converters build items without the request at hand,
/// so callers (proxy.rs) apply the map afterwards with
/// [`tool_namespace_map`] from the same request payload. Without this a
/// namespaced call resolves against `functions`, where no handler is
/// registered.
pub fn apply_namespaces_to_output(output: &mut [Value], namespaces: &BTreeMap<String, String>) {
    for item in output.iter_mut() {
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            continue;
        }
        if let Some(name) = item.get("name").and_then(Value::as_str) {
            if let Some(ns) = namespaces.get(name) {
                item["namespace"] = json!(ns);
            }
        }
    }
}

/// The Responses output item for a freeform tool call: `custom_tool_call`
/// (raw `input`), never `function_call`. Codex's router builds
/// `ToolPayload::Custom` only from this item type — a `function_call` for a
/// custom tool becomes `ToolPayload::Function`, which no freeform handler
/// matches, and the call is aborted as unknown.
///
/// `item_id` is the id the opening `output_item.added` frame already
/// announced for this output index: both frames describe one item, so minting
/// a fresh id here would hand the client two.
pub(crate) fn custom_tool_call_item(
    item_id: &str,
    call_id: &str,
    name: &str,
    input: &str,
) -> Value {
    json!({
        "id": item_id,
        "type": "custom_tool_call",
        "status": "completed",
        "call_id": call_id,
        "name": name,
        "input": input,
    })
}

/// Rewrite one function-wrapped freeform item back to the Responses custom
/// shape. Opening items keep their in-progress status; completed items are
/// normalized for the Codex tool router.
pub(crate) fn restore_freeform_response_item(item: &mut Value, completed: bool) {
    let input = item
        .get("arguments")
        .and_then(Value::as_str)
        .map(unwrap_freeform_arguments)
        .unwrap_or_default();
    let Some(map) = item.as_object_mut() else {
        return;
    };
    map.remove("arguments");
    map.insert("input".into(), json!(input));
    map.insert("type".into(), json!("custom_tool_call"));
    if completed {
        map.insert("status".into(), json!("completed"));
    }
}

/// Unwrap freeform tool calls in an already-built response output: the routed
/// model emits their input as `{"input": "<raw text>"}` ([`tool_parameters`]),
/// and Codex's freeform handler needs exactly the raw text in a
/// `custom_tool_call` item. The streaming translator does both per frame;
/// non-streaming converters build `function_call` items without the request at
/// hand, so callers (proxy.rs) apply the set afterwards with
/// [`freeform_tool_names`] from the same request payload.
pub fn unwrap_freeform_to_output(output: &mut [Value], freeform: &BTreeSet<String>) {
    for item in output.iter_mut() {
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            continue;
        }
        let Some(name) = item.get("name").and_then(Value::as_str).map(str::to_string) else {
            continue;
        };
        if !freeform.contains(&name) {
            continue;
        }
        restore_freeform_response_item(item, true);
    }
}

// ---------------------------------------------------------------------------
// Streaming translation
// ---------------------------------------------------------------------------
