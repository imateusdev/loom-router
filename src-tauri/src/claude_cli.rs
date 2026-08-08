//! Local `claude` CLI integration for the claude-code provider.
//!
//! The claude-code provider has no API key: its credential is the login the
//! user already performed inside Claude Code CLI/Desktop (`claude auth
//! status`). LoomRouter never sees or stores that credential - it only
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

/// Resolve the `claude` binary, cached for the process.
///
/// A macOS app launched from Finder does not inherit the shell's PATH - it
/// gets launchd's (`/usr/bin:/bin:/usr/sbin:/sbin`), which contains no
/// package manager's bin directory. Claude Code installs into `~/.local/bin`,
/// `~/.claude/local`, `/opt/homebrew/bin`, the npm/bun/volta globals and
/// friends, so probing PATH alone finds the CLI when the app is started from
/// a terminal and never when it is double-clicked. Same story on Windows,
/// where GUI launches do not inherit the shell profile PATH either.
///
/// Resolution order mirrors `codex::codex_bin` (and honours `CLAUDE_BIN`, the
/// same escape hatch):
///   1. `CLAUDE_BIN` explicitly;
///   2. whatever PATH this process has (correct when launched from a shell);
///   3. the user's login shell (`$SHELL -lic 'command -v claude'`), which is
///      the only way to honour a PATH they set in their own rc files;
///   4. well-known per-platform install locations.
pub fn claude_binary() -> Option<std::path::PathBuf> {
    static RESOLVED: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    RESOLVED.get_or_init(resolve_claude_bin).clone()
}

fn resolve_claude_bin() -> Option<std::path::PathBuf> {
    // 1. Explicit override, the documented escape hatch.
    if let Ok(explicit) = std::env::var("CLAUDE_BIN") {
        let p = std::path::PathBuf::from(explicit.trim());
        if !p.as_os_str().is_empty() && p.exists() {
            return Some(p);
        }
    }

    // 2. Whatever PATH this process has. On Windows the npm shim is
    // `claude.cmd`, the native installer a real `claude.exe`.
    let names: &[&str] = if cfg!(windows) {
        &["claude.cmd", "claude.exe", "claude"]
    } else {
        &["claude"]
    };
    for name in names {
        if let Some(p) = find_in_path(name) {
            if runs(&p) {
                return Some(p);
            }
        }
    }

    // 3. Ask the user's login shell where it is (non-Windows shells only).
    #[cfg(unix)]
    if let Some(found) = login_shell_lookup() {
        return Some(found);
    }

    // 4. Well-known install locations.
    well_known_install()
}

/// Walk `PATH` for a binary name, returning the first match that is a file.
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

/// Resolve `claude` through the user's shell, so the PATH they configured in
/// rc files is honoured even when this process inherited launchd's bare one.
///
/// `-lic`, not `-lc`: zsh is the macOS default and only sources `.zshrc` for
/// *interactive* shells, while `.zprofile`/`.zlogin` handle login ones. Most
/// people put their PATH in `.zshrc`, so a login-only probe returns nothing
/// on the exact setup this is meant to rescue.
///
/// An interactive shell can print a banner or a prompt, so the last non-empty
/// line is taken and then verified by actually running it. It can also hang
/// on a bad rc file, and this runs inside a status call the UI waits on -
/// hence the deadline.
#[cfg(unix)]
fn login_shell_lookup() -> Option<std::path::PathBuf> {
    use std::io::Read;

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let mut child = std::process::Command::new(&shell)
        .args(["-lic", "command -v claude"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                tracing::warn!("the login shell did not answer in time; skipping it");
                return None;
            }
            Err(_) => return None,
        }
    }

    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    let path = out
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())?
        .to_string();
    let bin = std::path::PathBuf::from(&path);
    if runs(&bin) {
        return Some(bin);
    }
    None
}

