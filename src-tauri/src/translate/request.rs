use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

use super::compaction::{compaction_item_text, is_compaction_item, COMPACTION_SUMMARY_PREFIX};
use super::response::is_minimax_model;
use super::tools::{all_tool_specs, flatten_tools, FREEFORM_INPUT_FIELD, TOOL_SEARCH_NAME};

pub fn flatten_agent_messages(payload: &mut Value) -> usize {
    if let Some(input) = payload.get_mut("input").and_then(Value::as_array_mut) {
        let mut touched = 0;
        for item in input.iter_mut() {
            if item.get("type").and_then(Value::as_str) == Some("agent_message") {
                if let Some(map) = item.as_object_mut() {
                    map.insert("type".into(), Value::String("message".into()));
                    map.insert("role".into(), Value::String("user".into()));
                    touched += 1;
                }
            }
            touched += flatten_content_parts(item.get_mut("content"), "input_text");
        }
        return touched;
    }

    let Some(messages) = payload.get_mut("messages").and_then(Value::as_array_mut) else {
        return 0;
    };
    let mut touched = 0;
    for message in messages.iter_mut() {
        touched += flatten_content_parts(message.get_mut("content"), "text");
    }
    touched
}

fn flatten_content_parts(content: Option<&mut Value>, part_type: &str) -> usize {
    let Some(Value::Array(parts)) = content else {
        return 0;
    };
    let mut touched = 0;
    parts.retain_mut(|part| {
        if part.get("type").and_then(Value::as_str) == Some("encrypted_content") {
            let text = part
                .get("encrypted_content")
                .and_then(Value::as_str)
                .or_else(|| part.get("text").and_then(Value::as_str))
                .unwrap_or_default();
            if !text.is_empty() {
                *part = json!({ "type": part_type, "text": text });
                touched += 1;
                return true;
            }
            // empty encrypted_content part — remove it
            touched += 1;
            return false;
        }
        true
    });
    touched
}

// ---------------------------------------------------------------------------

