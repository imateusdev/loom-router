use super::*;
use serde_json::{json, Value};

#[test]
fn compaction_envelope_round_trips() {
    let encoded = encode_compaction_summary("did X, next Y");
    assert!(encoded.starts_with("lr1:"));
    assert_eq!(
        decode_compaction_summary(&encoded).as_deref(),
        Some("did X, next Y")
    );
    assert_eq!(decode_compaction_summary("gAAAAA-real-blob"), None);
}

#[test]
fn routed_compaction_replays_as_plain_user_text() {
    let payload = json!({
        "input": [{
            "type": "compaction",
            "encrypted_content": encode_compaction_summary("fixed bug, next run tests"),
        }]
    });
    let out = responses_to_chat(&payload, "m", false).unwrap();
    let msg = &out["messages"][0];
    assert_eq!(msg["role"], "user");
    let content = msg["content"].as_str().unwrap();
    assert!(content.contains("Another language model started"));
    assert!(content.contains("fixed bug, next run tests"));
}

#[test]
fn native_passthrough_keeps_real_compaction_blobs() {
    let mut payload = json!({
        "input": [
            {"type": "compaction", "encrypted_content": "gAAAAA-real-blob"},
            {"type": "compaction", "encrypted_content": encode_compaction_summary("ours")},
        ]
    });
    let changed = compaction_items_for_native(&mut payload);
    assert_eq!(changed, 1);
    assert_eq!(payload["input"][0]["type"], "compaction");
    assert_eq!(payload["input"][1]["type"], "message");
}

#[test]
fn flatten_agent_messages_turns_agent_message_into_a_plain_user_message() {
    let mut payload = json!({
        "input": [{
            "type": "agent_message",
            "author": "/root",
            "recipient": "/root/child",
            "content": [
                {"type":"input_text","text":"Message Type: NEW_TASK\nTask name: /root/child\nSender: /root\nPayload:\n"},
                {"type":"encrypted_content","encrypted_content":"Do the work."}
            ]
        }]
    });

    let touched = flatten_agent_messages(&mut payload);

    assert_eq!(touched, 2);
    let item = &payload["input"][0];
    assert_eq!(item["type"], "message");
    assert_eq!(item["role"], "user");
    let content = item["content"].as_array().unwrap();
    assert!(content
        .iter()
        .all(|part| part.get("type").and_then(Value::as_str) != Some("encrypted_content")));
    assert_eq!(content[0]["type"], "input_text");
    assert_eq!(content[1]["type"], "input_text");
    assert_eq!(content[1]["text"], "Do the work.");
}

#[test]
fn flatten_agent_messages_rewrites_encrypted_content_in_chat_payloads() {
    let mut payload = json!({
        "messages": [{
            "role": "user",
            "content": [
                {"type":"text","text":"Task:"},
                {"type":"encrypted_content","encrypted_content":"Please review it."}
            ]
        }]
    });

    let touched = flatten_agent_messages(&mut payload);

    assert_eq!(touched, 1);
    let content = payload["messages"][0]["content"].as_array().unwrap();
    assert!(content
        .iter()
        .all(|part| part.get("type").and_then(Value::as_str) != Some("encrypted_content")));
    assert_eq!(content[1]["type"], "text");
    assert_eq!(content[1]["text"], "Please review it.");
}

