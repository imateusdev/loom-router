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
                convert_response_input_item(item, &mut messages)?;
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
    // Codex sends reasoning effort inside a Responses-only object; map it to
    // the Chat Completions field providers understand. Codex's canonical
    // tiers are low/medium/high/xhigh; Kimi K3 accepts low/high/max.
    if let Some(effort) = payload
        .get("reasoning")
        .and_then(|r| r.get("effort"))
        .and_then(Value::as_str)
    {
        let mapped = match effort {
            "xhigh" => "max",
            "medium" => "high",
            other => other,
        };
        out["reasoning_effort"] = Value::String(mapped.to_string());
    }
    // Ask upstream to report usage on the final chunk when streaming.
    if out["stream"].as_bool() == Some(true) {
        out["stream_options"] = json!({"include_usage": true});
    }
    Ok(out)
}

fn convert_response_input_item(item: &Value, messages: &mut Vec<Value>) -> Result<()> {
    let item_type = item.get("type").and_then(Value::as_str);
    // Plain message items carry role+content; typed items are tool IO.
    if item_type.is_none() || item_type == Some("message") {
        let raw_role = item.get("role").and_then(Value::as_str).unwrap_or("user");
        // Some providers (Kimi) reject the Responses-era "developer" role;
        // it is semantically the system prompt, so downgrade it.
        let role = if raw_role == "developer" { "system" } else { raw_role };
        match item.get("content") {
            Some(Value::String(s)) => {
                if !s.is_empty() {
                    messages.push(json!({"role": role, "content": s}));
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
                        messages.push(json!({"role": role, "content": text}));
                    }
                } else {
                    let mut content = vec![json!({"type": "text", "text": text})];
                    content.extend(media);
                    messages.push(json!({"role": role, "content": content}));
                }
            }
            _ => {}
        }
        return Ok(());
    }
    match item_type {
        Some("function_call") => {
            messages.push(json!({
                "role": "assistant",
                "content": Value::Null,
                "tool_calls": [{
                    "id": item.get("call_id").or(item.get("id")).cloned().unwrap_or(Value::Null),
                    "type": "function",
                    "function": {
                        "name": item.get("name").cloned().unwrap_or(Value::Null),
                        "arguments": item.get("arguments").cloned().unwrap_or(json!("")),
                    }
                }]
            }));
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
    json!({
        "input_tokens": u.get("prompt_tokens").cloned().unwrap_or(json!(0)),
        "output_tokens": u.get("completion_tokens").cloned().unwrap_or(json!(0)),
        "total_tokens": u.get("total_tokens").cloned().unwrap_or(json!(0)),
    })
}

fn map_usage_anthropic(u: &Value) -> Value {
    let input = u.get("input_tokens").cloned().unwrap_or(json!(0));
    let output = u.get("output_tokens").cloned().unwrap_or(json!(0));
    let total = input.as_u64().unwrap_or(0) + output.as_u64().unwrap_or(0);
    json!({"input_tokens": input, "output_tokens": output, "total_tokens": total})
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
        if let Some(text) = msg.get("content").and_then(Value::as_str) {
            if !text.is_empty() {
                output.push(message_item(&text));
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
    text_acc: String,
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
            text_acc: String::new(),
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
                        let partial =
                            delta.get("partial_json").and_then(Value::as_str).unwrap_or("");
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
        let seq = self.seq();
        out.push(OutFrame {
            event: Some("response.output_item.added".into()),
            data: json!({
                "type":"response.output_item.added","sequence_number":seq,
                "output_index":0,
                "item":{"id":self.msg_item_id,"type":"message","status":"in_progress","role":"assistant","content":[]}
            }),
            done_marker: false,
        });
        let seq = self.seq();
        out.push(OutFrame {
            event: Some("response.content_part.added".into()),
            data: json!({
                "type":"response.content_part.added","sequence_number":seq,
                "item_id":self.msg_item_id,"output_index":0,"content_index":0,
                "part":{"type":"output_text","text":"","annotations":[]}
            }),
            done_marker: false,
        });
    }

    fn on_text_delta(&mut self, text: &str, out: &mut Vec<OutFrame>) {
        self.text_acc.push_str(text);
        match self.downstream {
            DownstreamKind::Responses => {
                self.ensure_message_open(out);
                let seq = self.seq();
                out.push(OutFrame {
                    event: Some("response.output_text.delta".into()),
                    data: json!({
                        "type":"response.output_text.delta","sequence_number":seq,
                        "item_id":self.msg_item_id,"output_index":0,"content_index":0,
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
            let output_index = self.next_tool_output_index;
            self.next_tool_output_index += 1;
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
                if self.msg_open {
                    let seq = self.seq();
                    out.push(OutFrame {
                        event: Some("response.output_text.done".into()),
                        data: json!({
                            "type":"response.output_text.done","sequence_number":seq,
                            "item_id":self.msg_item_id,"output_index":0,"content_index":0,
                            "text":self.text_acc
                        }),
                        done_marker: false,
                    });
                    let seq = self.seq();
                    out.push(OutFrame {
                        event: Some("response.content_part.done".into()),
                        data: json!({
                            "type":"response.content_part.done","sequence_number":seq,
                            "item_id":self.msg_item_id,"output_index":0,"content_index":0,
                            "part":{"type":"output_text","text":self.text_acc,"annotations":[]}
                        }),
                        done_marker: false,
                    });
                    let seq = self.seq();
                    out.push(OutFrame {
                        event: Some("response.output_item.done".into()),
                        data: json!({
                            "type":"response.output_item.done","sequence_number":seq,
                            "output_index":0,
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
    fn responses_request_converts_tools_and_input() {
        let payload = json!({
            "model": "deepseek/deepseek-chat",
            "instructions": "Be brief",
            "input": [{"role":"user","content":[{"type":"input_text","text":"hi"}]}],
            "tools": [{"type":"function","name":"get_weather","description":"w","parameters":{"type":"object"}}],
            "stream": true
        });
        let out = responses_to_chat(&payload, "deepseek-chat").unwrap();
        assert_eq!(out["model"], "deepseek-chat");
        assert_eq!(out["messages"][0]["role"], "system");
        assert_eq!(out["messages"][1]["content"], "hi");
        assert_eq!(out["tools"][0]["function"]["name"], "get_weather");
        assert_eq!(out["stream_options"]["include_usage"], true);
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
        let f1 = t.push_event(None, r#"{"choices":[{"delta":{"content":"Hel"},"finish_reason":null}]}"#);
        let f2 = t.push_event(None, r#"{"choices":[{"delta":{"content":"lo"},"finish_reason":null}]}"#);
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
        let done = t.push_event(None, r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#);
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
        let mut t =
            StreamTranslator::new(UpstreamKind::Anthropic, DownstreamKind::ChatCompletions, "m");
        t.push_event(Some("message_start"), r#"{"type":"message_start","message":{"usage":{"input_tokens":5}}}"#);
        t.push_event(Some("content_block_start"), r#"{"type":"content_block_start","index":0,"content_block":{"type":"text"}}"#);
        let deltas = t.push_event(
            Some("content_block_delta"),
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
        );
        assert!(deltas.iter().any(|f| f.data["choices"][0]["delta"]["content"] == "hi"));
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
        assert_eq!(chat["choices"][0]["message"]["tool_calls"][0]["function"]["name"], "run");
        assert_eq!(chat["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(chat["usage"]["total_tokens"], 6);
    }
}
