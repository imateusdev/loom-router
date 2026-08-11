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
/// a backend that resolves ids — OpenAI's, when the user switches the thread
/// to a native model — the backend answers 404 for an id it never issued.
/// The marker is what lets the native passthrough tell those apart from ids
/// the backend really did hand out. `-` cannot occur in the hex that follows
/// a real id's prefix, so the pair is unambiguous.
const SYNTHETIC_ID_MARKER: &str = "lr-";

/// Mint an item id under `prefix` (`rs`, `msg`, `fc`, …), marked as ours.
pub(crate) fn synthetic_id(prefix: &str) -> String {
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
/// field to carry a namespace — the Responses protocol keeps `namespace` and
/// `name` as separate fields on a function call, which is why the round trip
/// needs this map instead of a naming convention.
///
/// `tool_search` is deferred tool loading: Codex advertises only the search
/// tool, runs the search locally (BM25 over its registry) when the model
/// calls it with `execution: "client"`, and lists the discovered specs in a
/// `tool_search_output` item in the NEXT request's input. On the native path
/// the Responses backend activates those tools; a routed model has no such
/// backend, so the proxy plays that role — see [`all_tool_specs`]. The spec
/// itself flattens into an ordinary Chat function here.
pub(crate) const TOOL_SEARCH_NAME: &str = "tool_search";
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
/// specs — the same tool found by two `tool_search` calls — collapse into
/// one entry and, unlike distinct namespaces sharing a bare name, do not
/// trigger the collision prefix.
#[allow(clippy::type_complexity)]
pub(crate) fn flatten_tools(
    tools: &[Value],
) -> (
    Vec<Value>,
    BTreeMap<String, String>,
    BTreeMap<(String, String), String>,
    BTreeSet<String>,
) {
    // Chat Completions requires every function's `parameters` to be a JSON
    // Schema rooted at `type: "object"`; strict upstreams (e.g. OpenCode Go)
    // 400 anything else. Codex's `custom` tools — apply_patch ships as one —
    // are freeform and carry no `parameters` at all (their schema is a
    // grammar), so a verbatim clone would emit `{}`/`null` and get rejected.
    // Freeform tools are given a string wrapper ([`tool_parameters`]); the
    // description also gets a hint so the "do not wrap in JSON" instruction
    // from the tool's own docs does not fight the wrapper. A `function` tool
    // that ships no usable schema is not freeform — it gets the empty object
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
pub(crate) const FREEFORM_INPUT_FIELD: &str = "input";

/// Whether a tool spec is freeform: a `custom` tool whose input is raw text
/// (apply_patch ships as one). Its schema is a grammar, so `parameters` is
/// absent or not rooted at `type: "object"` — unusable as a Chat function
/// schema as-is.
///
/// Both halves are load-bearing. Keying on the missing schema alone would
/// sweep in a plain `function` tool that ships no parameters: it would be
/// handed an `input` argument it does not take, and its call would come home
/// as a `custom_tool_call`, which Codex routes to a freeform handler and
/// aborts as unknown — the exact failure this path exists to fix.
pub(crate) fn is_freeform_tool(t: &Value) -> bool {
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
/// Everything else — a tool with no schema that is not freeform — gets the
/// empty object schema. That satisfies the same requirement without inventing
/// an argument the tool does not take.
pub(crate) fn tool_parameters(t: &Value, freeform: bool) -> Value {
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
pub(crate) fn unwrap_freeform_arguments(arguments: &str) -> String {
    match parse_wrapped_input(arguments) {
        Some(input) => input,
        None => arguments.to_string(),
    }
}

/// Parse a freeform tool's Chat arguments as `{"input": "<text>"}` and return
/// the text. The streamed arguments can carry real control characters inside
/// the string (a lenient provider decodes the JSON escapes), which a strict
/// re-parse would reject — so control characters are re-escaped to their JSON
/// form first. Already-valid JSON is unchanged by that pass.
pub(crate) fn parse_wrapped_input(arguments: &str) -> Option<String> {
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
pub(crate) fn deferred_tool_specs(payload: &Value) -> Vec<Value> {
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
pub(crate) fn all_tool_specs(payload: &Value) -> Vec<Value> {
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
