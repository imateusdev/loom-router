//! End-to-end proxy scenarios grouped by downstream and upstream protocol.

mod e2e {
    mod main_updates;
    mod prompt_cache;
    mod protocol_passthrough;
    mod responses_http;
    mod responses_ws;
    mod support;
}
