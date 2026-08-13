use axum::http::StatusCode;
use axum::{body::Body, extract::Request, middleware::Next, response::Response};
use std::path::Path;
use std::sync::OnceLock;

// The proxy accepts traffic from any local process, so every endpoint needs a
// user-local secret before it can use a configured provider credential.
static LOCAL_TOKEN: OnceLock<String> = OnceLock::new();

/// Shared secret required on every request to the local proxy.
///
/// The token is generated once and persisted under `~/.loomrouter`, so a
/// running Codex keeps the same credentials after LoomRouter restarts.
/// Regenerating it per process only worked while Codex reloaded
/// `config.toml`; an already-open app kept sending the old provider token.
#[cfg(not(test))]
pub fn local_token() -> &'static str {
    LOCAL_TOKEN.get_or_init(|| load_or_create_local_token(&local_token_path()))
}

#[cfg(test)]
pub fn local_token() -> &'static str {
    LOCAL_TOKEN.get_or_init(generate_local_token)
}

#[cfg(not(test))]
fn local_token_path() -> std::path::PathBuf {
    crate::config::config_dir().join("local-token")
}

fn managed_token_from(raw: &str) -> Option<String> {
    let managed_start = raw.find(crate::codex::BEGIN_MARK)? + crate::codex::BEGIN_MARK.len();
    let managed_end = raw[managed_start..].find(crate::codex::END_MARK)? + managed_start;
    let managed = &raw[managed_start..managed_end];
    let needle = "x-loomrouter-token\" = \"";
    let start = managed.find(needle)? + needle.len();
    let end = managed[start..].find('"')?;
    Some(managed[start..start + end].to_string())
}

fn configured_token() -> Option<String> {
    let raw = std::fs::read_to_string(crate::codex::codex_home().join("config.toml")).ok()?;
    managed_token_from(&raw)
}

fn generate_local_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn valid_local_token(token: &str) -> bool {
    token.len() == 64 && token.chars().all(|c| c.is_ascii_hexdigit())
}

fn load_or_create_local_token(path: &Path) -> String {
    if let Ok(token) = std::fs::read_to_string(path) {
        if valid_local_token(&token) {
            return token;
        }
    }
    if let Some(token) = configured_token().filter(|token| valid_local_token(token)) {
        if let Err(e) = crate::secure_fs::write_private(path, token.as_bytes()) {
            tracing::warn!(path = %path.display(), error = %e, "failed to persist migrated local token");
        }
        return token;
    }
    let token = generate_local_token();
    if let Err(e) = crate::secure_fs::write_private(path, token.as_bytes()) {
        tracing::warn!(path = %path.display(), error = %e, "failed to persist local token");
    }
    token
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_local_token_is_reused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("local-token");

        let first = load_or_create_local_token(&path);
        let second = load_or_create_local_token(&path);

        assert_eq!(first, second);
        assert_eq!(first.len(), 64);
    }

    #[test]
    fn managed_block_token_is_read_without_leaking_adjacent_text() {
        let raw = format!(
            "# x-loomrouter-token\" = \"{}\"\n{}\nhttp_headers = {{ \"x-loomrouter-token\" = \"abc\", \"Authorization\" = \"Bearer abc\" }}\n{}",
            "0".repeat(64),
            crate::codex::BEGIN_MARK,
            crate::codex::END_MARK,
        );
        assert_eq!(managed_token_from(&raw).as_deref(), Some("abc"));
    }

    #[test]
    fn token_outside_managed_block_is_ignored() {
        let raw = format!(
            "x-loomrouter-token\" = \"{}\"\n{}\n# no token here\n{}",
            "a".repeat(64),
            crate::codex::BEGIN_MARK,
            crate::codex::END_MARK,
        );
        assert_eq!(managed_token_from(&raw), None);
    }
}
