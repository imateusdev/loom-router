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
use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------
// Synthetic item ids
// ---------------------------------------------------------------------------

/// Marker carried by every item id LoomRouter mints.
///
/// A Chat or Anthropic upstream returns no Responses item ids, so the
/// translator invents them. They are real only inside this process: the agent
/// keeps them in its thread history, and if that history is later replayed to
/// a backend that resolves ids - OpenAI's, when the user switches the thread
/// to a native model - the backend answers 404 for an id it never issued.
/// The marker is what lets the native passthrough tell those apart from ids
/// the backend really did hand out. `-` cannot occur in the hex that follows
/// a real id's prefix, so the pair is unambiguous.
const SYNTHETIC_ID_MARKER: &str = "lr-";

/// Mint an item id under `prefix` (`rs`, `msg`, `fc`, …), marked as ours.
fn synthetic_id(prefix: &str) -> String {
    format!(
        "{prefix}_{SYNTHETIC_ID_MARKER}{}",
        uuid::Uuid::new_v4().simple()
    )
}

/// Whether an item id was minted here rather than by an upstream.
///
/// Marked ids are recognised outright. Ids minted before the marker existed
/// are still sitting in saved threads, so they get a narrower test: the
/// translator rendered a v4 UUID with its dashes stripped, which is 32 hex
/// digits whose version and variant nibbles are pinned. An id that satisfies
/// all three and came from a backend would be a coincidence; treating it as
/// ours costs one replayed reasoning summary.
pub fn is_synthetic_item_id(id: &str) -> bool {
    let Some((_, body)) = id.split_once('_') else {
        return false;
    };
    if body.starts_with(SYNTHETIC_ID_MARKER) {
        return true;
    }
    let hex: Vec<char> = body.chars().collect();
    hex.len() == 32
        && hex.iter().all(char::is_ascii_hexdigit)
        && hex[12] == '4'
        && matches!(hex[16], '8' | '9' | 'a' | 'b')
}

/// Strip the ids this process invented out of a request bound for a backend
/// that resolves them.
///
/// Only reasoning items are dropped whole: their body is a summary the
/// translator built from an upstream that has no Responses equivalent, so it
/// carries nothing the backend can use, and leaving it in is what produces
/// "Item with id 'rs_…' not found". Every other item keeps its content and
/// loses just the id, so the turn still reads as input rather than as a
/// reference to something stored. Returns how many items it touched.
pub fn strip_synthetic_ids(payload: &mut Value) -> usize {
    let Some(input) = payload.get_mut("input").and_then(Value::as_array_mut) else {
        return 0;
    };
    let before = input.len();
    input.retain(|item| {
        let ours = item
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(is_synthetic_item_id);
        !(ours && item.get("type").and_then(Value::as_str) == Some("reasoning"))
    });
    let mut touched = before - input.len();
    for item in input.iter_mut() {
        let ours = item
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(is_synthetic_item_id);
        if ours {
            if let Some(map) = item.as_object_mut() {
                map.remove("id");
                touched += 1;
            }
        }
    }
    touched
}

// ---------------------------------------------------------------------------
// Request translation
// ---------------------------------------------------------------------------