pub fn responses_to_chat(payload: &Value, model: &str, unified_reasoning: bool) -> Result<Value> {
    let mut messages: Vec<Value> = Vec::new();

    if let Some(instructions) = payload.get("instructions").and_then(Value::as_str) {
        messages.push(json!({"role": "system", "content": instructions}));
    }

    // Flatten BEFORE walking the input: replayed function_call items and
    // tool_search_output listings are re-flattened through the replay map,
    // and the discovered specs join the request's own tools.
    //
    // Namespaced and freeform tools used to be filtered out here, which
    // silently removed the whole multi-agent surface, apply_patch, and
    // every MCP server: a real request carries 23 tools and only 12 were
    // of type `function`. A dropped tool looks exactly like a tool the
    // model was never given, so this failed as "the model can't use MCP"
    // rather than as an error.
    let specs = all_tool_specs(payload);
    let (chat_tools, _namespaces, replay_names, _freeform) = flatten_tools(&specs);

    match payload.get("input") {
        Some(Value::String(text)) => {
            messages.push(json!({"role": "user", "content": text}));
        }
        Some(Value::Array(items)) => {
            // Thinking models require prior reasoning on replay: DeepSeek/Kimi
            // expect reasoning_content, while MiniMax expects the raw
            // reasoning_details array. Responses input carries it as reasoning
            // items; collect the text and re-attach it to the next assistant
            // message in the model's dialect.
            let minimax = is_minimax_model(model);
            let mut pending_reasoning = String::new();
            let mut pending_minimax_details: Option<Value> = None;
            for item in items {
                if item.get("type").and_then(Value::as_str) == Some("reasoning") {
                    if let Some(parts) = item.get("summary").and_then(Value::as_array) {
                        for p in parts {
                            if let Some(t) = p.get("text").and_then(Value::as_str) {
                                pending_reasoning.push_str(t);
                            }
                        }
                    }
                    if minimax {
                        if let Some(details) = item
                            .get("minimax_reasoning_details")
                            .and_then(Value::as_array)
                        {
                            if !details.is_empty() {
                                pending_minimax_details = Some(json!(details.clone()));
                            }
                        }
                    }
                    continue;
                }
                convert_response_input_item(
                    item,
                    &mut messages,
                    &mut pending_reasoning,
                    &mut pending_minimax_details,
                    minimax,
                    &replay_names,
                )?;
            }
        }
        _ => return Err(anyhow!("Responses payload has no usable 'input'")),
    }

    hoist_interleaved_system(&mut messages);
    hoist_tool_images(&mut messages);

    let mut out = json!({
        "model": model,
        "messages": messages,
        "stream": payload.get("stream").and_then(Value::as_bool).unwrap_or(false),
    });
    if let Some(max) = payload.get("max_output_tokens") {
        out["max_tokens"] = max.clone();
    }
    if !chat_tools.is_empty() {
        out["tools"] = Value::Array(chat_tools);
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

/// Private marker holding images lifted out of a tool result until
/// `hoist_tool_images` relocates them. Never reaches an upstream.
pub(crate) const TOOL_MEDIA_KEY: &str = "__loom_tool_media";

/// Flatten a Responses content array into chat text plus chat image parts.
///
/// Shared by message items and tool outputs: a Responses-only part such as
/// `input_image` is rejected outright by a chat upstream ("unknown variant
/// `input_image`, expected `text`"), so none may survive translation.
fn flatten_responses_parts(parts: &[Value]) -> (String, Vec<Value>) {
    let mut text = String::new();
    let mut media: Vec<Value> = Vec::new();
    for p in parts {
        match p.get("type").and_then(Value::as_str) {
            Some("input_text") | Some("output_text") => {
                if let Some(t) = p.get("text").and_then(Value::as_str) {
                    text.push_str(t);
                }
            }
            // Inter-agent task payloads travel in this part, and the text
            // sits under `encrypted_content` rather than `text`. Skipping it
            // delivered a spawned agent the header without its body - the
            // child received "Message Type: NEW_TASK / Task name: ...
            // / Payload:" and nothing after it, then reported having no task.
            //
            // The name describes the field's role in the native path, where
            // the backend encrypts it; a routed model produces the payload
            // itself, so what arrives here is the plaintext the parent wrote.
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
                    .map(str::to_string)
                    .or_else(|| {
                        p.get("image_url")
                            .and_then(|url| url.get("url"))
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    });
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
    (text, media)
}

/// Chat Completions has no place for an image inside a `tool` message, and
/// strict providers (Kimi) reject a user message interleaved between an
/// assistant's tool calls and their results. So every image a tool returned
/// rides in one user message placed after the whole run of tool results.
fn hoist_tool_images(messages: &mut Vec<Value>) {
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    let mut pending: Vec<Value> = Vec::new();
    for mut msg in messages.drain(..) {
        let media = msg
            .as_object_mut()
            .and_then(|object| object.remove(TOOL_MEDIA_KEY))
            .and_then(|value| match value {
                Value::Array(parts) => Some(parts),
                _ => None,
            })
            .unwrap_or_default();
        let is_tool = msg.get("role").and_then(Value::as_str) == Some("tool");
        if !is_tool && !pending.is_empty() {
            out.push(json!({"role": "user", "content": std::mem::take(&mut pending)}));
        }
        pending.extend(media);
        out.push(msg);
    }
    if !pending.is_empty() {
        out.push(json!({"role": "user", "content": pending}));
    }
    *messages = out;
}

fn convert_response_input_item(
    item: &Value,
    messages: &mut Vec<Value>,
    pending_reasoning: &mut String,
    pending_minimax_details: &mut Option<Value>,
    minimax: bool,
    replay_names: &BTreeMap<(String, String), String>,
) -> Result<()> {
    let item_type = item.get("type").and_then(Value::as_str);
    if is_compaction_item(item) {
        messages.push(json!({
            "role": "user",
            "content": format!(
                "{COMPACTION_SUMMARY_PREFIX}\n\n{}",
                compaction_item_text(item.get("encrypted_content").and_then(Value::as_str)),
            ),
        }));
        return Ok(());
    }
    // Attach collected reasoning to the next assistant message, then clear.
    let take_reasoning =
        |msg: &mut Value, role: &str, pending: &mut String, pending_details: &mut Option<Value>| {
            if role == "assistant" && !pending.is_empty() {
                let text = std::mem::take(pending);
                if minimax {
                    let details = pending_details.take().unwrap_or_else(|| {
                        json!([{
                            "type": "reasoning.text",
                            "format": "openai-responses-v1",
                            "index": 0,
                            "text": text,
                        }])
                    });
                    msg["reasoning_details"] = details;
                } else {
                    msg["reasoning_content"] = Value::String(text);
                }
            }
        };
    // Plain message items carry role+content; typed items are tool IO.
    // `agent_message` is how Codex delivers an inter-agent task to a spawned
    // child (ResponseItem::AgentMessage: author/recipient, header in an
    // input_text part, body in encrypted_content). It carries no role field,
    // so it falls through to "user" below — which is what a task is for the
    // child. Dropping it (the old `_ => {}` arm) left the child with the
    // environment and instructions but no task.
    if item_type.is_none() || item_type == Some("message") || item_type == Some("agent_message") {
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
                    take_reasoning(&mut msg, role, pending_reasoning, pending_minimax_details);
                    messages.push(msg);
                }
            }
            Some(Value::Array(parts)) => {
                let (text, media) = flatten_responses_parts(parts);
                // Providers reject empty messages (e.g. Kimi: "the message
                // with role 'developer' must not be empty"). Codex emits
                // empty developer placeholders, so drop contentless items.
                if media.is_empty() {
                    if !text.is_empty() {
                        let mut msg = json!({"role": role, "content": text});
                        take_reasoning(&mut msg, role, pending_reasoning, pending_minimax_details);
                        messages.push(msg);
                    }
                } else {
                    let mut content = vec![json!({"type": "text", "text": text})];
                    content.extend(media);
                    let mut msg = json!({"role": role, "content": content});
                    take_reasoning(&mut msg, role, pending_reasoning, pending_minimax_details);
                    messages.push(msg);
                }
            }
            _ => {}
        }
        return Ok(());
    }
    match item_type {
        Some("function_call") | Some("tool_search_call") | Some("custom_tool_call") => {
            // The model knows tools by their flattened names; a replayed call
            // arrives as bare `name` + `namespace` (Responses keeps them in
            // separate fields). Re-flatten through the same map the tool list
            // was built with, or collision-prefixed tools come back under a
            // name the model never saw. tool_search has no namespace and its
            // arguments are a JSON object rather than a string.
            let is_search = item_type == Some("tool_search_call");
            let is_custom = item_type == Some("custom_tool_call");
            let name = if is_search {
                json!(TOOL_SEARCH_NAME)
            } else {
                let ns = item.get("namespace").and_then(Value::as_str).unwrap_or("");
                let bare = item.get("name").and_then(Value::as_str).unwrap_or("");
                replay_names
                    .get(&(ns.to_string(), bare.to_string()))
                    .map(|s| json!(s))
                    .unwrap_or_else(|| item.get("name").cloned().unwrap_or(Value::Null))
            };
            // A freeform call's raw input lives in `input`, not `arguments`;
            // the model emitted it as `{"input": "<text>"}` (see
            // tool_parameters), so re-wrap it to keep history consistent with
            // what the model produced.
            let arguments = if is_custom {
                let input = item.get("input").and_then(Value::as_str).unwrap_or("");
                json!(json!({FREEFORM_INPUT_FIELD: input}).to_string())
            } else {
                match item.get("arguments") {
                    Some(Value::String(s)) => json!(s),
                    Some(v) => json!(v.to_string()),
                    None => json!(""),
                }
            };
            let new_call = json!({
                "id": item.get("call_id").or(item.get("id")).cloned().unwrap_or(Value::Null),
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": arguments,
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
                take_reasoning(
                    &mut msg,
                    "assistant",
                    pending_reasoning,
                    pending_minimax_details,
                );
                messages.push(msg);
            }
        }
        Some("function_call_output") | Some("custom_tool_call_output") => {
            // Codex's view_image tool answers with a Responses content array,
            // not a string. Copying it verbatim shipped `input_image` to a
            // chat upstream, which rejected the entire request.
            let output = item.get("output").cloned().unwrap_or(json!(""));
            let (content, media) = match &output {
                Value::Array(parts) => {
                    let (text, media) = flatten_responses_parts(parts);
                    // Providers reject empty tool content, and the image
                    // itself arrives in the user message hoisted below.
                    let text = if text.is_empty() && !media.is_empty() {
                        "[image returned by the tool]".to_string()
                    } else {
                        text
                    };
                    (Value::String(text), media)
                }
                _ => (output.clone(), Vec::new()),
            };
            let mut msg = json!({
                "role": "tool",
                "tool_call_id": item.get("call_id").cloned().unwrap_or(Value::Null),
                "content": content,
            });
            if !media.is_empty() {
                msg[TOOL_MEDIA_KEY] = Value::Array(media);
            }
            messages.push(msg);
        }
        // Client-side search results: the discovered specs are already in
        // this request's tool list (see all_tool_specs); what the model
        // still needs is the answer to its call, rendered with the
        // flattened names it can actually invoke. The server-side
        // variant carries no call_id and has no assistant call to answer,
        // so emitting a tool message for it would orphan the pairing
        // strict providers enforce.
        Some("tool_search_output") if item.get("call_id").and_then(Value::as_str).is_some() => {
            messages.push(json!({
                "role": "tool",
                "tool_call_id": item.get("call_id").cloned().unwrap_or(Value::Null),
                "content": render_tool_search_results(item, replay_names),
            }));
        }
        _ => {} // reasoning and friends: dropped
    }
    Ok(())
}

/// Model-readable answer to a `tool_search` call: which tools the client-side
/// search found, listed under the flattened names this request's tool list
/// advertises them by.
fn render_tool_search_results(
    item: &Value,
    replay_names: &BTreeMap<(String, String), String>,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    let push = |ns: &str, spec: &Value, lines: &mut Vec<String>| {
        let Some(bare) = spec.get("name").and_then(Value::as_str) else {
            return;
        };
        let flat = replay_names
            .get(&(ns.to_string(), bare.to_string()))
            .cloned()
            .unwrap_or_else(|| bare.to_string());
        let desc = spec
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        let desc = if desc.chars().count() > 300 {
            format!("{}…", desc.chars().take(300).collect::<String>())
        } else {
            desc.to_string()
        };
        if ns.is_empty() {
            lines.push(format!("- {flat}: {desc}"));
        } else {
            lines.push(format!("- {flat} (namespace {ns}): {desc}"));
        }
    };
    for spec in item
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match spec.get("type").and_then(Value::as_str) {
            Some("function") | Some("custom") => push("", spec, &mut lines),
            Some("namespace") => {
                let ns = spec.get("name").and_then(Value::as_str).unwrap_or_default();
                for inner in spec
                    .get("tools")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    push(ns, inner, &mut lines);
                }
            }
            _ => {}
        }
    }
    if lines.is_empty() {
        "tool_search found no tools matching the query.".to_string()
    } else {
        format!(
            "tool_search found {} tool(s); they are now available to call:\n{}",
            lines.len(),
            lines.join("\n")
        )
    }
}