/// Whether this command answers `--version` successfully. Guards against
/// stale npm shims that point at a moved or unlinked install.
fn runs(bin: &std::path::Path) -> bool {
    let mut command = std::process::Command::new(bin);
    hide_console_window(&mut command);
    command
        .arg("--version")
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .env("CLAUDE_CODE_SKIP_BACKGROUND_PREFETCH", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Per-platform install directories Claude Code is known to use.
#[cfg(unix)]
fn well_known_install() -> Option<std::path::PathBuf> {
    let home = dirs::home_dir()?;
    let candidates = [
        home.join(".local/bin/claude"),
        home.join(".claude/local/claude"),
        home.join(".bun/bin/claude"),
        home.join(".volta/bin/claude"),
        home.join(".npm-global/bin/claude"),
        home.join(".yarn/bin/claude"),
        std::path::PathBuf::from("/opt/homebrew/bin/claude"),
        std::path::PathBuf::from("/usr/local/bin/claude"),
        std::path::PathBuf::from("/opt/local/bin/claude"),
    ];
    for path in candidates {
        if path.is_file() && runs(&path) {
            return Some(path);
        }
    }
    newest_versioned_install(home)
}

/// The native installer keeps every release under
/// `~/.local/share/claude/versions/<version>/claude` and symlinks the newest
/// into `~/.local/bin` - pick the most recently modified one when the link
/// is missing (e.g. the app was double-clicked and `~/.local/bin` is absent
/// from PATH anyway).
#[cfg(unix)]
fn newest_versioned_install(home: std::path::PathBuf) -> Option<std::path::PathBuf> {
    let bin_root = home.join(".local/share/claude/versions");
    let mut newest: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    for entry in std::fs::read_dir(bin_root).ok()?.flatten() {
        let bin = entry.path().join("claude");
        if !bin.is_file() {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if newest.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
            newest = Some((mtime, bin));
        }
    }
    newest.map(|(_, p)| p)
}

/// Claude Code's Windows install locations: the native installer under
/// `%USERPROFILE%\.local\bin` and `%APPDATA%\npm` (npm global shim), plus
/// the versioned native install under `%USERPROFILE%\.local\share`.
#[cfg(windows)]
fn well_known_install() -> Option<std::path::PathBuf> {
    let home = dirs::home_dir()?;
    let data_local = dirs::data_local_dir().unwrap_or_else(|| home.join(".local"));
    let appdata = std::env::var_os("APPDATA").map(std::path::PathBuf::from);
    let names = ["claude.exe", "claude.cmd", "claude"];
    let roots = [
        home.join(".local/bin"),
        home.join(".claude/local"),
        data_local.join("share/claude/versions"),
        appdata.as_ref()?.join("npm"),
    ];
    for root in roots {
        for name in names {
            let candidate = root.join(name);
            if candidate.is_file() && runs(&candidate) {
                return Some(candidate);
            }
        }
    }
    let mut newest: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    for entry in std::fs::read_dir(home.join(".local/share/claude/versions"))
        .ok()?
        .flatten()
    {
        for name in names {
            let bin = entry.path().join(name);
            if !bin.is_file() || !runs(&bin) {
                continue;
            }
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            if newest.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
                newest = Some((mtime, bin));
            }
        }
    }
    newest.map(|(_, p)| p)
}

/// Whether the local CLI exists at all (used to gate the provider's
/// credential health without shelling out).
pub fn claude_cli_available() -> bool {
    claude_binary().is_some()
}

/// One completed `claude -p` turn: the final assistant text and the token
/// counts the CLI reported.
#[derive(Debug, Clone, Default)]
pub struct ClaudePrintResult {
    pub text: String,
    pub session_id: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Total billed cost in USD, as reported by the CLI.
    pub total_cost_usd: f64,
}

/// Run one non-interactive turn through the local `claude` CLI.
///
/// This is the bridge between LoomRouter and the user's Claude subscription:
/// the proxy translates the agent's request to a text prompt, spawns
/// `claude -p` with it, and turns the CLI's JSON answer back into Responses
/// events. The CLI uses the user's own login - LoomRouter never sees a token.
///
/// The prompt is fed via stdin (print mode reads the prompt from stdin when
/// it is not given as a positional argument), so arbitrarily long
/// conversations work. Runs on a blocking thread because it spawns a
/// subprocess. `config_dir`, when set, overrides `CLAUDE_CONFIG_DIR` so a
/// specific login can be addressed (the multi-account foundation).
pub async fn run_print_turn(
    prompt: &str,
    model: &str,
    config_dir: Option<&std::path::Path>,
) -> anyhow::Result<ClaudePrintResult> {
    let Some(bin) = claude_binary() else {
        anyhow::bail!("claude CLI not found (set CLAUDE_BIN to its location)");
    };
    let prompt = prompt.to_string();
    let model = model.to_string();
    let config_dir = config_dir.map(std::path::Path::to_path_buf);
    tokio::task::spawn_blocking(move || {
        let mut cmd = std::process::Command::new(&bin);
        hide_console_window(&mut cmd);
        cmd.arg("-p")
            .arg("--model")
            .arg(&model)
            .arg("--output-format")
            .arg("json")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
            .env("CLAUDE_CODE_SKIP_BACKGROUND_PREFETCH", "1");
        if let Some(dir) = config_dir {
            cmd.env("CLAUDE_CONFIG_DIR", dir);
        }
        let mut child = cmd.spawn()?;
        use std::io::Write;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(prompt.as_bytes())?;
        }
        let out = child.wait_with_output()?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            anyhow::bail!(
                "`claude -p` exited {}: {}",
                out.status,
                stderr.trim().chars().take(500).collect::<String>()
            );
        }
        parse_print_result(&out.stdout)
    })
    .await?
}

