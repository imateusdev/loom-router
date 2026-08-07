//! Local `claude` CLI integration for the claude-code provider.
//!
//! The claude-code provider has no API key: its credential is the login the
//! user already performed inside Claude Code CLI/Desktop (`claude auth
//! status`). LoomRouter never sees or stores that credential — it only
//! probes whether the CLI is present and logged in, and (in the future)
//! spawns the real binary to serve model requests on the subscription.
//!
//! Nothing here calls the Anthropic API directly and no token is read from
//! disk, so this stays on the safe side of Anthropic's credential policy.

use serde::Serialize;

/// Parsed `claude auth status` output.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ClaudeAuthStatus {
    pub logged_in: bool,
    pub auth_method: Option<String>,
    pub subscription_type: Option<String>,
    pub email: Option<String>,
    /// Human label for the plan (`subscription_type`), used by the UI.
    pub plan: Option<String>,
    pub error: Option<String>,
}

/// Resolve the `claude` binary: `CLAUDE_BIN` (dev escape hatch, mirrors
/// `CODEX_BIN`) or `claude` on PATH. Returns None when neither exists.
pub fn claude_binary() -> Option<std::path::PathBuf> {
    if let Ok(explicit) = std::env::var("CLAUDE_BIN") {
        if !explicit.trim().is_empty() {
            let p = std::path::PathBuf::from(explicit);
            if p.exists() {
                return Some(p);
            }
        }
    }
    if let Some(p) = find_in_path("claude") {
        return Some(p);
    }
    None
}

fn find_in_path(bin: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Whether the local CLI exists at all (used to gate the provider's
/// credential health without shelling out).
pub fn claude_cli_available() -> bool {
    claude_binary().is_some()
}

/// Run `claude auth status` and parse the subscription/login state.
///
/// Runs on a blocking thread: it spawns a subprocess. Any failure surfaces
/// as `logged_in: false` with an `error` string rather than an Err, so the
/// UI can distinguish "not logged in" from "binary missing".
pub async fn auth_status() -> ClaudeAuthStatus {
    let Some(bin) = claude_binary() else {
        return ClaudeAuthStatus {
            logged_in: false,
            error: Some("claude CLI not found on PATH".to_string()),
            ..Default::default()
        };
    };
    let bin = bin.clone();
    tokio::task::spawn_blocking(move || {
        let out = std::process::Command::new(bin)
            .args(["auth", "status"])
            .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
            .env("CLAUDE_CODE_SKIP_BACKGROUND_PREFETCH", "1")
            .output();
        match out {
            Ok(o) if o.status.success() => {
                match serde_json::from_slice::<serde_json::Value>(&o.stdout) {
                    Ok(v) => {
                        let logged_in = v
                            .get("loggedIn")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false);
                        let subscription_type = v
                            .get("subscriptionType")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string);
                        let plan = subscription_type.as_deref().map(plan_label);
                        ClaudeAuthStatus {
                            logged_in,
                            auth_method: v
                                .get("authMethod")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_string),
                            subscription_type: subscription_type.clone(),
                            email: v
                                .get("email")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_string),
                            plan,
                            error: if logged_in {
                                None
                            } else {
                                Some("claude CLI is not logged in".to_string())
                            },
                        }
                    }
                    Err(e) => ClaudeAuthStatus {
                        logged_in: false,
                        error: Some(format!("could not parse `claude auth status`: {e}")),
                        ..Default::default()
                    },
                }
            }
            Ok(o) => ClaudeAuthStatus {
                logged_in: false,
                error: Some(format!(
                    "`claude auth status` exited {}: {}",
                    o.status,
                    String::from_utf8_lossy(&o.stderr).trim()
                )),
                ..Default::default()
            },
            Err(e) => ClaudeAuthStatus {
                logged_in: false,
                error: Some(format!("could not run `claude auth status`: {e}")),
                ..Default::default()
            },
        }
    })
    .await
    .unwrap_or_else(|_| ClaudeAuthStatus {
        logged_in: false,
        error: Some("claude auth probe panicked".to_string()),
        ..Default::default()
    })
}

/// Map Claude Code's `subscriptionType` to a UI-friendly plan name.
fn plan_label(subscription: &str) -> String {
    match subscription {
        "free" => "Free".to_string(),
        "pro" => "Pro".to_string(),
        "max" => "Max".to_string(),
        "team" => "Team".to_string(),
        "enterprise" => "Enterprise".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_label_covers_the_known_plans() {
        assert_eq!(plan_label("free"), "Free");
        assert_eq!(plan_label("pro"), "Pro");
        assert_eq!(plan_label("max"), "Max");
        assert_eq!(plan_label("team"), "Team");
        assert_eq!(plan_label("enterprise"), "Enterprise");
        assert_eq!(plan_label("mystery"), "mystery");
    }

    #[test]
    fn claude_code_catalog_is_curated() {
        assert_eq!(
            crate::providers::CLAUDE_CODE_MODELS.len(),
            5,
            "curated catalog must list every model a Max plan exposes"
        );
        assert!(crate::providers::claude_code_context("claude-opus-5") == Some(1_000_000));
        assert!(crate::providers::claude_code_fast_mode("claude-opus-5"));
        assert!(crate::providers::claude_code_fast_mode("claude-opus-4-8"));
        assert!(!crate::providers::claude_code_fast_mode(
            "claude-sonnet-4-6"
        ));
        assert!(!crate::providers::claude_code_fast_mode("claude-haiku-4-5"));
        assert!(crate::providers::claude_code_context("claude-haiku-4-5") == Some(200_000));
        assert!(!crate::providers::claude_code_fast_mode("claude-fable-5"));
    }
}