/// Chat Completions payload -> Anthropic Messages payload.
/// Convert chat user content into Anthropic blocks. A chat `image_url` part
/// is meaningless to Anthropic (it wants `image` + a typed `source`), and
/// copying it verbatim makes the upstream reject the whole request the same
/// way a Responses `input_image` does on a chat upstream.
fn chat_parts_to_anthropic(content: Value) -> Value {
    let Value::Array(parts) = content else {
        return content;
    };
    let blocks: Vec<Value> = parts
        .into_iter()
        .map(|part| match part.get("type").and_then(Value::as_str) {
            Some("image_url") => {
                let url = part
                    .get("image_url")
                    .and_then(|value| match value {
                        Value::String(url) => Some(url.clone()),
                        other => other.get("url").and_then(Value::as_str).map(str::to_string),
                    })
                    .unwrap_or_default();
                // Anthropic takes raw bytes for a data URL and the address
                // itself for a remote one.
                match url
                    .strip_prefix("data:")
                    .and_then(|rest| rest.split_once(";base64,"))
                {
                    Some((media_type, data)) => json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": media_type,
                            "data": data,
                        },
                    }),
                    None => json!({
                        "type": "image",
                        "source": {"type": "url", "url": url},
                    }),
                }
            }
            _ => part,
        })
        .collect();
    Value::Array(blocks)
}

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
            _ => {
                messages.push(json!({"role": "user", "content": chat_parts_to_anthropic(content)}))
            }
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
                    // Anthropic requires an object-rooted schema. Anything
                    // else — absent, null, a bare `{}`, a grammar — becomes
                    // the minimal object rather than travelling as-is: `{}`
                    // is the shape strict upstreams reject.
                    "input_schema": match f.get("parameters") {
                        Some(Value::Object(m)) if m.get("type").and_then(Value::as_str) == Some("object") => {
                            f.get("parameters").cloned().unwrap()
                        }
                        _ => json!({"type": "object"}),
                    },
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