/// Parse the `claude -p --output-format json` answer into a usable turn.
fn parse_print_result(stdout: &[u8]) -> anyhow::Result<ClaudePrintResult> {
    let v: serde_json::Value = serde_json::from_slice(stdout).map_err(|e| {
        anyhow::anyhow!(
            "could not parse `claude -p` output: {e}: {}",
            String::from_utf8_lossy(stdout)
                .chars()
                .take(200)
                .collect::<String>()
        )
    })?;
    if v.get("is_error").and_then(serde_json::Value::as_bool) == Some(true) {
        anyhow::bail!(
            "`claude -p` returned an error: {}",
            v.get("result")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
        );
    }
    let usage = v.get("usage").cloned().unwrap_or(serde_json::json!({}));
    Ok(ClaudePrintResult {
        text: v
            .get("result")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        session_id: v
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        input_tokens: usage
            .get("input_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        output_tokens: usage
            .get("output_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        total_cost_usd: v
            .get("total_cost_usd")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0),
    })
}

/// Render a chat-message transcript into a single text prompt for `claude -p`.
///
/// `claude -p` takes one flat prompt, so roles are marked inline. Tool results
/// are rendered as user turns; tool_calls are skipped (print mode resolves its
/// own tools from its own toolset, not the caller's). The prompt is fed via
/// stdin, so length is only bounded by the CLI itself.
pub fn render_prompt(messages: &[serde_json::Value]) -> String {
    let mut out = String::new();
    for m in messages {
        let role = m
            .get("role")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let content = m
            .get("content")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        match role {
            "system" => {
                out.push_str("System instructions:\n");
                out.push_str(content);
            }
            "assistant" => {
                if m.get("tool_calls").is_some() {
                    continue;
                }
                out.push_str("Assistant:\n");
                out.push_str(content);
            }
            "tool" => {
                out.push_str("Tool result (from a previous turn):\n");
                out.push_str(content);
            }
            _ => {
                out.push_str("User:\n");
                out.push_str(content);
            }
        }
        out.push('\n');
        out.push('\n');
    }
    out
}

/// Serialize one SSE event in Anthropic's wire format:
/// an `event:` line, then a `data:` line holding one JSON object.
pub fn anthropic_sse_event(event: &str, data: &serde_json::Value) -> String {
    format!(
        "event: {event}\ndata: {}\n\n",
        serde_json::to_string(data).unwrap_or_default()
    )
}

/// Build the canonical Anthropic response object for a finished `claude -p`
/// turn, in Anthropic's JSON wire shape.
pub fn anthropic_json_response(
    id: &str,
    model: &str,
    text: &str,
    input_tokens: u64,
    output_tokens: u64,
) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": [{"type": "text", "text": text}],
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
        },
    })
}