/// Responses API payload -> Chat Completions payload.
/// Flatten the Responses tool list into Chat Completions functions, and
/// report which namespace each flattened name came from.
///
/// Codex sends five tool shapes: `function`, `custom`, `namespace`,
/// `tool_search` and `web_search`. Chat Completions has exactly one, and no
/// field to carry a namespace - the Responses protocol keeps `namespace` and
/// `name` as separate fields on a function call, which is why the round trip
/// needs this map instead of a naming convention.
///
/// `tool_search` is deferred tool loading: Codex advertises only the search
/// tool, runs the search locally (BM25 over its registry) when the model
/// calls it with `execution: "client"`, and lists the discovered specs in a
/// `tool_search_output` item in the NEXT request's input. On the native path
/// the Responses backend activates those tools; a routed model has no such
/// backend, so the proxy plays that role - see [`all_tool_specs`]. The spec
/// itself flattens into an ordinary Chat function here.
const TOOL_SEARCH_NAME: &str = "tool_search";
///
/// Encoding the namespace into the name does not work here. A real request
/// carries `namespace[mcp__codex_apps__codex_document_control]` holding
/// `_get_docum_83c7f0565c0f`: the namespace already contains `__` and the
/// tool name opens with `_`, so any concatenation produces a run of
/// underscores that no split can undo. Names stay untouched, and collisions
/// between namespaces are the only case that gets a prefix.
///
/// Returns the chat-shaped tools, a `flattened name -> namespace` map, and a
/// `(namespace, bare name) -> flattened name` map. The third is the inverse
/// view: replayed `function_call` items and `tool_search_output` listings
/// refer to tools by bare name + namespace and must be re-flattened into the
/// exact names the model saw.
///
/// `web_search` is dropped: it is executed by the Responses backend, not by
/// the model, and has no Chat equivalent. Duplicate `(namespace, name)`
/// specs - the same tool found by two `tool_search` calls - collapse into
/// one entry and, unlike distinct namespaces sharing a bare name, do not
/// trigger the collision prefix.
#[allow(clippy::type_complexity)]
fn flatten_tools(
    tools: &[Value],
) -> (
    Vec<Value>,
    BTreeMap<String, String>,
    BTreeMap<(String, String), String>,
    BTreeSet<String>,
) {
    // Chat Completions requires every function's `parameters` to be a JSON
    // Schema rooted at `type: "object"`; strict upstreams (e.g. OpenCode Go)
    // 400 anything else. Codex's `custom` tools - apply_patch ships as one -
    // are freeform and carry no `parameters` at all (their schema is a
    // grammar), so a verbatim clone would emit `{}`/`null` and get rejected.
    // Freeform tools are given a string wrapper ([`tool_parameters`]); the
    // description also gets a hint so the "do not wrap in JSON" instruction
    // from the tool's own docs does not fight the wrapper. A `function` tool
    // that ships no usable schema is not freeform - it gets the empty object
    // schema, which is what a zero-argument function means.
    let as_chat = |name: &str, t: &Value, freeform: bool| {
        let description = t.get("description").cloned().unwrap_or(Value::Null);
        let description = if freeform {
            match description {
                Value::String(s) => Value::String(format!(
                    "{s}\n\nThe {FREEFORM_INPUT_FIELD} field carries the tool's entire raw input."
                )),
                other => other,
            }
        } else {
            description
        };
        json!({
            "type": "function",
            "function": {
                "name": name,
                "description": description,
                "parameters": tool_parameters(t, freeform),
            }
        })
    };

    // Which bare names appear under more than one namespace ("" counts as the
    // default namespace of plain function/custom tools). Distinct namespaces
    // per name, not raw occurrence count: a duplicated spec must not prefix
    // itself. Computed up front so both the request and the response derive
    // the same names from the same payload.
    let mut seen: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for t in tools {
        match t.get("type").and_then(Value::as_str) {
            Some("function") | Some("custom") => {
                if let Some(n) = t.get("name").and_then(Value::as_str) {
                    seen.entry(n).or_default().insert("");
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
                    if let Some(n) = inner.get("name").and_then(Value::as_str) {
                        seen.entry(n).or_default().insert(ns);
                    }
                }
            }
            _ => {}
        }
    }

    let mut out = Vec::new();
    let mut namespaces = BTreeMap::new();
    let mut replay = BTreeMap::new();
    let mut freeform = BTreeSet::new();
    let mut emitted: BTreeSet<(&str, &str)> = BTreeSet::new();
    for t in tools {
        match t.get("type").and_then(Value::as_str) {
            // `custom` is a freeform tool (apply_patch ships as one). It was
            // being dropped alongside the namespaces.
            Some("function") | Some("custom") => {
                if let Some(n) = t.get("name").and_then(Value::as_str) {
                    if emitted.insert(("", n)) {
                        let is_freeform = is_freeform_tool(t);
                        out.push(as_chat(n, t, is_freeform));
                        if is_freeform {
                            freeform.insert(n.to_string());
                        }
                        replay.insert((String::new(), n.to_string()), n.to_string());
                    }
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
                    if !emitted.insert((ns, n)) {
                        continue;
                    }
                    let flat = if seen.get(n).map(|s| s.len()).unwrap_or(0) > 1 {
                        format!("{ns}_{n}")
                    } else {
                        n.to_string()
                    };
                    let is_freeform = is_freeform_tool(inner);
                    out.push(as_chat(&flat, inner, is_freeform));
                    if is_freeform {
                        freeform.insert(flat.clone());
                    }
                    replay.insert((ns.to_string(), n.to_string()), flat.clone());
                    if !ns.is_empty() {
                        namespaces.insert(flat, ns.to_string());
                    }
                }
            }
            // Deferred tool discovery runs client-side in Codex, so unlike
            // web_search it DOES have a Chat equivalent: an ordinary function
            // the model calls with {query, limit}. Its call is translated
            // back into a `tool_search_call` on the response path.
            Some("tool_search") if emitted.insert(("", TOOL_SEARCH_NAME)) => {
                out.push(as_chat(TOOL_SEARCH_NAME, t, false));
                replay.insert(
                    (String::new(), TOOL_SEARCH_NAME.to_string()),
                    TOOL_SEARCH_NAME.to_string(),
                );
            }
            _ => {}
        }
    }
    (out, namespaces, replay, freeform)
}

