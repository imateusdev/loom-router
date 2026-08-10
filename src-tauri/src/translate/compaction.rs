use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Routed compaction envelopes
//
// Codex remote compaction v2 sends a normal `/v1/responses` turn ending in
// `{"type":"compaction_trigger"}` and requires the reply stream to contain
// exactly one `{"type":"compaction","encrypted_content":...}` output item.
// Routed providers cannot produce OpenAI's encrypted blob, so LoomRouter
// returns a transparent envelope. The marker keeps that summary out of the
// native ChatGPT backend, which would try to decrypt it as its own blob.
// ---------------------------------------------------------------------------

pub const COMPACTION_PROMPT: &str = "You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary for another LLM that will resume the task.

Include:
- Current progress and key decisions made
- Important context, constraints, or user preferences
- What remains to be done (clear next steps)
- Any critical data, examples, or references needed to continue

Be concise, structured, and focused on helping the next LLM seamlessly continue the work.";

pub const COMPACTION_SUMMARY_PREFIX: &str = "Another language model started to solve this problem and produced a summary of its thinking process. You also have access to the state of the tools that were used by that language model. Use this to build on the work that has already been done and avoid duplicating work. Here is the summary produced by the other language model, use the information in this summary to assist with your own analysis:";

pub const OPAQUE_COMPACTION_NOTE: &str = "[earlier conversation was compacted; the summary is stored in a format this model cannot read]";

const COMPACTION_MARKER: &str = "lr1:";

/// Wrap a routed summary in the envelope LoomRouter can decode later.
pub fn encode_compaction_summary(summary: &str) -> String {
    format!("{COMPACTION_MARKER}{}", STANDARD.encode(summary))
}

/// Decode a summary produced by [`encode_compaction_summary`].
pub fn decode_compaction_summary(encrypted_content: &str) -> Option<String> {
    let encoded = encrypted_content.strip_prefix(COMPACTION_MARKER)?;
    let bytes = STANDARD.decode(encoded).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    (!text.is_empty()).then_some(text)
}

pub(super) fn is_compaction_item(item: &Value) -> bool {
    matches!(
        item.get("type").and_then(Value::as_str),
        Some("compaction") | Some("compaction_summary") | Some("context_compaction")
    )
}

pub(super) fn compaction_item_text(encrypted_content: Option<&str>) -> String {
    encrypted_content
        .and_then(decode_compaction_summary)
        .unwrap_or_else(|| OPAQUE_COMPACTION_NOTE.to_string())
}

fn compaction_as_user_message(encrypted_content: Option<&str>) -> Value {
    json!({
        "type": "message",
        "role": "user",
        "content": [{
            "type": "input_text",
            "text": format!("{COMPACTION_SUMMARY_PREFIX}\n\n{}", compaction_item_text(encrypted_content)),
        }],
    })
}

/// Convert every compaction item in a routed Responses payload to plain text.
/// Only the native ChatGPT backend understands the real encrypted type.
pub fn compaction_items_for_routed(payload: &mut Value) -> usize {
    let Some(input) = payload.get_mut("input").and_then(Value::as_array_mut) else {
        return 0;
    };
    let mut out = Vec::with_capacity(input.len());
    let mut changed = 0;
    for item in input.drain(..) {
        if is_compaction_item(&item) {
            out.push(compaction_as_user_message(
                item.get("encrypted_content").and_then(Value::as_str),
            ));
            changed += 1;
        } else {
            out.push(item);
        }
    }
    *input = out;
    changed
}

/// Convert only LoomRouter's transparent compaction envelopes before a native
/// passthrough; real OpenAI-encrypted items stay untouched for the backend.
pub fn compaction_items_for_native(payload: &mut Value) -> usize {
    let Some(input) = payload.get_mut("input").and_then(Value::as_array_mut) else {
        return 0;
    };
    let mut out = Vec::with_capacity(input.len());
    let mut changed = 0;
    for item in input.drain(..) {
        let ours = is_compaction_item(&item)
            && item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .and_then(decode_compaction_summary)
                .is_some();
        if ours {
            out.push(compaction_as_user_message(
                item.get("encrypted_content").and_then(Value::as_str),
            ));
            changed += 1;
        } else {
            out.push(item);
        }
    }
    *input = out;
    changed
}

// ---------------------------------------------------------------------------