/// Build the SSE frames a streaming Anthropic upstream would emit for a
/// finished `claude -p` turn: message_start, the text content block, and
/// message_stop. This is what translate_byte_stream consumes.
pub fn anthropic_sse_stream(
    id: &str,
    model: &str,
    text: &str,
    input_tokens: u64,
    output_tokens: u64,
) -> Vec<String> {
    let events = vec![
        anthropic_sse_event(
            "message_start",
            &serde_json::json!({
                "type": "message_start",
                "message": {
                    "id": id,
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "content": [],
                    "usage": {"input_tokens": input_tokens, "output_tokens": 0},
                },
            }),
        ),
        anthropic_sse_event(
            "content_block_start",
            &serde_json::json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""},
            }),
        ),
        anthropic_sse_event(
            "content_block_delta",
            &serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": text},
            }),
        ),
        anthropic_sse_event(
            "content_block_stop",
            &serde_json::json!({"type": "content_block_stop", "index": 0}),
        ),
        anthropic_sse_event(
            "message_delta",
            &serde_json::json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": output_tokens},
            }),
        ),
        anthropic_sse_event("message_stop", &serde_json::json!({"type": "message_stop"})),
    ];
    events
}

/// Run `claude auth status` and parse the subscription/login state.
///
/// Any failure surfaces as `logged_in: false` with an `error` string rather
/// than an Err, so the UI can distinguish "not logged in" from "binary
/// missing". The probe has a hard deadline: a hung CLI must not make every
/// setup poll pile up another blocked subprocess.
pub async fn auth_status() -> ClaudeAuthStatus {
    let Some(bin) = claude_binary() else {
        return ClaudeAuthStatus {
            logged_in: false,
            error: Some(
                "claude CLI not found (searched PATH, your login shell and the usual install locations; set CLAUDE_BIN to its location)".to_string(),
            ),
            ..Default::default()
        };
    };
    let mut command = tokio::process::Command::new(bin);
    command
        .args(["auth", "status"])
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .env("CLAUDE_CODE_SKIP_BACKGROUND_PREFETCH", "1")
        .kill_on_drop(true);
    // why: CLI shims are console applications on Windows, while this app is not.
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);

    let out = tokio::time::timeout(std::time::Duration::from_secs(10), command.output()).await;

    match out {
        Ok(Ok(o)) if o.status.success() => {
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
        Ok(Ok(o)) => ClaudeAuthStatus {
            logged_in: false,
            error: Some(format!(
                "`claude auth status` exited {}: {}",
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            )),
            ..Default::default()
        },
        Ok(Err(e)) => ClaudeAuthStatus {
            logged_in: false,
            error: Some(format!("could not run `claude auth status`: {e}")),
            ..Default::default()
        },
        Err(_) => ClaudeAuthStatus {
            logged_in: false,
            error: Some("`claude auth status` timed out".to_string()),
            ..Default::default()
        },
    }
}

#[cfg(windows)]
fn hide_console_window(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;

    // CLI shims are console applications on Windows; this app is not.
    command.creation_flags(0x0800_0000);
}