/// The single string property a freeform tool's Chat schema wraps its raw
/// input in. [`unwrap_freeform_arguments`] reverses it on the response path so
/// Codex's freeform handler receives the raw input, not the JSON wrapper.
const FREEFORM_INPUT_FIELD: &str = "input";

/// Whether a tool spec is freeform: a `custom` tool whose input is raw text
/// (apply_patch ships as one). Its schema is a grammar, so `parameters` is
/// absent or not rooted at `type: "object"` - unusable as a Chat function
/// schema as-is.
///
/// Both halves are load-bearing. Keying on the missing schema alone would
/// sweep in a plain `function` tool that ships no parameters: it would be
/// handed an `input` argument it does not take, and its call would come home
/// as a `custom_tool_call`, which Codex routes to a freeform handler and
/// aborts as unknown - the exact failure this path exists to fix.
fn is_freeform_tool(t: &Value) -> bool {
    t.get("type").and_then(Value::as_str) == Some("custom")
        && !matches!(
            t.get("parameters"),
            Some(Value::Object(m)) if m.get("type").and_then(Value::as_str) == Some("object")
        )
}

/// Build the `parameters` schema a tool emits as a Chat function.
///
/// Ordinary tools carry a JSON schema in `parameters` and pass through
/// unchanged. Freeform tools get a wrapper object with a single string
/// property: Chat Completions requires every function schema to be a JSON
/// object (strict upstreams 400 a bare `{}`), and the property guides the
/// model to put the raw input where the response path can unwrap it.
///
/// Everything else - a tool with no schema that is not freeform - gets the
/// empty object schema. That satisfies the same requirement without inventing
/// an argument the tool does not take.
fn tool_parameters(t: &Value, freeform: bool) -> Value {
    match t.get("parameters") {
        Some(Value::Object(m)) if m.get("type").and_then(Value::as_str) == Some("object") => {
            t.get("parameters").cloned().unwrap()
        }
        _ if freeform => json!({
            "type": "object",
            "properties": {
                FREEFORM_INPUT_FIELD: {
                    "type": "string",
                    "description": "The raw text input for this freeform tool (do not wrap it in further JSON).",
                }
            },
            "required": [FREEFORM_INPUT_FIELD],
        }),
        _ => json!({ "type": "object", "properties": {} }),
    }
}

/// Reverse [`tool_parameters`] on a model tool call: freeform tools travel as
/// `{"input": "<raw text>"}` through Chat, and Codex's freeform handler needs
/// exactly the raw text. Anything that is not that shape passes through
/// untouched (a model may legitimately emit the raw input directly, and a
/// provider may tolerate non-JSON arguments).
fn unwrap_freeform_arguments(arguments: &str) -> String {
    match parse_wrapped_input(arguments) {
        Some(input) => input,
        None => arguments.to_string(),
    }
}

