use super::*;
use serde_json::{json, Value};

/// A request in the deferred-loading shape: only `tool_search` (plus the
/// direct core tools) is advertised up front, and whatever an earlier
/// search discovered arrives as a `tool_search_output` item in the input.
fn tool_search_payload() -> Value {
    json!({
        "input": [
            {"role":"user","content":[{"type":"input_text","text":"create a grafana alert"}]},
            {"type":"tool_search_call","call_id":"search-1","execution":"client",
             "arguments":{"query":"grafana alert","limit":5}},
            {"type":"tool_search_output","call_id":"search-1","status":"completed","execution":"client",
             "tools":[
                {"type":"namespace","name":"mcp__grafana","description":"Grafana","tools":[
                    {"type":"function","name":"create_alert","description":"Create an alert rule",
                     "defer_loading":true,
                     "parameters":{"type":"object","properties":{"name":{"type":"string"}}}},
                    {"type":"function","name":"list_alerts","description":"List alert rules",
                     "defer_loading":true,"parameters":{"type":"object"}}
                ]},
                {"type":"function","name":"get_current_time","description":"now",
                 "defer_loading":true,"parameters":{"type":"object"}}
             ]}
        ],
        "tools": [
            {"type":"function","name":"exec_command","description":"e","parameters":{"type":"object"}},
            {"type":"tool_search","execution":"client","description":"# Tool discovery",
             "parameters":{"type":"object","properties":{"query":{"type":"string"},"limit":{"type":"number"}},"required":["query"]}}
        ]
    })
}

