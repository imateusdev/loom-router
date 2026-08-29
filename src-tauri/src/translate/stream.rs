use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

use super::response::{
    apply_namespace, chat_reasoning_text, custom_tool_call_item, is_minimax_model,
    map_usage_anthropic, map_usage_chat, now_unix, restore_freeform_response_item,
    unwrap_freeform_to_output,
};
use super::tools::{synthetic_id, unwrap_freeform_arguments, TOOL_SEARCH_NAME};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamKind {
    OpenAiChat,
    Anthropic,
    /// Responses-format upstream. Most events pass through; compatibility
    /// requests may restore function-wrapped freeform calls on the way back.
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
    rs_output_index: usize,
    rs_open: bool,
    rs_closed: bool,
    rs_text_acc: String,
    rs_details: Vec<Value>,
    anthropic_thinking_blocks: BTreeSet<usize>,
    // tool calls keyed by upstream index
    tools: BTreeMap<usize, ToolCallState>,
    next_output_index: usize,
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
    /// Flattened names of freeform custom tools, from the same request
    /// ([`freeform_tool_names`]). Their arguments are streamed as the JSON
    /// wrapper they travelled in and unwrapped only at the closing frames,
    /// where the full string is known.
    freeform_tools: BTreeSet<String>,
    /// Item ids for a Responses-native upstream that saw a freeform tool as a
    /// function. Its argument deltas carry the JSON wrapper and are buffered;
    /// the completed item is restored to one raw custom-tool input.
    responses_freeform_items: BTreeSet<String>,
}

impl StreamTranslator {
    pub fn new(upstream: UpstreamKind, downstream: DownstreamKind, model: &str) -> Self {
        Self {
            upstream,
            downstream,
            model: model.to_string(),
            response_id: synthetic_id("resp"),
            chat_id: format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
            created: now_unix(),
            seq: 0,
            started: false,
            completed: false,
            msg_item_id: synthetic_id("msg"),
            msg_open: false,
            msg_output_index: 0,
            text_acc: String::new(),
            rs_item_id: synthetic_id("rs"),
            rs_output_index: 0,
            rs_open: false,
            rs_closed: false,
            rs_text_acc: String::new(),
            rs_details: Vec::new(),
            anthropic_thinking_blocks: BTreeSet::new(),
            tools: BTreeMap::new(),
            next_output_index: 0,
            usage: None,
            finish_reason: None,
            tool_namespaces: BTreeMap::new(),
            freeform_tools: BTreeSet::new(),
            responses_freeform_items: BTreeSet::new(),
        }
    }

    /// Carry the request's `flattened name -> namespace` map into the reply.
    /// Build it with [`tool_namespace_map`] from the same payload that was
    /// translated, so both directions agree on the flattened names.
    pub fn with_tool_namespaces(mut self, namespaces: BTreeMap<String, String>) -> Self {
        self.tool_namespaces = namespaces;
        self
    }

    /// Carry the request's freeform tool names into the reply so their calls
    /// can be unwrapped back into the raw input ([`freeform_tool_names`]).
    pub fn with_freeform_tools(mut self, freeform: BTreeSet<String>) -> Self {
        self.freeform_tools = freeform;
        self
    }