/// Parse a freeform tool's Chat arguments as `{"input": "<text>"}` and return
/// the text. The streamed arguments can carry real control characters inside
/// the string (a lenient provider decodes the JSON escapes), which a strict
/// re-parse would reject - so control characters are re-escaped to their JSON
/// form first. Already-valid JSON is unchanged by that pass.
fn parse_wrapped_input(arguments: &str) -> Option<String> {
    let mut sanitized = String::with_capacity(arguments.len());
    for c in arguments.chars() {
        match c {
            '\n' => sanitized.push_str("\\n"),
            '\r' => sanitized.push_str("\\r"),
            '\t' => sanitized.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                sanitized.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => sanitized.push(c),
        }
    }
    match serde_json::from_str::<Value>(&sanitized) {
        Ok(Value::Object(m)) => match m.get(FREEFORM_INPUT_FIELD) {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// Specs Codex already discovered through `tool_search`, recovered from
/// `tool_search_output` items in the input.
///
/// The search itself runs client-side; the output item lists the matched
/// specs (with `defer_loading: true`, a Responses-only flag the flatten
/// ignores) so the backend can activate them on the next call. A routed
/// model's "backend" is this proxy, so activation happens here: the specs
/// join the request's tool list.
fn deferred_tool_specs(payload: &Value) -> Vec<Value> {
    payload
        .get("input")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|i| i.get("type").and_then(Value::as_str) == Some("tool_search_output"))
                .flat_map(|i| {
                    i.get("tools")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Every tool spec a request carries: the `tools` array plus whatever earlier
/// `tool_search` rounds already discovered (see [`deferred_tool_specs`]).
fn all_tool_specs(payload: &Value) -> Vec<Value> {
    let mut all: Vec<Value> = payload
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    all.extend(deferred_tool_specs(payload));
    all
}

/// `flattened tool name -> namespace` for a Responses request, so the reply
/// can restore the namespace Chat Completions cannot carry. Derived from the
/// same specs `responses_to_chat` flattens, so both sides agree without
/// having to thread state through the call.
pub fn tool_namespace_map(payload: &Value) -> BTreeMap<String, String> {
    flatten_tools(&all_tool_specs(payload)).1
}

/// `flattened tool name` for every freeform custom tool in a request, so the
/// reply can unwrap the JSON wrapper those tools travel in back into the raw
/// input Codex's freeform handler expects. Derived from the same specs as
/// [`tool_namespace_map`], so both directions agree on the flattened names.
pub fn freeform_tool_names(payload: &Value) -> BTreeSet<String> {
    flatten_tools(&all_tool_specs(payload)).3
}

/// Convert Responses freeform tools into ordinary Responses functions for an
/// upstream that accepts function tools but rejects `type: "custom"`. The
/// caller decides when this compatibility path is needed; native Responses
/// providers keep their grammar-bearing custom tools untouched.
pub fn responses_with_function_tools(payload: &Value) -> Value {
    let mut out = payload.clone();
    let Some(tools) = out.get_mut("tools").and_then(Value::as_array_mut) else {
        return out;
    };

    for tool in tools {
        if !is_freeform_tool(tool) {
            continue;
        }
        let Some(name) = tool.get("name").and_then(Value::as_str).map(str::to_string) else {
            continue;
        };
        let description = tool
            .get("description")
            .and_then(Value::as_str)
            .map(|description| {
                format!(
                    "{description}\n\nThe {FREEFORM_INPUT_FIELD} field carries the tool's entire raw input."
                )
            });
        *tool = json!({
            "type": "function",
            "name": name,
            "description": description,
            "parameters": tool_parameters(tool, true),
        });
    }
    out
}

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
                convert_response_input_item(
                    item,
                    &mut messages,
                    &mut pending_reasoning,
                    &replay_names,
                )?;
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
    //   (low/medium/high/xhigh) - OpenRouter normalizes them per model.
    // - mapped: reasoning_effort collapsed to low/high/max - Kimi and
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
    replay_names: &BTreeMap<(String, String), String>,
) -> Result<()> {
    let item_type = item.get("type").and_then(Value::as_str);
    // Attach collected reasoning to the next assistant message, then clear.
    let take_reasoning = |msg: &mut Value, role: &str, pending: &mut String| {
        if role == "assistant" && !pending.is_empty() {
            msg["reasoning_content"] = Value::String(std::mem::take(pending));
        }
    };
    // Plain message items carry role+content; typed items are tool IO.
    // `agent_message` is how Codex delivers an inter-agent task to a spawned
    // child (ResponseItem::AgentMessage: author/recipient, header in an
    // input_text part, body in encrypted_content). It carries no role field,
    // so it falls through to "user" below - which is what a task is for the
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
                        // header without its body - the child received
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
                take_reasoning(&mut msg, "assistant", pending_reasoning);
                messages.push(msg);
            }
        }
        Some("function_call_output") | Some("custom_tool_call_output") => {
            messages.push(json!({
                "role": "tool",
                "tool_call_id": item.get("call_id").cloned().unwrap_or(Value::Null),
                "content": item.get("output").cloned().unwrap_or(json!("")),
            }));
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
                    // Anthropic requires an object-rooted schema. Anything
                    // else - absent, null, a bare `{}`, a grammar - becomes
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
///   - `/response/usage` - Responses `response.completed` events;
///   - `/usage`          - the common case for every dialect;
///   - `/choices/*/usage` - Chat Completions streams from providers such as
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
/// Returns `None` when the payload carries no usage yet - the normal case
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
        .unwrap_or_else(|| synthetic_id("resp"));
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
                    "id": synthetic_id("rs"),
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

fn message_item(text: &str) -> Value {
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
fn apply_namespace(mut item: Value, namespaces: &BTreeMap<String, String>, name: &str) -> Value {
    if let Some(ns) = namespaces.get(name) {
        item["namespace"] = json!(ns);
    }
    item
}

fn function_call_item(call_id: &str, name: &str, arguments: &str) -> Value {
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
fn tool_search_call_item(call_id: &str, arguments: &str) -> Value {
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
fn tool_call_output_item(call_id: &str, name: &str, arguments: &str) -> Value {
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
/// `ToolPayload::Custom` only from this item type - a `function_call` for a
/// custom tool becomes `ToolPayload::Function`, which no freeform handler
/// matches, and the call is aborted as unknown.
///
/// `item_id` is the id the opening `output_item.added` frame already
/// announced for this output index: both frames describe one item, so minting
/// a fresh id here would hand the client two.
fn custom_tool_call_item(item_id: &str, call_id: &str, name: &str, input: &str) -> Value {
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
fn restore_freeform_response_item(item: &mut Value, completed: bool) {
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
            rs_open: false,
            rs_closed: false,
            rs_text_acc: String::new(),
            tools: BTreeMap::new(),
            next_tool_output_index: 1,
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
    /// plain functions. Everything else - the whole multi-agent surface,
    /// apply_patch, and every MCP server - used to be filtered out, so a
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
        // task at all - and reported exactly that.
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
        // The user's message survives with its content - only the id it was
        // never going to be able to resolve is dropped.
        assert_eq!(input[0]["content"], "ola");
        assert_eq!(input[0]["role"], "user");
    }

    #[test]
    fn a_stripped_tool_call_keeps_its_pairing() {
        // The shape a real switch-model turn produced: the contaminated items
        // were a function_call and a message, not reasoning. The call is
        // paired with its output by `call_id`, which is not an item id and
        // must survive - otherwise the backend sees an orphaned result.
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
        // whose schema is a grammar, not a JSON object - there is no
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
        // argument it does not take, and - the reason this matters - it must
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
        // message - otherwise the model edits blind.
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
        // there - the call fails as an unknown tool.
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
    fn streamed_freeform_tool_call_comes_back_as_custom_tool_call() {
        // apply_patch travels as a Chat function whose arguments are the JSON
        // wrapper `{"input": "<patch>"}`. The closing frames must hand Codex a
        // `custom_tool_call` item carrying the raw patch - its router builds
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

    #[test]
    fn non_streamed_tool_call_restores_the_namespace_too() {
        // chat_completion_to_responses has no request at hand, so proxy.rs
        // applies the namespace map afterwards - same gap as the streaming
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
    // normalize_usage - the single source of truth for usage dialects.
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
}
