use axum::http::StatusCode;
use axum::{body::Body, extract::Request, middleware::Next, response::Response};
use std::sync::OnceLock;

// The proxy accepts traffic from any local process, so every endpoint needs a
// process-local secret before it can use a configured provider credential.
static LOCAL_TOKEN: OnceLock<String> = OnceLock::new();

/// Shared secret required on every request to the local proxy.
/// Generated once per process from 32 random bytes (two UUIDv4s = 64 hex
/// chars; `uuid` is the only RNG crate already in Cargo.toml).
pub fn local_token() -> &'static str {
    LOCAL_TOKEN.get_or_init(|| {
        format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        )
    })
}

/// Constant-time token comparison avoids leaking a valid token prefix through
/// timing on the local port.
fn token_eq(given: &str) -> bool {
    let expected = local_token().as_bytes();
    let given = given.as_bytes();
    if given.len() != expected.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in given.iter().zip(expected.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

fn is_authorized(req: &Request) -> bool {
    if let Some(v) = req
        .headers()
        .get("x-loomrouter-token")
        .and_then(|v| v.to_str().ok())
    {
        if token_eq(v.trim()) {
            return true;
        }
    }
    if let Some(v) = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
    {
        if let Some(t) = v.strip_prefix("Bearer ") {
            if token_eq(t.trim()) {
                return true;
            }
        }
    }
    // WS clients that cannot set headers authenticate via the query string.
    if let Some(query) = req.uri().query() {
        for pair in query.split('&') {
            if let Some(v) = pair.strip_prefix("token=") {
                if token_eq(v) {
                    return true;
                }
            }
        }
    }
    false
}

pub(super) async fn auth_gate(req: Request, next: Next) -> Response {
    if is_authorized(&req) {
        return next.run(req).await;
    }
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header("content-type", "application/json")
        .body(Body::from(
            "{\"error\":{\"message\":\"loom-router: missing or invalid local token\"}}",
        ))
        .expect("a static unauthorized response is valid")
}