#[test]
fn non_streamed_tool_call_restores_the_namespace_too() {
    // chat_completion_to_responses has no request at hand, so proxy.rs
    // applies the namespace map afterwards — same gap as the streaming
    // path above, one hop later. A tool discovered via tool_search gets
    // its namespace back the same way.
    let payload = tool_search_payload();
    let chat = json!({
        "id":"chatcmpl-2",
        "choices":[{
            "index":0,
            "message":{"role":"assistant","content":null,"tool_calls":[{
                "id":"call-9","type":"function",
                "function":{"name":"create_alert","arguments":"{\"name\":\"cpu-high\"}"}
            }]},
            "finish_reason":"tool_calls"
        }]
    });
    let mut out = chat_completion_to_responses(&chat, "k3");
    let output = out["output"].as_array_mut().unwrap();
    apply_namespaces_to_output(output, &tool_namespace_map(&payload));
    let item = &out["output"][0];
    assert_eq!(item["type"], "function_call");
    assert_eq!(item["name"], "create_alert");
    assert_eq!(item["namespace"], "mcp__grafana");
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
fn responses_to_chat_drops_orphan_tool_output() {
    // A truncated/lost conversation can replay an output without its call.
    // Sending that to Console Go fails with a tool-pairing 400, so the
    // translator must drop the incomplete result.
    let payload = json!({
        "input": [
            {"role":"user","content":[{"type":"input_text","text":"continue"}]},
            {"type":"function_call_output","call_id":"orphan-output","output":"result"},
            {"role":"assistant","content":[{"type":"output_text","text":"done"}]}
        ]
    });
    let out = responses_to_chat(&payload, "kimi-k3", false).unwrap();
    let msgs = out["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2, "{msgs:?}");
    assert!(msgs
        .iter()
        .all(|m| m.get("role").and_then(Value::as_str) != Some("tool")));
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
fn minimax_reasoning_round_trips_as_reasoning_details() {
    let payload = json!({
        "model": "minimax-m3",
        "input": [
            {"role":"user","content":[{"type":"input_text","text":"weather?"}]},
            {
                "type": "reasoning",
                "summary": [{"type": "summary_text", "text": "need the tool"}],
                "minimax_reasoning_details": [{
                    "type": "reasoning.text",
                    "id": "r1",
                    "format": "openai-responses-v1",
                    "index": 0,
                    "text": "need the tool"
                }]
            },
            {"type":"function_call","call_id":"c1","name":"get_weather","arguments":"{}"},
            {"type":"function_call_output","call_id":"c1","output":"sunny"},
            {
                "type": "reasoning",
                "summary": [{"type": "summary_text", "text": "got it"}],
                "minimax_reasoning_details": [{
                    "type": "reasoning.text",
                    "id": "r2",
                    "format": "openai-responses-v1",
                    "index": 0,
                    "text": "got it"
                }]
            },
            {"role":"assistant","content":[{"type":"output_text","text":"Sunny today"}]}
        ]
    });
    let out = responses_to_chat(&payload, "minimax-m3", false).unwrap();
    let msgs = out["messages"].as_array().unwrap();
    let tool_call_msg = &msgs[1];
    assert_eq!(
        tool_call_msg["reasoning_details"][0]["text"],
        "need the tool"
    );
    assert!(tool_call_msg.get("reasoning_content").is_none());
    let assistant_msg = &msgs[3];
    assert_eq!(assistant_msg["reasoning_details"][0]["text"], "got it");
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

#[test]
fn extract_text_reads_openai_chat_content() {
    let payload = json!({
        "choices": [{"message": {"role": "assistant", "content": "openai answer"}}]
    });
    assert_eq!(
        extract_text(UpstreamKind::OpenAiChat, &payload).as_deref(),
        Some("openai answer")
    );
}

#[test]
fn extract_text_joins_anthropic_text_blocks() {
    let payload = json!({
        "content": [
            {"type": "text", "text": "first "},
            {"type": "text", "text": "second"},
            {"type": "tool_use", "name": "x"}
        ]
    });
    assert_eq!(
        extract_text(UpstreamKind::Anthropic, &payload).as_deref(),
        Some("first \nsecond")
    );
}

#[test]
fn extract_text_joins_responses_output_blocks() {
    let payload = json!({
        "output": [
            {"type": "message", "content": [{"type": "output_text", "text": "line one"}]},
            {"type": "function_call", "content": [{"type": "output_text", "text": "line two"}]}
        ]
    });
    assert_eq!(
        extract_text(UpstreamKind::Responses, &payload).as_deref(),
        Some("line one\nline two")
    );
}

#[test]
fn extract_text_returns_none_for_error_or_empty_envelopes() {
    assert_eq!(
        extract_text(
            UpstreamKind::OpenAiChat,
            &json!({"error": {"message": "boom"}})
        ),
        None
    );
    assert_eq!(
        extract_text(UpstreamKind::Anthropic, &json!({"content": []})),
        None
    );
    assert_eq!(
        extract_text(UpstreamKind::Responses, &json!({"output": []})),
        None
    );
    assert_eq!(extract_text(UpstreamKind::OpenAiChat, &json!({})), None);
}
#[test]
fn responses_to_chat_converts_images_in_tool_output() {
    // Codex's view_image tool returns the image inside the call output,
    // not in a message. Copying that array verbatim shipped a Responses
    // `input_image` part to a Chat Completions upstream, which rejected
    // the whole request: "messages[N]: unknown variant `input_image`".
    let payload = json!({
        "input": [
            {"role":"user","content":[{"type":"input_text","text":"look"}]},
            {"type":"function_call","call_id":"view_image:1","name":"view_image","arguments":"{}"},
            {"type":"function_call_output","call_id":"view_image:1","output":[
                {"type":"input_text","text":"here it is"},
                {"type":"input_image","image_url":"data:image/png;base64,aGVsbG8="}
            ]}
        ]
    });
    let out = responses_to_chat(&payload, "deepseek-v4-flash", false).unwrap();
    let dumped = serde_json::to_string(&out["messages"]).unwrap();
    assert!(
        !dumped.contains("input_image"),
        "no Responses-only part may reach a chat upstream: {dumped}"
    );
    assert!(
        !dumped.contains("input_text"),
        "no Responses-only part may reach a chat upstream: {dumped}"
    );
}

#[test]
fn chat_to_anthropic_converts_image_parts_to_source_blocks() {
    // Anthropic has no `image_url` part; forwarding one verbatim breaks
    // every vision model served over that protocol (minimax, qwen).
    let payload = json!({
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "look"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,aGVsbG8="}},
            {"type": "image_url", "image_url": {"url": "https://images.example/a.png"}}
        ]}]
    });

    let out = chat_to_anthropic(&payload, "minimax-m3").unwrap();
    let blocks = out["messages"][0]["content"].as_array().unwrap();

    assert_eq!(blocks[1]["type"], "image");
    assert_eq!(blocks[1]["source"]["type"], "base64");
    assert_eq!(blocks[1]["source"]["media_type"], "image/png");
    assert_eq!(blocks[1]["source"]["data"], "aGVsbG8=");
    assert_eq!(blocks[2]["source"]["type"], "url");
    assert_eq!(blocks[2]["source"]["url"], "https://images.example/a.png");
    let dumped = serde_json::to_string(&out).unwrap();
    assert!(!dumped.contains("image_url"), "{dumped}");
}

#[test]
fn responses_to_chat_keeps_tool_pairing_when_two_tools_return_images() {
    // The hoisted user message must land after BOTH tool results: strict
    // providers reject a user message sitting between an assistant's tool
    // calls and the results answering them.
    let payload = json!({
        "input": [
            {"role":"user","content":[{"type":"input_text","text":"look"}]},
            {"type":"function_call","call_id":"view_image:1","name":"view_image","arguments":"{}"},
            {"type":"function_call","call_id":"view_image:2","name":"view_image","arguments":"{}"},
            {"type":"function_call_output","call_id":"view_image:1","output":[
                {"type":"input_image","image_url":"data:image/png;base64,YQ=="}
            ]},
            {"type":"function_call_output","call_id":"view_image:2","output":[
                {"type":"input_image","image_url":"data:image/png;base64,Yg=="}
            ]}
        ]
    });
    let out = responses_to_chat(&payload, "kimi-k3", false).unwrap();
    let msgs = out["messages"].as_array().unwrap();
    let roles: Vec<&str> = msgs
        .iter()
        .map(|m| m["role"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        roles,
        vec!["user", "assistant", "tool", "tool", "user"],
        "{msgs:?}"
    );
    let hoisted = msgs[4]["content"].as_array().unwrap();
    assert_eq!(hoisted.len(), 2, "both images ride along: {msgs:?}");
    assert_eq!(hoisted[0]["type"], "image_url");
    let dumped = serde_json::to_string(&out["messages"]).unwrap();
    assert!(!dumped.contains(TOOL_MEDIA_KEY), "marker leaked: {dumped}");
}
