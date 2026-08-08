//! End-to-end proxy scenarios grouped by downstream and upstream protocol.

mod e2e {
    mod protocol_passthrough;
    mod responses_http;
    mod responses_ws;
    mod support;
}