    fn seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn allocate_output_index(&mut self) -> usize {
        let index = self.next_output_index;
        self.next_output_index += 1;
        index
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
            UpstreamKind::Responses => self.push_responses_event(event_name.unwrap_or(""), &chunk),
        }
    }

    /// Flush terminal frames (called when upstream closes without a
    /// finish signal so downstream never hangs).
    pub fn finalize(&mut self) -> Vec<OutFrame> {
        if self.upstream == UpstreamKind::Responses {
            return Vec::new();
        }
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

    /// Pass through one native Responses event, restoring only the custom
    /// tools that were function-wrapped for upstream compatibility.
    fn push_responses_event(&mut self, event_name: &str, chunk: &Value) -> Vec<OutFrame> {
        let mut data = chunk.clone();
        let event_type = data
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or(event_name)
            .to_string();

        match event_type.as_str() {
            "response.output_item.added" => {
                let Some(item) = data.get_mut("item") else {
                    return vec![OutFrame {
                        event: Some(event_type),
                        data,
                        done_marker: false,
                    }];
                };
                let is_wrapped = item.get("type").and_then(Value::as_str) == Some("function_call")
                    && item
                        .get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|name| self.freeform_tools.contains(name));
                if is_wrapped {
                    if let Some(item_id) = item.get("id").and_then(Value::as_str) {
                        self.responses_freeform_items.insert(item_id.to_string());
                    }
                    restore_freeform_response_item(item, false);
                }
            }
            "response.function_call_arguments.delta" | "response.function_call_arguments.done" => {
                if data
                    .get("item_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| self.responses_freeform_items.contains(id))
                {
                    return Vec::new();
                }
            }
            "response.output_item.done" => {
                if let Some(item) = data.get_mut("item") {
                    let is_wrapped = item
                        .get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| self.responses_freeform_items.contains(id))
                        || (item.get("type").and_then(Value::as_str) == Some("function_call")
                            && item
                                .get("name")
                                .and_then(Value::as_str)
                                .is_some_and(|name| self.freeform_tools.contains(name)));
                    if is_wrapped {
                        restore_freeform_response_item(item, true);
                    }
                }
            }
            "response.completed" => {
                if let Some(output) = data
                    .get_mut("response")
                    .and_then(|response| response.get_mut("output"))
                    .and_then(Value::as_array_mut)
                {
                    unwrap_freeform_to_output(output, &self.freeform_tools);
                }
                self.completed = true;
            }
            _ => {}
        }

        vec![OutFrame {
            event: Some(event_type),
            data,
            done_marker: false,
        }]
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

        // Kimi/MiniMax thinking streams as reasoning_content, with MiniMax
        // additionally exposing reasoning_details on some chunks.
        if is_minimax_model(&self.model) {
            if let Some(details) = delta.get("reasoning_details").and_then(Value::as_array) {
                self.merge_minimax_reasoning_details(details);
            }
        }
        if let Some(text) = chat_reasoning_text(&delta) {
            if !text.is_empty() {
                self.on_reasoning_delta(&text, &mut out);
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
                    Some("thinking") => {
                        self.ensure_started(&mut out);
                        let idx = data.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                        self.anthropic_thinking_blocks.insert(idx);
                    }
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
                    Some("thinking_delta") => {
                        let text = delta.get("thinking").and_then(Value::as_str).unwrap_or("");
                        if !text.is_empty() {
                            self.on_reasoning_delta(text, &mut out);
                        }
                    }
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
                    // Anthropic itself only reports input_tokens on
                    // message_start. A streamed `claude -p` turn does not know
                    // them yet at that point, so honour them here when sent —
                    // otherwise the turn records zero prompt tokens.
                    if let Some(input) = u.get("input_tokens") {
                        if merged
                            .get("input_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0)
                            == 0
                        {
                            merged["input_tokens"] = input.clone();
                        }
                    }
                    self.usage = Some(merged);
                }
                if data.pointer("/delta/stop_reason").is_some() {
                    self.finish_reason = data
                        .pointer("/delta/stop_reason")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
            }
            "content_block_stop" => {
                let idx = data.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if self.anthropic_thinking_blocks.remove(&idx) {
                    self.close_reasoning(&mut out);
                    self.reset_reasoning();
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
        self.msg_output_index = self.allocate_output_index();
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

    /// Merge MiniMax's streamed `reasoning_details` blocks back into one
    /// block per type/id so the raw field can be replayed on the next turn.
    fn merge_minimax_reasoning_details(&mut self, details: &[Value]) {
        for block in details {
            let Some(id) = block.get("id").and_then(Value::as_str) else {
                self.rs_details.push(block.clone());
                continue;
            };
            if let Some(existing) = self.rs_details.iter_mut().find(|b| {
                b.get("type").and_then(Value::as_str) == block.get("type").and_then(Value::as_str)
                    && b.get("id").and_then(Value::as_str) == Some(id)
            }) {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        match existing.get_mut("text") {
                            Some(Value::String(current)) => current.push_str(text),
                            _ => {
                                existing["text"] = json!(text);
                            }
                        }
                    }
                }
            } else {
                self.rs_details.push(block.clone());
            }
        }
    }

    /// Open the reasoning item lazily on the first thinking delta and stream
    /// it as a Responses reasoning summary.
    fn on_reasoning_delta(&mut self, text: &str, out: &mut Vec<OutFrame>) {
        if self.downstream != DownstreamKind::Responses {
            // Chat Completions downstream: surface thinking as plain content
            // would corrupt tool flow; drop it there. Accumulating first would
            // retain every progress line for a turn that cannot emit one.
            return;
        }
        self.rs_text_acc.push_str(text);
        self.ensure_started(out);
        if !self.rs_open {
            self.rs_open = true;
            self.rs_output_index = self.allocate_output_index();
            let output_index = self.rs_output_index;
            let seq = self.seq();
            out.push(OutFrame {
                event: Some("response.output_item.added".into()),
                data: json!({
                    "type":"response.output_item.added","sequence_number":seq,
                    "output_index":output_index,
                    "item":{"id":self.rs_item_id,"type":"reasoning","status":"in_progress","summary":[]}
                }),
                done_marker: false,
            });
            let seq = self.seq();
            out.push(OutFrame {
                event: Some("response.reasoning_summary_part.added".into()),
                data: json!({
                    "type":"response.reasoning_summary_part.added","sequence_number":seq,
                    "item_id":self.rs_item_id,"output_index":output_index,"summary_index":0,
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
                "item_id":self.rs_item_id,"output_index":self.rs_output_index,"summary_index":0,
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
        let output_index = self.rs_output_index;
        let seq = self.seq();
        out.push(OutFrame {
            event: Some("response.reasoning_summary_text.done".into()),
            data: json!({
                "type":"response.reasoning_summary_text.done","sequence_number":seq,
                "item_id":self.rs_item_id,"output_index":output_index,"summary_index":0,
                "text":self.rs_text_acc
            }),
            done_marker: false,
        });
        let seq = self.seq();
        out.push(OutFrame {
            event: Some("response.reasoning_summary_part.done".into()),
            data: json!({
                "type":"response.reasoning_summary_part.done","sequence_number":seq,
                "item_id":self.rs_item_id,"output_index":output_index,"summary_index":0,
                "part":{"type":"summary_text","text":self.rs_text_acc}
            }),
            done_marker: false,
        });
        let seq = self.seq();
        let mut item = json!({
            "id": self.rs_item_id,
            "type": "reasoning",
            "status": "completed",
            "summary": [{"type": "summary_text", "text": self.rs_text_acc}],
        });
        if is_minimax_model(&self.model) && !self.rs_details.is_empty() {
            item["minimax_reasoning_details"] = json!(self.rs_details.clone());
        }
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

    fn reset_reasoning(&mut self) {
        self.rs_item_id = synthetic_id("rs");
        self.rs_open = false;
        self.rs_closed = false;
        self.rs_text_acc.clear();
        self.rs_details.clear();
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
            let output_index = self.allocate_output_index();
            let state = ToolCallState {
                item_id: synthetic_id("fc"),
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
                // tool_search streams buffered: its arguments are a JSON
                // object on the wire, so the string-typed
                // function_call_arguments.* frames do not apply. The full
                // object goes out with output_item.done.
                let is_search = tool_name == TOOL_SEARCH_NAME;
                // Freeform tools are `custom_tool_call` items with a raw
                // `input`, not `function_call`: Codex's router builds
                // `ToolPayload::Custom` only from that item type.
                let is_freeform = self.freeform_tools.contains(&tool_name);
                if just_opened {
                    let seq = self.seq();
                    let item = if is_search {
                        json!({"id":item_id,"type":"tool_search_call","status":"in_progress",
                               "call_id":tool_call_id,"execution":"client","arguments":{}})
                    } else if is_freeform {
                        apply_namespace(
                            json!({"id":item_id,"type":"custom_tool_call","status":"in_progress",
                                   "call_id":tool_call_id,"name":tool_name,"input":""}),
                            &self.tool_namespaces,
                            &tool_name,
                        )
                    } else {
                        apply_namespace(
                            json!({"id":item_id,"type":"function_call","status":"in_progress",
                                   "call_id":tool_call_id,"name":tool_name,"arguments":""}),
                            &self.tool_namespaces,
                            &tool_name,
                        )
                    };
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
                // Freeform tools stream their input as a JSON wrapper; the
                // raw text is only known once the wrapper is complete, so the
                // intermediate deltas are buffered and the unwrapped value
                // goes out with the closing frames.
                if !args.is_empty() && !is_search && !is_freeform {
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
                    let is_search = name == TOOL_SEARCH_NAME;
                    // Freeform tools travelled as a JSON wrapper; Codex's
                    // freeform handler needs the raw input, so unwrap it now
                    // that the full arguments string is known.
                    let is_freeform = self.freeform_tools.contains(&name);
                    let final_arguments = if is_freeform {
                        unwrap_freeform_arguments(&arguments)
                    } else {
                        arguments.clone()
                    };
                    if !is_search && !is_freeform {
                        let seq = self.seq();
                        out.push(OutFrame {
                            event: Some("response.function_call_arguments.done".into()),
                            data: json!({
                                "type":"response.function_call_arguments.done","sequence_number":seq,
                                "item_id":item_id,"output_index":output_index,
                                "arguments":final_arguments.clone()
                            }),
                            done_marker: false,
                        });
                    }
                    let seq = self.seq();
                    let item = if is_search {
                        json!({"id":item_id,"type":"tool_search_call","status":"completed",
                               "call_id":call_id,"execution":"client",
                               "arguments":serde_json::from_str::<Value>(&arguments).unwrap_or(json!({}))})
                    } else if is_freeform {
                        apply_namespace(
                            custom_tool_call_item(&item_id, &call_id, &name, &final_arguments),
                            &self.tool_namespaces,
                            &name,
                        )
                    } else {
                        apply_namespace(
                            json!({"id":item_id,"type":"function_call","status":"completed",
                                   "call_id":call_id,"name":name,"arguments":final_arguments}),
                            &self.tool_namespaces,
                            &name,
                        )
                    };
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
