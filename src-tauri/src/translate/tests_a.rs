use super::*;
use crate::translate::response::function_call_item;
use serde_json::{json, Value};
use std::collections::BTreeSet;

#[test]
fn minimax_reasoning_details_map_to_summary_not_message_text() {
    let chat = json!({
        "id": "chatcmpl-1",
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "answer",
                "reasoning_details": [{
                    "type": "reasoning.text",
                    "id": "r1",
                    "format": "openai-responses-v1",
                    "index": 0,
                    "text": "think"
                }]
            },
            "finish_reason": "stop"
        }]
    });
    let resp = chat_completion_to_responses(&chat, "minimax-m3");
    assert_eq!(resp["output"][0]["type"], "reasoning");
    assert_eq!(resp["output"][0]["summary"][0]["text"], "think");
    assert_eq!(
        resp["output"][0]["minimax_reasoning_details"][0]["id"],
        "r1"
    );
    assert_eq!(resp["output"][1]["content"][0]["text"], "answer");
}

#[test]
fn minimax_stream_reasoning_details_produce_summary_events() {
    let mut t = StreamTranslator::new(
        UpstreamKind::OpenAiChat,
        DownstreamKind::Responses,
        "minimax-m3",
    );
    let chunks = [
        json!({"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.text","id":"r1","format":"openai-responses-v1","index":0,"text":"thinking "}]},"finish_reason":null}]}),
        json!({"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.text","id":"r1","format":"openai-responses-v1","index":0,"text":"hard"}]},"finish_reason":null}]}),
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
    let done = types
        .iter()
        .find(|(e, _)| e == "response.reasoning_summary_text.done")
        .unwrap();
    assert_eq!(done.1["text"], "thinking hard");
    let msg_delta = types
        .iter()
        .find(|(e, _)| e == "response.output_text.delta")
        .unwrap();
    assert_eq!(msg_delta.1["delta"], "answer");
    let reasoning_done = types
        .iter()
        .find(|(e, data)| e == "response.output_item.done" && data["item"]["type"] == "reasoning")
        .unwrap();
    assert_eq!(
        reasoning_done.1["item"]["minimax_reasoning_details"][0]["text"],
        "thinking hard"
    );
}