#[test]
fn flatten_agent_messages_reads_the_body_from_text_fallback() {
    let mut payload = json!({
        "input": [{
            "type": "agent_message",
            "author": "/root",
            "recipient": "/root/child",
            "content": [{
                "type": "encrypted_content",
                "text": "Body carried under text instead of encrypted_content."
            }]
        }]
    });

    flatten_agent_messages(&mut payload);

    let content = payload["input"][0]["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "input_text");
    assert_eq!(
        content[0]["text"],
        "Body carried under text instead of encrypted_content."
    );
}

#[test]
fn flatten_agent_messages_handles_multiple_encrypted_parts() {
    let mut payload = json!({
        "input": [{
            "type": "agent_message",
            "author": "/root",
            "recipient": "/root/child",
            "content": [
                {"type":"input_text","text":"Header\n"},
                {"type":"encrypted_content","encrypted_content":"First body"},
                {"type":"encrypted_content","encrypted_content":"Second body"}
            ]
        }]
    });

    let touched = flatten_agent_messages(&mut payload);

    assert_eq!(touched, 3);
    let content = payload["input"][0]["content"].as_array().unwrap();
    assert!(content
        .iter()
        .all(|part| part.get("type").and_then(Value::as_str) != Some("encrypted_content")));
    assert_eq!(content[1]["text"], "First body");
    assert_eq!(content[2]["text"], "Second body");
}

#[test]
fn flatten_agent_messages_removes_empty_encrypted_parts() {
    let mut payload = json!({
        "input": [{
            "type": "agent_message",
            "author": "/root",
            "recipient": "/root/child",
            "content": [
                {"type":"input_text","text":"Header
    "},
                {"type":"encrypted_content","encrypted_content":""},
                {"type":"encrypted_content","encrypted_content":"Real body"}
            ]
        }]
    });

    let touched = flatten_agent_messages(&mut payload);

    // touched: agent_message rewrite (1) + input_text kept (0) + empty removed (1) + real body flattened (1) = 3
    assert_eq!(touched, 3);
    let content = payload["input"][0]["content"].as_array().unwrap();
    // Only two parts remain: the header input_text and the real body
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["type"], "input_text");
    assert_eq!(content[1]["type"], "input_text");
    assert_eq!(content[1]["text"], "Real body");
    assert!(content
        .iter()
        .all(|part| part.get("type").and_then(Value::as_str) != Some("encrypted_content")));
}

#[test]
fn flatten_agent_messages_removes_multiple_empty_encrypted_parts_among_real_parts() {
    let mut payload = json!({
        "input": [{
            "type": "agent_message",
            "author": "/root",
            "recipient": "/root/child",
            "content": [
                {"type":"input_text","text":"Header\n"},
                {"type":"encrypted_content","encrypted_content":""},
                {"type":"encrypted_content","encrypted_content":"First body"},
                {"type":"encrypted_content","encrypted_content":""},
                {"type":"encrypted_content","encrypted_content":"Second body"},
                {"type":"encrypted_content","encrypted_content":""}
            ]
        }]
    });

    let touched = flatten_agent_messages(&mut payload);

    // touched: agent_message rewrite (1) + empty removed (3) + real bodies flattened (2)
    assert_eq!(touched, 6);
    let content = payload["input"][0]["content"].as_array().unwrap();
    assert_eq!(content.len(), 3);
    assert!(content
        .iter()
        .all(|part| part.get("type").and_then(Value::as_str) != Some("encrypted_content")));
    assert_eq!(content[0]["type"], "input_text");
    assert_eq!(content[1]["type"], "input_text");
    assert_eq!(content[1]["text"], "First body");
    assert_eq!(content[2]["type"], "input_text");
    assert_eq!(content[2]["text"], "Second body");
}

#[test]
fn flatten_agent_messages_removes_empty_encrypted_parts_in_chat_payloads() {
    let mut payload = json!({
        "messages": [{
            "role": "user",
            "content": [
                {"type":"text","text":"Header"},
                {"type":"encrypted_content","encrypted_content":""},
                {"type":"encrypted_content","encrypted_content":"Body"}
            ]
        }]
    });

    let touched = flatten_agent_messages(&mut payload);

    // touched: empty removed (1) + real body flattened (1)
    assert_eq!(touched, 2);
    let content = payload["messages"][0]["content"].as_array().unwrap();
    assert_eq!(content.len(), 2);
    assert!(content
        .iter()
        .all(|part| part.get("type").and_then(Value::as_str) != Some("encrypted_content")));
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[1]["type"], "text");
    assert_eq!(content[1]["text"], "Body");
}

#[test]
fn flatten_agent_messages_leaves_plain_payloads_untouched() {
    let mut payload = json!({
        "input": [{
            "role": "user",
            "content": [{"type":"input_text","text":"plain turn"}]
        }]
    });

    let touched = flatten_agent_messages(&mut payload);

    assert_eq!(touched, 0);
    assert_eq!(payload["input"][0]["role"], "user");
    assert_eq!(payload["input"][0]["content"][0]["type"], "input_text");
}

#[test]
fn responses_to_chat_delivers_a_plain_string_agent_message() {
    let payload = json!({
        "input": [{
            "type": "agent_message",
            "author": "/root",
            "recipient": "/root/child",
            "content": "Do the work."
        }]
    });

    let out = responses_to_chat(&payload, "k3", false).unwrap();

    assert_eq!(out["messages"][0]["role"], "user");
    assert_eq!(out["messages"][0]["content"], "Do the work.");
}