#[cfg(not(windows))]
fn hide_console_window(_: &mut std::process::Command) {}

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

    #[test]
    fn claude_code_labels_capitalize_each_token() {
        // Dash-delimited slugs read poorly in the tray and the providers
        // panel; the label is the id with tokens title-cased and space-joined.
        assert_eq!(
            crate::providers::claude_code_label("claude-opus-4-8"),
            Some("Claude Opus 4 8".to_string())
        );
        assert_eq!(
            crate::providers::claude_code_label("claude-fable-5"),
            Some("Claude Fable 5".to_string())
        );
        // Ids outside the curated catalog carry no pretty label, so callers
        // stamping unconditionally fall back to the raw id.
        assert_eq!(crate::providers::claude_code_label("not-a-model"), None);
    }

    /// Regression: a macOS app launched from Finder inherits launchd's bare
    /// PATH (`/usr/bin:/bin:/usr/sbin:/sbin`), which contains no package
    /// manager's bin directory - so probing PATH alone found `claude` when
    /// the app was started from a terminal and never when double-clicked.
    /// Resolution must fall through to the login shell and the known install
    /// locations to cover the exact state a Finder-launched app runs in.
    ///
    /// Mutates PATH for the process, so it is deliberately the only test in
    /// this module that does.
    #[cfg(unix)]
    #[test]
    fn resolves_the_cli_under_the_launchd_path() {
        let had_cli = resolve_claude_bin().is_some();
        if !had_cli {
            eprintln!("no Claude CLI installed here; nothing to resolve");
            return;
        }

        let saved = std::env::var("PATH").ok();
        // SAFETY: single-threaded test, restored below.
        unsafe {
            std::env::set_var("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
            std::env::remove_var("CLAUDE_BIN");
        }
        let found = resolve_claude_bin();
        unsafe {
            match saved {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
        }

        assert!(
            found.is_some(),
            "the CLI must still be found without a useful PATH - this is the \
             exact state a Finder-launched app runs in"
        );
    }

    #[test]
    fn render_prompt_flattens_roles_and_skips_tool_calls() {
        let messages = serde_json::json!([
            {"role": "system", "content": "You are a senior engineer."},
            {"role": "user", "content": "Fix the bug."},
            {"role": "assistant", "content": "Let me look.", "tool_calls": [{"id": "t1"}]},
            {"role": "tool", "content": "{\"ok\": true}"},
            {"role": "assistant", "content": "Done."},
        ]);
        let out = render_prompt(messages.as_array().unwrap());
        assert!(out.contains("System instructions:\nYou are a senior engineer."));
        assert!(out.contains("User:\nFix the bug."));
        // The assistant turn that only carried a tool call is dropped, but
        // its follow-up text and the tool result survive.
        assert!(!out.contains("Let me look."));
        assert!(out.contains("Tool result (from a previous turn):\n{\"ok\": true}"));
        assert!(out.contains("Assistant:\nDone."));
    }

    #[test]
    fn parse_print_result_reads_text_and_usage() {
        let stdout = br#"{
            "result": "The fix is in.",
            "session_id": "sess_1",
            "total_cost_usd": 0.012,
            "usage": {"input_tokens": 120, "output_tokens": 40}
        }"#;
        let r = parse_print_result(stdout).unwrap();
        assert_eq!(r.text, "The fix is in.");
        assert_eq!(r.session_id, "sess_1");
        assert_eq!(r.input_tokens, 120);
        assert_eq!(r.output_tokens, 40);
        assert!((r.total_cost_usd - 0.012).abs() < 1e-9);
    }

    #[test]
    fn parse_print_result_flags_is_error() {
        let stdout = br#"{"is_error": true, "result": "oops", "type": "error_reasoning"}"#;
        assert!(parse_print_result(stdout).is_err());
    }

    #[test]
    fn anthropic_sse_stream_emits_parseable_frames() {
        let frames = anthropic_sse_stream("msg_1", "claude-opus-5", "hi", 10, 5);
        // Every frame is a complete, blank-line-terminated SSE event that the
        // incremental parser closes immediately.
        let mut parser = crate::sse::SseParser::new();
        let joined: Vec<u8> = frames.iter().flat_map(|f| f.as_bytes().to_vec()).collect();
        let events = parser.push(&joined);
        parser.flush();
        assert_eq!(events.len(), 6);
        assert_eq!(events[0].event.as_deref(), Some("message_start"));
        assert_eq!(events[4].event.as_deref(), Some("message_delta"));
        assert_eq!(events[5].event.as_deref(), Some("message_stop"));
        let start: serde_json::Value =
            serde_json::from_str(&events[0].data).expect("message_start data parses");
        assert_eq!(start["message"]["usage"]["input_tokens"], 10);
        let delta: serde_json::Value =
            serde_json::from_str(&events[4].data).expect("message_delta data parses");
        assert_eq!(delta["usage"]["output_tokens"], 5);
    }

    #[test]
    fn anthropic_json_response_has_wire_shape() {
        let v = anthropic_json_response("msg_1", "claude-opus-5", "hi", 10, 5);
        assert_eq!(v["type"], "message");
        assert_eq!(v["content"][0]["text"], "hi");
        assert_eq!(v["usage"]["input_tokens"], 10);
        assert_eq!(v["usage"]["output_tokens"], 5);
    }
}