#[test]
fn chat_stream_reasoning_produces_summary_events() {
    let mut t = StreamTranslator::new(UpstreamKind::OpenAiChat, DownstreamKind::Responses, "k3");
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
fn responses_function_tool_adapter_wraps_only_freeform_custom_tools() {
    let payload = json!({
        "tools": [
            {
                "type": "custom",
                "name": "apply_patch",
                "description": "Apply a patch",
                "format": {"type": "grammar", "syntax": "lark", "definition": "start: \"ok\""}
            },
            {
                "type": "function",
                "name": "ping",
                "description": "Ping",
                "parameters": {"type": "object", "properties": {}}
            }
        ]
    });

    let out = responses_with_function_tools(&payload);

    assert_eq!(out["tools"][0]["type"], "function");
    assert_eq!(out["tools"][0]["name"], "apply_patch");
    assert_eq!(out["tools"][0]["parameters"]["type"], "object");
    assert_eq!(out["tools"][0]["parameters"]["required"], json!(["input"]));
    assert!(out["tools"][0].get("format").is_none());
    assert_eq!(out["tools"][1], payload["tools"][1]);
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
fn agent_message_item_delivers_the_spawn_task() {
    // The real wire shape (ResponseItem::AgentMessage, produced by
    // InterAgentCommunication::to_model_input_item): a typed item with
    // author/recipient instead of role. Unknown typed items are dropped,
    // so the spawned child received environment + instructions and no
    // task at all — and reported exactly that.
    let payload = json!({
        "input": [{
            "type": "agent_message",
            "author": "/root",
            "recipient": "/root/architecture",
            "content": [
                {"type":"input_text","text":"Message Type: NEW_TASK\nTask name: /root/architecture\nSender: /root\nPayload:\n"},
                {"type":"encrypted_content","encrypted_content":"Trace the request path for tool_search in the loom-router workspace."}
            ]
        }]
    });
    let out = responses_to_chat(&payload, "k3", false).unwrap();
    let msg = &out["messages"][0];
    assert_eq!(msg["role"], "user");
    let content = msg["content"].as_str().unwrap();
    assert!(content.contains("NEW_TASK"), "header: {content}");
    assert!(
        content.contains("Trace the request path"),
        "payload body missing: {content}"
    );
}

#[test]
fn minted_ids_are_recognisable_as_ours() {
    // Every id the translator invents carries the marker.
    for prefix in ["rs", "msg", "fc", "resp", "tsc"] {
        let id = synthetic_id(prefix);
        assert!(id.starts_with(&format!("{prefix}_lr-")), "{id}");
        assert!(is_synthetic_item_id(&id), "{id}");
    }
    // The shape minted before the marker existed, still in saved threads:
    // a v4 UUID with the dashes stripped. This is the id from the bug
    // report.
    assert!(is_synthetic_item_id("rs_2296e1eb8a924d3091c787f430854d9a"));
    // Backend ids are left alone: wrong length, and no version/variant
    // nibbles where a stripped v4 UUID would have them.
    for theirs in [
        "rs_68b1f0a9c4d84e2f9a3b",
        "msg_0e5f2c1d",
        "rs_2296e1eb8a921d3091c787f430854d9a", // version nibble is not 4
        "rs_2296e1eb8a924d3011c787f430854d9a", // variant nibble is not 8-b
        "no-underscore",
    ] {
        assert!(!is_synthetic_item_id(theirs), "{theirs}");
    }
}

#[test]
fn the_native_backend_never_sees_an_id_it_did_not_issue() {
    // A thread that ran on a routed model and then switched to a native
    // one replays reasoning the translator invented. Sending it back is
    // what produces "Item with id 'rs_…' not found".
    let mut payload = json!({
        "model": "gpt-5.4-mini",
        "store": false,
        "input": [
            {"type": "message", "role": "user", "id": "msg_lr-aaaa", "content": "ola"},
            {"type": "reasoning", "id": "rs_2296e1eb8a924d3091c787f430854d9a",
             "summary": [{"type": "summary_text", "text": "thinking"}]},
            {"type": "reasoning", "id": "rs_68b1f0a9c4d84e2f9a3b",
             "summary": [{"type": "summary_text", "text": "theirs"}]},
            {"type": "message", "role": "assistant", "id": "msg_68c0", "content": "oi"}
        ]
    });
    assert_eq!(strip_synthetic_ids(&mut payload), 2);
    let input = payload["input"].as_array().unwrap();

    // Our reasoning item is gone; theirs is untouched, id and all.
    let ids: Vec<&str> = input
        .iter()
        .map(|i| i.get("id").and_then(Value::as_str).unwrap_or("-"))
        .collect();
    assert_eq!(ids, ["-", "rs_68b1f0a9c4d84e2f9a3b", "msg_68c0"]);
    // The user's message survives with its content — only the id it was
    // never going to be able to resolve is dropped.
    assert_eq!(input[0]["content"], "ola");
    assert_eq!(input[0]["role"], "user");
}

#[test]
fn a_stripped_tool_call_keeps_its_pairing() {
    // The shape a real switch-model turn produced: the contaminated items
    // were a function_call and a message, not reasoning. The call is
    // paired with its output by `call_id`, which is not an item id and
    // must survive — otherwise the backend sees an orphaned result.
    let mut payload = json!({
        "input": [
            {"type": "function_call", "id": "fc_lr-4775aa5e3fb2411cb77325c0fc6b9754",
             "call_id": "call_88", "name": "shell", "arguments": "{}"},
            {"type": "function_call_output", "id": "fco_019fdd6e-607a-7a11-b51e-26c751b141f9",
             "call_id": "call_88", "output": "ok"}
        ]
    });
    assert_eq!(strip_synthetic_ids(&mut payload), 1);
    let input = payload["input"].as_array().unwrap();
    assert_eq!(input.len(), 2);
    assert!(input[0].get("id").is_none());
    assert_eq!(input[0]["call_id"], "call_88");
    assert_eq!(input[0]["arguments"], "{}");
    // Codex mints dashed v7 UUIDs, which the legacy shape test must not
    // mistake for the dashless v4 the translator used to emit.
    assert_eq!(input[1]["id"], "fco_019fdd6e-607a-7a11-b51e-26c751b141f9");
}

#[test]
fn stripping_ignores_a_request_with_no_input() {
    let mut payload = json!({"model": "gpt-5.4-mini"});
    assert_eq!(strip_synthetic_ids(&mut payload), 0);
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
fn freeform_custom_tool_without_parameters_gets_a_valid_object_schema() {
    // Real Codex shape: apply_patch ships as a `custom` freeform tool
    // whose schema is a grammar, not a JSON object — there is no
    // `parameters` field. A verbatim clone would emit `{}`, and the strict
    // upstream rejects that with "schema must be a JSON Schema of
    // 'type: object', got 'type: null'".
    let payload = json!({
        "input": [{"role":"user","content":[{"type":"input_text","text":"hi"}]}],
        "tools": [{
            "type": "custom",
            "name": "apply_patch",
            "description": "Edit files.",
            "format": {"type":"grammar","syntax":"lark","definition":"start: hunk+"}
        }],
        "stream": true
    });
    let out = responses_to_chat(&payload, "m", false).unwrap();
    let tool = &out["tools"][0]["function"];
    assert_eq!(tool["name"], "apply_patch");
    assert_eq!(tool["parameters"]["type"], "object");
    // The freeform input is guided into a single string property so a
    // Chat model knows what to produce and the response path can unwrap.
    assert_eq!(tool["parameters"]["properties"]["input"]["type"], "string");
    assert_eq!(tool["parameters"]["required"][0], "input");
    // The description hint must not fight the freeform instruction.
    let desc = tool["description"].as_str().unwrap();
    assert!(desc.contains("raw input"), "{desc}");
}

#[test]
fn a_schemaless_function_tool_is_not_treated_as_freeform() {
    // Only `custom` tools are freeform. A `function` that ships no schema
    // is a zero-argument function: it must not be handed an `input`
    // argument it does not take, and — the reason this matters — it must
    // not land in the freeform set, or its call comes home as a
    // `custom_tool_call` and Codex aborts it as an unknown tool.
    let specs = vec![json!({"type":"function","name":"ping","description":"pong"})];
    let (chat, _namespaces, _replay, freeform) = flatten_tools(&specs);
    assert!(!freeform.contains("ping"), "{freeform:?}");
    // Still a valid object schema: a bare `{}` is what strict upstreams
    // reject, and an empty `properties` says "takes no arguments".
    assert_eq!(
        chat[0]["function"]["parameters"],
        json!({"type":"object","properties":{}})
    );
    assert_eq!(chat[0]["function"]["description"], "pong");
}

#[test]
fn union_root_tool_schemas_are_flattened_for_strict_providers() {
    let payload = json!({
        "input": [{"role":"user","content":[{"type":"input_text","text":"hi"}]}],
        "tools": [{
            "type": "function",
            "name": "automation_update",
            "description": "Update automations",
            "parameters": {
                "oneOf": [
                    {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]},
                    {"type": "object", "properties": {"mode": {"type": "string"}}, "required": ["mode"]}
                ],
                "required": ["common"]
            }
        }],
        "stream": false
    });

    let out = responses_to_chat(&payload, "m", false).unwrap();
    let params = &out["tools"][0]["function"]["parameters"];

    assert_eq!(params["type"], "object");
    assert_eq!(params["properties"]["id"]["type"], "string");
    assert_eq!(params["properties"]["mode"]["type"], "string");
    assert!(params["required"]
        .as_array()
        .unwrap()
        .contains(&json!("common")));
}

#[test]
fn whole_number_tool_arguments_are_coerced_before_codex_deserializes_them() {
    let item = function_call_item("call_1", "automation_update", "{\"limit\":20000.0}");

    assert_eq!(item["arguments"], "{\"limit\":20000}");
}

#[test]
fn freeform_tool_names_and_unwrap_round_trip() {
    let payload = json!({
        "input": [{"role":"user","content":[{"type":"input_text","text":"hi"}]}],
        "tools": [
            {"type":"function","name":"exec_command","description":"e","parameters":{"type":"object"}},
            {"type":"custom","name":"apply_patch","description":"Edit files."}
        ],
        "stream": true
    });
    assert!(freeform_tool_names(&payload).contains("apply_patch"));
    assert!(!freeform_tool_names(&payload).contains("exec_command"));

    // The model wraps the patch; the unwrap restores the raw text.
    let chat = json!({
        "id": "chatcmpl-1", "model": "m", "created": 1,
        "choices": [{
            "index": 0, "finish_reason": "tool_calls",
            "message": {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_0", "type": "function",
                 "function": {"name": "apply_patch", "arguments": "{\"input\":\"*** Begin Patch\\n\"}"}}
            ]}
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    });
    let mut resp = chat_completion_to_responses(&chat, "m");
    let output = resp
        .get_mut("output")
        .and_then(Value::as_array_mut)
        .unwrap();
    apply_namespaces_to_output(output, &tool_namespace_map(&payload));
    unwrap_freeform_to_output(output, &freeform_tool_names(&payload));
    let call = output
        .iter()
        .find(|i| i.get("type").and_then(Value::as_str) == Some("custom_tool_call"))
        .unwrap();
    assert_eq!(call["name"], "apply_patch");
    assert_eq!(call["input"], "*** Begin Patch\n");
    assert!(call.get("arguments").is_none());
}

#[test]
fn freeform_unwrap_leaves_non_wrapped_calls_untouched() {
    // A model that emits the raw input directly (not JSON) or a different
    // object shape must pass through unmodified, never mangled.
    let freeform: BTreeSet<String> = ["apply_patch".into()].into();
    let mut output = vec![
        json!({"type":"function_call","call_id":"a","name":"apply_patch","arguments":"*** raw patch ***"}),
        json!({"type":"function_call","call_id":"b","name":"apply_patch","arguments":"{\"other\":1}"}),
        json!({"type":"function_call","call_id":"c","name":"exec_command","arguments":"{\"input\":\"x\"}"}),
    ];
    unwrap_freeform_to_output(&mut output, &freeform);
    assert_eq!(output[0]["type"], "custom_tool_call");
    assert_eq!(output[0]["input"], "*** raw patch ***");
    assert_eq!(output[1]["type"], "custom_tool_call");
    assert_eq!(output[1]["input"], "{\"other\":1}");
    // exec_command is not freeform: it stays a function_call and its
    // input field is a real argument.
    assert_eq!(output[2]["type"], "function_call");
    assert_eq!(output[2]["arguments"], "{\"input\":\"x\"}");
}

#[test]
fn freeform_call_and_output_survive_the_next_request() {
    // After Codex runs apply_patch it replays the conversation, including
    // the model's call (`custom_tool_call`) and its result
    // (`custom_tool_call_output`). Both must reach the routed model: the
    // call re-wrapped in the shape it produced, the result as a tool
    // message — otherwise the model edits blind.
    let payload = json!({
        "input": [
            {"role":"user","content":[{"type":"input_text","text":"edit hello.sh"}]},
            {"type":"custom_tool_call","call_id":"call-7","name":"apply_patch",
             "input":"*** Begin Patch\n*** Update File: hello.sh\n@@\n- ola\n+ oi\n*** End Patch\n"},
            {"type":"custom_tool_call_output","call_id":"call-7",
             "output":"Files updated!\n*** Updated File: hello.sh\n"},
            {"role":"user","content":[{"type":"input_text","text":"and?"}]}
        ],
        "tools": [{"type":"custom","name":"apply_patch","description":"Edit files."}]
    });
    let out = responses_to_chat(&payload, "m", false).unwrap();
    let msgs = out["messages"].as_array().unwrap();
    let call = msgs
        .iter()
        .find_map(|m| m.get("tool_calls").and_then(Value::as_array))
        .expect("assistant tool_call");
    assert_eq!(call[0]["function"]["name"], "apply_patch");
    let args = call[0]["function"]["arguments"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(args).unwrap();
    assert_eq!(
        parsed["input"],
        "*** Begin Patch\n*** Update File: hello.sh\n@@\n- ola\n+ oi\n*** End Patch\n"
    );
    let tool = msgs
        .iter()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
        .expect("tool message answering apply_patch");
    assert_eq!(tool["tool_call_id"], "call-7");
    assert!(tool["content"].as_str().unwrap().contains("Updated File"));
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

fn chat_tool_names(out: &Value) -> Vec<&str> {
    out["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect()
}

#[test]
fn tool_search_and_discovered_tools_reach_the_model() {
    let out = responses_to_chat(&tool_search_payload(), "k3", false).unwrap();
    let names = chat_tool_names(&out);
    assert!(names.contains(&"exec_command"));
    // The discovery tool itself flattens into an ordinary function…
    assert!(names.contains(&"tool_search"), "{names:?}");
    // …and the specs the client-side search found are activated into the
    // tool list, which is the backend's job on the native path.
    assert!(names.contains(&"create_alert"), "{names:?}");
    assert!(names.contains(&"list_alerts"), "{names:?}");
    assert!(names.contains(&"get_current_time"), "{names:?}");
    assert_eq!(names.len(), 5, "{names:?}");
}

#[test]
fn tool_search_round_trip_is_visible_in_the_messages() {
    let out = responses_to_chat(&tool_search_payload(), "k3", false).unwrap();
    let msgs = out["messages"].as_array().unwrap();
    let call = msgs
        .iter()
        .find_map(|m| m.get("tool_calls").and_then(Value::as_array))
        .expect("assistant tool_call");
    assert_eq!(call[0]["function"]["name"], "tool_search");
    assert_eq!(call[0]["id"], "search-1");
    // Chat carries arguments as a string; the Responses item holds an
    // object, so the conversion must re-serialize, not wrap in quotes.
    let args = call[0]["function"]["arguments"].as_str().unwrap();
    assert!(args.contains("grafana alert"), "{args}");
    let answer = msgs
        .iter()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
        .expect("tool message answering the search");
    assert_eq!(answer["tool_call_id"], "search-1");
    let content = answer["content"].as_str().unwrap();
    assert!(content.contains("create_alert"), "{content}");
    assert!(content.contains("namespace mcp__grafana"), "{content}");
}

#[test]
fn discovered_namespaced_tools_join_the_namespace_map() {
    let map = tool_namespace_map(&tool_search_payload());
    assert_eq!(
        map.get("create_alert").map(String::as_str),
        Some("mcp__grafana")
    );
    assert!(!map.contains_key("tool_search"));
    assert!(!map.contains_key("exec_command"));
}

#[test]
fn rediscovering_the_same_tool_neither_duplicates_nor_prefixes_it() {
    // Two searches can return the same tool; the raw occurrence count
    // used to feed the collision logic, which would have prefixed the
    // name as if two different namespaces shared it.
    let mut payload = tool_search_payload();
    let duplicate = payload["input"][2].clone();
    payload["input"].as_array_mut().unwrap().push(duplicate);
    let out = responses_to_chat(&payload, "k3", false).unwrap();
    let names = chat_tool_names(&out);
    assert_eq!(
        names.iter().filter(|n| **n == "create_alert").count(),
        1,
        "{names:?}"
    );
}

#[test]
fn replayed_call_recovers_the_collision_prefixed_name() {
    // The model calls `b_ping` because two namespaces share `ping`. On
    // replay the call arrives as bare name + namespace, and sending the
    // bare name back would reference a tool the model never saw.
    let payload = json!({
        "input": [
            {"role":"user","content":[{"type":"input_text","text":"hi"}]},
            {"type":"function_call","call_id":"c1","namespace":"b","name":"ping","arguments":"{}"}
        ],
        "tools": [
            {"type":"namespace","name":"a","tools":[
                {"type":"function","name":"ping","description":"a","parameters":{"type":"object"}}
            ]},
            {"type":"namespace","name":"b","tools":[
                {"type":"function","name":"ping","description":"b","parameters":{"type":"object"}}
            ]}
        ]
    });
    let out = responses_to_chat(&payload, "k3", false).unwrap();
    let names = chat_tool_names(&out);
    assert!(
        names.contains(&"a_ping") && names.contains(&"b_ping"),
        "{names:?}"
    );
    let msgs = out["messages"].as_array().unwrap();
    let call = msgs
        .iter()
        .find_map(|m| m.get("tool_calls").and_then(Value::as_array))
        .expect("assistant tool_call");
    assert_eq!(call[0]["function"]["name"], "b_ping");
}

#[test]
fn tool_search_call_comes_back_in_the_shape_codex_dispatches() {
    let chat = json!({
        "id":"chatcmpl-1",
        "choices":[{
            "index":0,
            "message":{"role":"assistant","content":null,"tool_calls":[{
                "id":"search-1","type":"function",
                "function":{"name":"tool_search","arguments":"{\"query\":\"grafana alert\",\"limit\":5}"}
            }]},
            "finish_reason":"tool_calls"
        }]
    });
    let out = chat_completion_to_responses(&chat, "k3");
    let item = &out["output"][0];
    // Codex only routes the call to its client-side BM25 handler in this
    // exact shape: type + execution "client", arguments as an object.
    assert_eq!(item["type"], "tool_search_call");
    assert_eq!(item["execution"], "client");
    assert_eq!(item["call_id"], "search-1");
    assert_eq!(item["arguments"]["query"], "grafana alert");
    assert!(item["arguments"].is_object());
}

#[test]
fn streamed_tool_search_call_skips_the_string_argument_frames() {
    let mut t = StreamTranslator::new(UpstreamKind::OpenAiChat, DownstreamKind::Responses, "m");
    t.push_event(None, r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"search-1","function":{"name":"tool_search","arguments":""}}]},"finish_reason":null}]}"#);
    t.push_event(None, r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"query\":\"grafana\"}"}}]},"finish_reason":null}]}"#);
    let done = t.push_event(
        None,
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
    );
    // arguments is a JSON object on this item type, so the string-typed
    // function_call_arguments.* frames must not be emitted for it.
    assert!(!done
        .iter()
        .any(|f| f.event.as_deref() == Some("response.function_call_arguments.done")));
    let item_done = done
        .iter()
        .find(|f| f.event.as_deref() == Some("response.output_item.done"))
        .expect("output_item.done frame");
    assert_eq!(item_done.data["item"]["type"], "tool_search_call");
    assert_eq!(item_done.data["item"]["execution"], "client");
    assert_eq!(item_done.data["item"]["arguments"]["query"], "grafana");
}

