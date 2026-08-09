//! Protocol translation between wire formats, including streaming.

mod request;
mod response;
mod stream;
mod tools;

pub use request::{chat_to_anthropic, responses_to_chat};
pub use response::{
    anthropic_to_chat, anthropic_to_responses, apply_namespaces_to_output,
    chat_completion_to_responses, extract_text, normalize_usage, unwrap_freeform_to_output,
};
pub use stream::{DownstreamKind, OutFrame, StreamTranslator, UpstreamKind};
pub use tools::{
    freeform_tool_names, is_synthetic_item_id, responses_with_function_tools, strip_synthetic_ids,
    tool_namespace_map,
};

#[cfg(test)]
pub(crate) use tools::{flatten_tools, synthetic_id};

#[cfg(test)]
mod tests_a;
#[cfg(test)]
mod tests_b;