#[test]
fn streamed_tool_call_restores_the_namespace_codex_resolves_against() {
    // Without this the call comes back namespace-less, Codex resolves it
    // against `functions`, and no collaboration handler is registered
    // there — the call fails as an unknown tool.
    let mut t = StreamTranslator::new(UpstreamKind::OpenAiChat, DownstreamKind::Responses, "k3")
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
fn streamed_freeform_tool_call_comes_back_as_custom_tool_call() {
    // apply_patch travels as a Chat function whose arguments are the JSON
    // wrapper `{"input": "<patch>"}`. The closing frames must hand Codex a
    // `custom_tool_call` item carrying the raw patch — its router builds
    // `ToolPayload::Custom` only from that item type, and its freeform
    // handler parses patches, not JSON.
    let freeform: BTreeSet<String> = ["apply_patch".into()].into();
    let mut t = StreamTranslator::new(UpstreamKind::OpenAiChat, DownstreamKind::Responses, "m")
        .with_freeform_tools(freeform);
    let first = json!({"choices":[{"index":0,
            "delta":{"tool_calls":[{"index":0,"id":"c1","type":"function",
                "function":{"name":"apply_patch","arguments":"{\"input\":\"*** Begin Patch\\n"}}]},
            "finish_reason":null}]});
    let second = json!({"choices":[{"index":0,
            "delta":{"tool_calls":[{"index":0,"function":{"arguments":"*** End Patch\\n\"}"}}]},
            "finish_reason":null}]});
    let mut done = t.push_event(None, &first.to_string());
    done.extend(t.push_event(None, &second.to_string()));
    done.extend(t.push_event(
        None,
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
    ));

    // Wrapper deltas are buffered, not streamed: the client would see the
    // raw patch in the closing frames, so no partial JSON may leak.
    assert!(!done
        .iter()
        .any(|f| f.event.as_deref() == Some("response.function_call_arguments.delta")));
    assert!(!done
        .iter()
        .any(|f| f.event.as_deref() == Some("response.function_call_arguments.done")));
    let added = done
        .iter()
        .find(|f| f.event.as_deref() == Some("response.output_item.added"))
        .expect("output_item.added frame");
    assert_eq!(added.data["item"]["type"], "custom_tool_call");
    let item_done = done
        .iter()
        .find(|f| f.event.as_deref() == Some("response.output_item.done"))
        .expect("output_item.done frame");
    assert_eq!(item_done.data["item"]["type"], "custom_tool_call");
    assert_eq!(item_done.data["item"]["name"], "apply_patch");
    assert_eq!(
        item_done.data["item"]["input"],
        "*** Begin Patch\n*** End Patch\n"
    );
    assert!(item_done.data["item"].get("arguments").is_none());
    // Both frames describe one output item, so they must agree on its id:
    // a client that correlates added/done by id would otherwise see the
    // opening item abandoned and a second one appear from nowhere.
    assert_eq!(added.data["item"]["id"], item_done.data["item"]["id"]);
}

#[test]
fn native_responses_function_wrapper_comes_back_as_custom_tool_call() {
    let freeform: BTreeSet<String> = ["apply_patch".into()].into();
    let mut translator =
        StreamTranslator::new(UpstreamKind::Responses, DownstreamKind::Responses, "m")
            .with_freeform_tools(freeform);

    let added = translator.push_event(
            Some("response.output_item.added"),
            r#"{"type":"response.output_item.added","output_index":0,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"apply_patch","arguments":"","status":"in_progress"}}"#,
        );
    let delta = translator.push_event(
            Some("response.function_call_arguments.delta"),
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":0,"delta":"{\"input\":\"*** Begin Patch\\n"}"#,
        );
    let arguments_done = translator.push_event(
            Some("response.function_call_arguments.done"),
            r#"{"type":"response.function_call_arguments.done","item_id":"fc_1","output_index":0,"name":"apply_patch","arguments":"{\"input\":\"*** Begin Patch\\n*** End Patch\\n\"}"}"#,
        );
    let item_done = translator.push_event(
            Some("response.output_item.done"),
            r#"{"type":"response.output_item.done","output_index":0,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"apply_patch","arguments":"{\"input\":\"*** Begin Patch\\n*** End Patch\\n\"}","status":"completed"}}"#,
        );

    assert_eq!(added.len(), 1);
    assert_eq!(added[0].data["item"]["type"], "custom_tool_call");
    assert_eq!(added[0].data["item"]["input"], "");
    assert!(added[0].data["item"].get("arguments").is_none());
    assert!(delta.is_empty());
    assert!(arguments_done.is_empty());
    assert_eq!(item_done.len(), 1);
    assert_eq!(item_done[0].data["item"]["type"], "custom_tool_call");
    assert_eq!(
        item_done[0].data["item"]["input"],
        "*** Begin Patch\n*** End Patch\n"
    );
    assert!(item_done[0].data["item"].get("arguments").is_none());
}
