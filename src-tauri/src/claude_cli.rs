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

use futures::StreamExt;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;

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
        if let Some(p) = crate::cli_locator::find_in_path(name) {
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
    crate::cli_locator::login_shell_lookup("command -v claude", runs)
}

/// Whether this command answers `--version` successfully. Guards against
/// stale npm shims that point at a moved or unlinked install.
fn runs(bin: &std::path::Path) -> bool {
    crate::cli_locator::executable_runs(bin, |command| {
        command.env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1");
    })
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

/// LoomRouter-owned permissions injected into Claude Code print turns.
///
/// Claude Code reads machine/user settings, not the LoomRouter repo. To keep
/// the built app usable without editing the user's global Claude settings,
/// LoomRouter writes a temporary settings file with only these allow rules
/// and passes it through `--settings`. This is intentionally narrow: it
/// covers the repo's quality gate, not arbitrary commands.
const INJECTED_CLAUDE_ALLOW: &[&str] = &[
    "WebSearch",
    "WebFetch",
    "Bash(curl:*)",
    "Bash(bun run lint)",
    "Bash(bun run test)",
    "Bash(bun run test:*)",
    "Bash(bun run build)",
    "Bash(cargo fmt --manifest-path src-tauri/Cargo.toml --check)",
    "Bash(cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings)",
    "Bash(cargo test --manifest-path src-tauri/Cargo.toml)",
    "Bash(rtk cargo fmt --manifest-path src-tauri/Cargo.toml --check)",
    "Bash(rtk cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings)",
    "Bash(rtk cargo test --manifest-path src-tauri/Cargo.toml)",
];

/// Write a private temporary Claude settings file carrying only the
/// LoomRouter allowlist, and return its path.
fn injected_claude_settings() -> anyhow::Result<std::path::PathBuf> {
    let dir = std::env::temp_dir().join("loomrouter-claude");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("settings-{}.json", uuid::Uuid::new_v4()));
    let settings = serde_json::json!({
        "permissions": {"allow": INJECTED_CLAUDE_ALLOW},
    });
    crate::secure_fs::write_private(&path, serde_json::to_vec_pretty(&settings)?.as_slice())?;
    Ok(path)
}

/// Best-effort project root for Claude Code project settings.
///
/// Claude Code loads `.claude/settings.json` from its working directory.
/// A built app cannot safely assume the source checkout exists on every
/// machine, so the only cross-platform sources are an explicit override and
/// the process cwd. Set `LOOM_CLAUDE_PROJECT_DIR` to the project root when
/// the app is launched outside the repo and still needs repo-scoped Claude
/// permissions.
fn claude_project_dir() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("LOOM_CLAUDE_PROJECT_DIR") {
        let candidate = std::path::PathBuf::from(dir);
        if candidate.join(".claude").join("settings.json").is_file() {
            return Some(candidate);
        }
    }
    let cwd = std::env::current_dir().ok()?;
    cwd.join(".claude")
        .join("settings.json")
        .is_file()
        .then_some(cwd)
}

/// Mark a LoomRouter-selected workspace as trusted before a non-interactive
/// Claude turn starts. Print mode cannot answer Claude Code's trust dialog,
/// so leaving this false makes the CLI silently ignore project permissions.
fn trust_claude_project(project_dir: &std::path::Path) -> anyhow::Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("home directory not found"))?;
    trust_claude_project_in(&home.join(".claude.json"), project_dir)
}

fn trust_claude_project_in(
    config_path: &std::path::Path,
    project_dir: &std::path::Path,
) -> anyhow::Result<()> {
    static CONFIG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut config = if config_path.is_file() {
        serde_json::from_slice::<Value>(&std::fs::read(config_path)?).map_err(|e| {
            anyhow::anyhow!(
                "could not parse Claude config {}: {e}",
                config_path.display()
            )
        })?
    } else {
        json!({})
    };
    let root = config
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Claude config root must be a JSON object"))?;
    let projects = root
        .entry("projects")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Claude config projects must be a JSON object"))?;
    let project_key = project_dir
        .canonicalize()
        .unwrap_or_else(|_| project_dir.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let project = projects.entry(project_key).or_insert_with(|| json!({}));
    let project = project
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Claude project config must be a JSON object"))?;
    if project
        .get("hasTrustDialogAccepted")
        .and_then(Value::as_bool)
        == Some(true)
    {
        return Ok(());
    }
    project.insert("hasTrustDialogAccepted".to_string(), Value::Bool(true));
    let encoded = serde_json::to_vec_pretty(&config)?;
    crate::secure_fs::write_private_with_backup(config_path, &encoded)?;
    Ok(())
}

fn configure_claude_project(cmd: &mut std::process::Command) -> anyhow::Result<()> {
    if let Some(dir) = claude_project_dir() {
        trust_claude_project(&dir)?;
        cmd.current_dir(dir);
    }
    Ok(())
}

fn configure_print_command(cmd: &mut std::process::Command, model: &str) {
    // Proxy turns are stateless, so persisting them only pollutes Claude Code's
    // session history with entries grouped under the app process cwd. Safe mode
    // keeps subscription auth while excluding user hooks, plugins, MCPs and
    // memory that do not belong in a routed model call.
    cmd.arg("-p")
        .arg("--safe-mode")
        // Print mode has no interactive permission prompt. Accept workspace
        // edits, while commands and network access remain explicitly allowlisted.
        .arg("--permission-mode")
        .arg(claude_permission_mode())
        .arg("--no-session-persistence")
        .arg("--prompt-suggestions")
        .arg("false")
        .arg("--model")
        .arg(model)
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1");
}

fn claude_permission_mode() -> String {
    std::env::var("LOOM_CLAUDE_PERMISSION_MODE")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "acceptEdits".to_string())
}

fn configure_child_environment(cmd: &mut std::process::Command, bin: &std::path::Path) {
    if let Some(path) = crate::cli_locator::child_path(bin) {
        cmd.env("PATH", path);
    }
}

/// One turn's input to the CLI: a flat prompt, or Claude Code's stream-json
/// message protocol when the turn carries images.
pub enum ClaudeTurnInput {
    Text(String),
    StreamJson(String),
}

/// Anthropic SSE frames as the CLI produces them, plus the slot a failure
/// lands in once the frames run out.
pub type StreamedTurn = (
    futures::stream::BoxStream<'static, Result<bytes::Bytes, reqwest::Error>>,
    std::sync::Arc<std::sync::Mutex<Option<String>>>,
);

/// Run one turn and emit Anthropic SSE frames **as the CLI produces them**.
///
/// `run_print_turn` waits for the whole agentic run before emitting anything.
/// `claude -p` is not a single completion — it is a full agent loop that can
/// work for many minutes, so that silence is indistinguishable from a hang.
/// Worse, clients treat a silent stream as a dead one: Codex drops it after
/// 300s with "stream disconnected" and retries, which restarts the run from
/// scratch, so a turn longer than the timeout can never converge.
///
/// Emitting frames as they arrive keeps the stream demonstrably alive and
/// shows the work. The frames are the same Anthropic SSE shape
/// `anthropic_sse_stream` builds, so the existing translator consumes them
/// unchanged — only their arrival is spread over the run.
///
/// The three pipes are drained by separate threads on purpose. A single
/// thread that writes all of stdin before reading stdout deadlocks once the
/// prompt outgrows the 64KB pipe buffer and the child blocks writing output
/// nobody is reading yet — a latent hang for exactly the large prompts this
/// change targets.
///
/// Errors cannot ride the byte stream (its error type is `reqwest::Error`,
/// which this module cannot mint), so a failure lands in the returned slot
/// and the caller appends it once the frames run out.
pub fn stream_print_turn(
    input: ClaudeTurnInput,
    model: &str,
    id: &str,
    config_dir: Option<&std::path::Path>,
) -> anyhow::Result<StreamedTurn> {
    use std::io::{BufRead, BufReader, Write};

    let Some(bin) = claude_binary() else {
        anyhow::bail!("claude CLI not found (set CLAUDE_BIN to its location)");
    };
    let injected = injected_claude_settings()?;
    let model = model.to_string();
    let id = id.to_string();
    let config_dir = config_dir.map(std::path::Path::to_path_buf);

    let mut cmd = std::process::Command::new(&bin);
    crate::cli_locator::hide_console_window(&mut cmd);
    crate::cli_locator::scrub_child_env_std(&mut cmd);
    configure_child_environment(&mut cmd, &bin);
    configure_claude_project(&mut cmd)?;
    configure_print_command(&mut cmd, &model);
    cmd.arg("--settings").arg(&injected);
    if matches!(input, ClaudeTurnInput::StreamJson(_)) {
        cmd.arg("--input-format").arg("stream-json");
    }
    cmd.arg("--output-format")
        .arg("stream-json")
        .arg("--verbose")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(dir) = config_dir {
        cmd.env("CLAUDE_CONFIG_DIR", dir);
    }

    let mut child = cmd.spawn()?;
    let payload = match input {
        ClaudeTurnInput::Text(t) => t,
        ClaudeTurnInput::StreamJson(t) => t,
    };
    if let Some(mut stdin) = child.stdin.take() {
        std::thread::spawn(move || {
            let _ = stdin.write_all(payload.as_bytes());
        });
    }
    // Drained so a chatty CLI can never block on a full stderr pipe.
    let stderr_buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    if let Some(stderr) = child.stderr.take() {
        let sink = stderr_buf.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut guard = sink.lock().unwrap_or_else(|e| e.into_inner());
                if guard.len() < 8192 {
                    guard.push_str(&line);
                    guard.push('\n');
                }
            }
        });
    }

    let failure = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<bytes::Bytes>();
    let fail_slot = failure.clone();

    // Kills the CLI and removes the temp settings file on every early-exit
    // path. Dropping a Child does neither, so without this guard a cancelled
    // turn would leave `claude -p` running tools in the workspace.
    struct ChildGuard {
        child: Option<std::process::Child>,
        injected: std::path::PathBuf,
    }
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if let Some(ref mut child) = self.child {
                let _ = child.kill();
                let _ = child.wait();
            }
            let _ = std::fs::remove_file(&self.injected);
        }
    }
    let mut guard = ChildGuard {
        child: Some(child),
        injected: injected.clone(),
    };

    std::thread::spawn(move || {
        let mut state = StreamTurn::new(&id, &model);
        if let Some(stdout) = guard.child.as_mut().and_then(|c| c.stdout.take()) {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(event) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                for frame in state.on_cli_event(&event) {
                    if tx.send(bytes::Bytes::from(frame)).is_err() {
                        return; // client hung up
                    }
                }
            }
        }
        // Normal exit: the child ran to completion, so wait for its
        // exit status. Defuse the guard so it doesn't kill the
        // already-finished process or re-remove the temp file.
        let status = guard.child.take().unwrap().wait();
        drop(guard);
        let stderr = stderr_buf
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .trim()
            .chars()
            .take(500)
            .collect::<String>();
        let failed = match &status {
            Ok(s) if !s.success() => Some(format!("`claude -p` exited {s}: {stderr}")),
            Err(e) => Some(format!("`claude -p` could not be waited on: {e}")),
            _ => state.error.clone(),
        };
        match failed {
            Some(message) => {
                *fail_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(message);
            }
            None => {
                for frame in state.finish() {
                    if tx.send(bytes::Bytes::from(frame)).is_err() {
                        return;
                    }
                }
            }
        }
    });

    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|chunk| (Ok(chunk), rx))
    })
    .boxed();
    Ok((stream, failure))
}

/// One visible Claude Code tool invocation. The input is reduced to a safe,
/// short label before it reaches this state, so later lifecycle events never
/// need to retain prompts, commands, or tool results.
struct ActiveClaudeTool {
    name: String,
    label: String,
    nested: bool,
    subagent: bool,
    /// Whether any event has arrived carrying this tool's id as its
    /// `parent_tool_use_id`. A subagent's first `tool_result` is only the
    /// launch acknowledgement, and its own events follow; that arrival is how
    /// the two are told apart.
    saw_nested: bool,
    started_at: std::time::Instant,
}

/// Turns Claude Code's `stream-json` lines into Anthropic SSE frames.
///
/// Agentic activity is emitted as synthetic Anthropic thinking blocks. The
/// existing stream translator maps those blocks to Responses reasoning
/// summaries, which makes tools and nested agents visible without asking
/// Codex to execute Claude's already-executed tools a second time.
struct StreamTurn {
    id: String,
    model: String,
    opened: bool,
    next_block_index: usize,
    active_tools: BTreeMap<String, ActiveClaudeTool>,
    input_tokens: u64,
    output_tokens: u64,
    result_text: String,
    /// Top-level assistant text, kept only as the answer of last resort. The
    /// `result` line is the answer; this is what the turn falls back on when
    /// the CLI exits cleanly without ever sending one.
    assistant_text: String,
    error: Option<String>,
}

impl StreamTurn {
    fn new(id: &str, model: &str) -> Self {
        Self {
            id: id.to_string(),
            model: model.to_string(),
            opened: false,
            next_block_index: 0,
            active_tools: BTreeMap::new(),
            input_tokens: 0,
            output_tokens: 0,
            result_text: String::new(),
            assistant_text: String::new(),
            error: None,
        }
    }

    fn ensure_open(&mut self, out: &mut Vec<String>) {
        if self.opened {
            return;
        }
        self.opened = true;
        out.push(anthropic_sse_event(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": self.id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "usage": {"input_tokens": self.input_tokens, "output_tokens": 0},
                },
            }),
        ));
    }

    fn emit_block(&mut self, kind: &str, text: &str, out: &mut Vec<String>) {
        if text.is_empty() {
            return;
        }
        self.ensure_open(out);
        let index = self.next_block_index;
        self.next_block_index += 1;
        let block = if kind == "thinking" {
            json!({"type": "thinking", "thinking": ""})
        } else {
            json!({"type": "text", "text": ""})
        };
        out.push(anthropic_sse_event(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": index,
                "content_block": block,
            }),
        ));
        let delta = if kind == "thinking" {
            json!({"type": "thinking_delta", "thinking": text})
        } else {
            json!({"type": "text_delta", "text": text})
        };
        out.push(anthropic_sse_event(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": index,
                "delta": delta,
            }),
        ));
        out.push(anthropic_sse_event(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": index}),
        ));
    }

    fn emit_progress(&mut self, text: String, out: &mut Vec<String>) {
        self.emit_block("thinking", &format!("{text}\n"), out);
    }

    fn on_tool_use(&mut self, event: &Value, block: &Value, out: &mut Vec<String>) {
        let id = block.get("id").and_then(Value::as_str).unwrap_or("");
        let name = block.get("name").and_then(Value::as_str).unwrap_or("Tool");
        let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
        let nested = event
            .get("parent_tool_use_id")
            .and_then(Value::as_str)
            .is_some();
        let subagent = matches!(name, "Agent" | "Task");
        let label = safe_tool_label(name, &input);
        let progress = if subagent {
            let agent_type = input
                .get("subagent_type")
                .and_then(Value::as_str)
                .map(safe_progress_preview)
                .filter(|value| !value.is_empty());
            match agent_type {
                Some(agent_type) => format!("Subagent started: {label} ({agent_type})"),
                None => format!("Subagent started: {label}"),
            }
        } else if nested {
            with_optional_detail("Subagent tool", name, &label)
        } else {
            with_optional_detail("Tool started", name, &label)
        };
        self.emit_progress(progress, out);
        if !id.is_empty() {
            self.active_tools.insert(
                id.to_string(),
                ActiveClaudeTool {
                    name: name.to_string(),
                    label,
                    nested,
                    subagent,
                    saw_nested: false,
                    started_at: std::time::Instant::now(),
                },
            );
        }
    }

    fn on_tool_result(&mut self, block: &Value, out: &mut Vec<String>) {
        let id = block
            .get("tool_use_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let Some(tool) = self.active_tools.remove(id) else {
            return;
        };
        let is_error = block.get("is_error").and_then(Value::as_bool) == Some(true);
        // A subagent acknowledges its launch with a first `tool_result`, then
        // keeps working; its answer arrives in a second one. The two are told
        // apart by whether the subagent's own events have started arriving,
        // not by the wording of the acknowledgement — that text belongs to
        // Claude Code and can be reworded without anything here failing.
        if tool.subagent && !is_error && !tool.saw_nested {
            self.emit_progress(format!("Subagent running: {}", tool.label), out);
            self.active_tools.insert(id.to_string(), tool);
            return;
        }
        let elapsed = format_elapsed(tool.started_at.elapsed());
        let status = if is_error { "failed" } else { "completed" };
        // A subagent is named by its task, the way its start line and `finish`
        // both name it. `Agent` identifies nothing once more than one has run.
        let (prefix, descriptor) = if tool.subagent {
            let descriptor = if tool.label.is_empty() {
                tool.name.clone()
            } else {
                tool.label.clone()
            };
            ("Subagent", descriptor)
        } else if tool.nested {
            ("Subagent tool", tool.name.clone())
        } else {
            ("Tool", tool.name.clone())
        };
        self.emit_progress(format!("{prefix} {status}: {descriptor} ({elapsed})"), out);
    }

    /// Record that a subagent has started producing its own events, which is
    /// what separates its launch acknowledgement from its answer.
    fn note_nested_activity(&mut self, event: &Value) {
        let Some(parent) = event.get("parent_tool_use_id").and_then(Value::as_str) else {
            return;
        };
        if let Some(tool) = self.active_tools.get_mut(parent) {
            tool.saw_nested = true;
        }
    }

    fn on_cli_event(&mut self, event: &Value) -> Vec<String> {
        let mut out = Vec::new();
        self.note_nested_activity(event);
        match event.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                if let Some(usage) = event.pointer("/message/usage") {
                    if let Some(n) = usage.get("input_tokens").and_then(Value::as_u64) {
                        if self.input_tokens == 0 {
                            self.input_tokens = n;
                        }
                    }
                }
                let blocks = event
                    .pointer("/message/content")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                            if text.is_empty() {
                                continue;
                            }
                            let nested = event
                                .get("parent_tool_use_id")
                                .and_then(Value::as_str)
                                .is_some();
                            if !nested {
                                // Untruncated and unredacted, unlike the
                                // progress line below: this is only ever read
                                // when no `result` line arrived, and it is then
                                // the answer rather than a summary of one.
                                if !self.assistant_text.is_empty() {
                                    self.assistant_text.push('\n');
                                }
                                self.assistant_text.push_str(text);
                            }
                            let prefix = if nested {
                                "Subagent update"
                            } else {
                                "Claude update"
                            };
                            self.emit_progress(
                                format!("{prefix}: {}", safe_progress_preview(text)),
                                &mut out,
                            );
                        }
                        Some("tool_use") => self.on_tool_use(event, &block, &mut out),
                        // Raw thinking is private chain of thought. Only
                        // observable actions become user-visible progress.
                        _ => {}
                    }
                }
            }
            Some("user") => {
                let blocks = event
                    .pointer("/message/content")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                        self.on_tool_result(&block, &mut out);
                    }
                }
            }
            Some("result") => {
                if event.get("is_error").and_then(Value::as_bool) == Some(true) {
                    self.error = Some(format!(
                        "`claude -p` returned an error: {}",
                        event.get("result").and_then(Value::as_str).unwrap_or("")
                    ));
                    return out;
                }
                self.result_text = event
                    .get("result")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if let Some(usage) = event.get("usage") {
                    if let Some(n) = usage.get("input_tokens").and_then(Value::as_u64) {
                        if self.input_tokens == 0 {
                            self.input_tokens = n;
                        }
                    }
                    self.output_tokens = usage
                        .get("output_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                }
            }
            _ => {}
        }
        out
    }

    /// Closing frames. A run that reported no assistant text still has to
    /// deliver the `result` line's answer, or the turn arrives empty.
    fn finish(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        let unfinished_subagents: Vec<(String, std::time::Duration)> = self
            .active_tools
            .values()
            .filter(|tool| tool.subagent)
            .map(|tool| (tool.label.clone(), tool.started_at.elapsed()))
            .collect();
        for (label, elapsed) in unfinished_subagents {
            self.emit_progress(
                format!("Subagent completed: {label} ({})", format_elapsed(elapsed)),
                &mut out,
            );
        }
        self.active_tools.clear();
        self.ensure_open(&mut out);
        // A clean exit with no `result` line still has to deliver an answer:
        // the assistant text is all there is, and emitting nothing would turn
        // a truncated run into an empty turn.
        if self.result_text.is_empty() {
            self.result_text = std::mem::take(&mut self.assistant_text);
        }
        if !self.result_text.is_empty() {
            let text = std::mem::take(&mut self.result_text);
            self.emit_block("text", &text, &mut out);
        }
        out.push(anthropic_sse_event(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                // input_tokens ride along because message_start was emitted
                // before the CLI reported them on short runs.
                "usage": {
                    "input_tokens": self.input_tokens,
                    "output_tokens": self.output_tokens,
                },
            }),
        ));
        out.push(anthropic_sse_event(
            "message_stop",
            &json!({"type": "message_stop"}),
        ));
        out
    }
}

fn with_optional_detail(prefix: &str, name: &str, detail: &str) -> String {
    if detail.is_empty() {
        format!("{prefix}: {name}")
    } else {
        format!("{prefix}: {name}, {detail}")
    }
}

fn safe_tool_label(name: &str, input: &Value) -> String {
    if matches!(name, "Agent" | "Task") {
        return input
            .get("description")
            .and_then(Value::as_str)
            .map(safe_progress_preview)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "unnamed task".to_string());
    }
    if matches!(name, "Read" | "Write" | "Edit" | "NotebookEdit") {
        return input
            .get("file_path")
            .or_else(|| input.get("notebook_path"))
            .and_then(Value::as_str)
            .map(compact_path)
            .unwrap_or_default();
    }
    if name == "Bash" {
        return input
            .get("description")
            .and_then(Value::as_str)
            .or_else(|| input.get("command").and_then(Value::as_str))
            .map(safe_progress_preview)
            .unwrap_or_default();
    }
    if matches!(name, "Glob" | "Grep") {
        let pattern = input
            .get("pattern")
            .and_then(Value::as_str)
            .map(safe_progress_preview)
            .unwrap_or_default();
        let path = input
            .get("path")
            .and_then(Value::as_str)
            .map(compact_path)
            .unwrap_or_default();
        return match (pattern.is_empty(), path.is_empty()) {
            (false, false) => format!("{pattern} in {path}"),
            (false, true) => pattern,
            (true, false) => path,
            (true, true) => String::new(),
        };
    }
    // First non-empty, not first present: an empty `description` would
    // otherwise mask the `query` or `url` that says what the tool is doing.
    for field in ["description", "query", "url"] {
        let preview = input
            .get(field)
            .and_then(Value::as_str)
            .map(safe_progress_preview)
            .unwrap_or_default();
        if !preview.is_empty() {
            return preview;
        }
    }
    String::new()
}

fn compact_path(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let parts: Vec<&str> = normalized
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    parts[parts.len().saturating_sub(3)..].join("/")
}

/// Punctuation that wraps a word without being part of it: quotes from a
/// shell or JSON, and the separators around a value in either.
const WRAPPERS: [char; 10] = ['"', '\'', '`', '{', '}', '[', ']', '(', ')', ','];

fn unwrap_word(word: &str) -> &str {
    word.trim_matches(|c: char| WRAPPERS.contains(&c) || c == ';')
}

/// Whether a key name means whatever it is bound to is a credential.
fn is_secret_key(key: &str) -> bool {
    let key = unwrap_word(key).to_ascii_lowercase();
    const NEEDLES: [&str; 9] = [
        "token",
        "secret",
        "password",
        "passwd",
        "apikey",
        "api_key",
        "api-key",
        "credential",
        "authorization",
    ];
    NEEDLES.iter().any(|needle| key.contains(needle)) || key.ends_with("key")
}

/// Whether a value is a credential on its own, whatever it is attached to.
/// Prefix-keyed rather than entropy-keyed: these are the issued shapes, and
/// guessing at randomness would redact ordinary identifiers too.
fn looks_like_secret(value: &str) -> bool {
    let value = unwrap_word(value);
    const PREFIXES: [&str; 10] = [
        "sk-",
        "sk_",
        "pk_",
        "rk_",
        "ghp_",
        "gho_",
        "ghs_",
        "github_pat_",
        "xoxb-",
        "glpat-",
    ];
    let lower = value.to_ascii_lowercase();
    if PREFIXES.iter().any(|prefix| lower.starts_with(prefix)) {
        return true;
    }
    // AWS access key ids are a fixed prefix and length.
    value.len() >= 16 && (value.starts_with("AKIA") || value.starts_with("ASIA"))
}

/// Reduce free text to something safe to show as progress.
///
/// Credentials reach here through shell commands and tool descriptions, in
/// whatever shape the caller wrote them: `k=v`, `k: v`, a JSON pair, an
/// `Authorization` header, or a bare issued token. Splitting on whitespace and
/// comparing whole words missed all but the first, since a quote or a colon is
/// enough to stop an equality test matching.
fn safe_progress_preview(text: &str) -> String {
    const MAX_CHARS: usize = 240;
    let mut out: Vec<String> = Vec::new();
    let mut redact_next = false;
    for word in text.split_whitespace() {
        let bare = unwrap_word(word);
        let lower = bare.to_ascii_lowercase();
        if redact_next {
            // An auth scheme is not itself the secret; the credential is the
            // word after it, so stay armed across one.
            if matches!(lower.as_str(), "bearer" | "token" | "basic" | "digest") {
                out.push(word.to_string());
                continue;
            }
            out.push("[redacted]".to_string());
            redact_next = false;
            continue;
        }
        // Flags whose argument is a credential by definition.
        if matches!(lower.as_str(), "-u" | "--user" | "--password" | "--token") {
            out.push(word.to_string());
            redact_next = true;
            continue;
        }
        // `Authorization:` and friends: the value is the rest of the header,
        // which whitespace has already split away.
        if let Some(key) = lower.strip_suffix(':') {
            if is_secret_key(key) {
                out.push(word.to_string());
                redact_next = true;
                continue;
            }
        }
        // `key=value`, `key:value`, `"key":"value"` — all one word.
        let split = bare
            .split_once('=')
            .or_else(|| bare.split_once(':'))
            .filter(|(key, value)| !key.is_empty() && !value.is_empty());
        if let Some((key, _)) = split {
            if is_secret_key(key) {
                out.push(format!("{}=[redacted]", unwrap_word(key)));
                continue;
            }
        }
        if looks_like_secret(bare) || split.is_some_and(|(_, value)| looks_like_secret(value)) {
            out.push("[redacted]".to_string());
            continue;
        }
        out.push(word.to_string());
    }
    let normalized = out.join(" ");
    let mut chars = normalized.chars();
    let preview: String = chars.by_ref().take(MAX_CHARS).collect();
    if chars.next().is_some() {
        format!("{preview}...")
    } else {
        preview
    }
}

fn format_elapsed(elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs();
    if secs >= 3600 {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{:.1}s", elapsed.as_secs_f64())
    }
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
    let injected = injected_claude_settings()?;
    tokio::task::spawn_blocking(move || {
        let mut cmd = std::process::Command::new(&bin);
        crate::cli_locator::hide_console_window(&mut cmd);
        crate::cli_locator::scrub_child_env_std(&mut cmd);
        configure_child_environment(&mut cmd, &bin);
        configure_claude_project(&mut cmd)?;
        configure_print_command(&mut cmd, &model);
        cmd.arg("--settings")
            .arg(&injected)
            .arg("--output-format")
            .arg("json")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(dir) = config_dir {
            cmd.env("CLAUDE_CONFIG_DIR", dir);
        }
        let mut child = cmd.spawn()?;
        use std::io::Write;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(prompt.as_bytes())?;
        }
        let out = child.wait_with_output()?;
        let _ = std::fs::remove_file(&injected);
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

/// Run one non-interactive turn through the local `claude` CLI with
/// structured content. Claude Code's `stream-json` input protocol accepts
/// Anthropic-style content blocks on stdin, including base64/URL image
/// blocks; print mode with `--output-format json` only accepts flat text.
///
/// This is the path used when a routed request contains image parts. It
/// keeps the same CLI/login contract as `run_print_turn`, but lets Claude
/// Code receive the actual image instead of dropping it while flattening to
/// text.
pub async fn run_print_turn_stream_json(
    messages: &[Value],
    model: &str,
    config_dir: Option<&std::path::Path>,
) -> anyhow::Result<ClaudePrintResult> {
    let Some(bin) = claude_binary() else {
        anyhow::bail!("claude CLI not found (set CLAUDE_BIN to its location)");
    };
    let input = render_stream_json(messages)?;
    let model = model.to_string();
    let config_dir = config_dir.map(std::path::Path::to_path_buf);
    let injected = injected_claude_settings()?;
    tokio::task::spawn_blocking(move || {
        let mut cmd = std::process::Command::new(&bin);
        crate::cli_locator::hide_console_window(&mut cmd);
        crate::cli_locator::scrub_child_env_std(&mut cmd);
        configure_child_environment(&mut cmd, &bin);
        configure_claude_project(&mut cmd)?;
        configure_print_command(&mut cmd, &model);
        cmd.arg("--settings")
            .arg(&injected)
            .arg("--input-format")
            .arg("stream-json")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(dir) = config_dir {
            cmd.env("CLAUDE_CONFIG_DIR", dir);
        }
        let mut child = cmd.spawn()?;
        use std::io::Write;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(input.as_bytes())?;
        }
        let out = child.wait_with_output()?;
        let _ = std::fs::remove_file(&injected);
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            anyhow::bail!(
                "`claude -p` exited {}: {}",
                out.status,
                stderr.trim().chars().take(500).collect::<String>()
            );
        }
        parse_stream_result(&out.stdout)
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

/// Parse the `claude -p --output-format stream-json` answer into a usable
/// turn. Stream output contains many lifecycle lines; the final `result`
/// line carries the same text, session id and usage fields as plain JSON.
fn parse_stream_result(stdout: &[u8]) -> anyhow::Result<ClaudePrintResult> {
    let mut result: Option<Value> = None;
    for line in stdout.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_slice::<Value>(line) {
            if v.get("type").and_then(Value::as_str) == Some("result") {
                result = Some(v);
            }
        }
    }
    let v = match result {
        Some(v) => v,
        None => {
            let v: Value = serde_json::from_slice(stdout).map_err(|e| {
                anyhow::anyhow!(
                    "could not parse `claude -p` stream output: {e}: {}",
                    String::from_utf8_lossy(stdout)
                        .chars()
                        .take(200)
                        .collect::<String>()
                )
            })?;
            v
        }
    };
    if v.get("is_error").and_then(Value::as_bool) == Some(true) {
        anyhow::bail!(
            "`claude -p` returned an error: {}",
            v.get("result").and_then(Value::as_str).unwrap_or("")
        );
    }
    let usage = v.get("usage").cloned().unwrap_or_else(|| json!({}));
    Ok(ClaudePrintResult {
        text: v
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        session_id: v
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        input_tokens: usage
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        total_cost_usd: v
            .get("total_cost_usd")
            .and_then(Value::as_f64)
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

/// Whether a chat transcript contains any image parts that must be sent
/// through the structured CLI input path.
pub fn messages_have_images(messages: &[Value]) -> bool {
    messages.iter().any(|message| {
        message
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|content| {
                content.iter().any(|part| {
                    matches!(
                        part.get("type").and_then(Value::as_str),
                        Some("input_image" | "image_url" | "image")
                    )
                })
            })
    })
}

/// Render a chat-message transcript into Claude Code's `stream-json` input.
///
/// Claude Code's structured stdin protocol expects one JSON message per
/// line. We keep the same role-marking style as `render_prompt`, but image
/// parts are emitted as Anthropic image blocks so the CLI receives the
/// actual attachment instead of text-only flattened prompt.
pub fn render_stream_json(messages: &[Value]) -> anyhow::Result<String> {
    let mut lines = Vec::new();
    for message in messages {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        if role == "assistant" && message.get("tool_calls").is_some() {
            continue;
        }
        let Some(blocks) = claude_content_blocks(message, role)? else {
            continue;
        };
        lines.push(serde_json::to_string(&json!({
            "type": "user",
            "message": {"role": "user", "content": blocks},
        }))?);
    }
    if lines.is_empty() {
        lines.push(serde_json::to_string(&json!({
            "type": "user",
            "message": {"role": "user", "content": [{"type": "text", "text": "Continue"}]},
        }))?);
    }
    Ok(format!("{}\n", lines.join("\n")))
}

fn claude_content_blocks(message: &Value, role: &str) -> anyhow::Result<Option<Vec<Value>>> {
    let prefix = match role {
        "system" => "System instructions:\n",
        "assistant" => "Assistant:\n",
        "tool" => "Tool result (from a previous turn):\n",
        _ => "User:\n",
    };
    match message.get("content") {
        Some(Value::String(text)) if !text.is_empty() => Ok(Some(vec![json!({
            "type": "text",
            "text": format!("{prefix}{text}"),
        })])),
        Some(Value::Array(parts)) => {
            let mut text = String::new();
            let mut images = Vec::new();
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("input_text" | "text" | "output_text") => {
                        if let Some(value) = part.get("text").and_then(Value::as_str) {
                            text.push_str(value);
                        }
                    }
                    Some("encrypted_content") => {
                        if let Some(value) = part.get("encrypted_content").and_then(Value::as_str) {
                            text.push_str(value);
                        }
                    }
                    Some("input_image" | "image_url") => {
                        if let Some(image) = image_content_block(part) {
                            images.push(image);
                        }
                    }
                    Some("image") => {
                        images.push(part.clone());
                    }
                    _ => {}
                }
            }
            if text.is_empty() && images.is_empty() {
                return Ok(None);
            }
            let mut blocks = Vec::new();
            if !text.is_empty() || images.is_empty() {
                blocks.push(json!({
                    "type": "text",
                    "text": format!("{prefix}{text}"),
                }));
            }
            blocks.extend(images);
            Ok(Some(blocks))
        }
        _ => Ok(None),
    }
}

fn image_content_block(part: &Value) -> Option<Value> {
    let url = part.get("image_url").and_then(|value| match value {
        Value::String(url) => Some(url.clone()),
        Value::Object(_) => value.get("url").and_then(Value::as_str).map(str::to_string),
        _ => None,
    })?;
    if let Some(rest) = url.strip_prefix("data:") {
        let (metadata, data) = rest.split_once(',')?;
        let media_type = metadata
            .split(';')
            .next()
            .filter(|mime| !mime.is_empty())
            .unwrap_or("image/png")
            .to_string();
        Some(json!({
            "type": "image",
            "source": {"type": "base64", "media_type": media_type, "data": data},
        }))
    } else {
        Some(json!({
            "type": "image",
            "source": {"type": "url", "url": url},
        }))
    }
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
    crate::cli_locator::scrub_child_env_tokio(&mut command);
    command
        .args(["auth", "status"])
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
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

    fn parsed_stream_events(frames: Vec<String>) -> Vec<(String, Value)> {
        let mut parser = crate::sse::SseParser::new();
        let joined: Vec<u8> = frames
            .iter()
            .flat_map(|frame| frame.as_bytes().iter().copied())
            .collect();
        parser
            .push(&joined)
            .into_iter()
            .map(|event| {
                (
                    event.event.unwrap_or_default(),
                    serde_json::from_str(&event.data).expect("SSE data must be JSON"),
                )
            })
            .collect()
    }

    #[test]
    fn streamed_subagent_activity_is_progress_not_answer_text() {
        let mut turn = StreamTurn::new("msg_1", "claude-opus-5");
        let mut frames = Vec::new();
        frames.extend(turn.on_cli_event(&json!({
            "type": "assistant",
            "message": {"content": [{
                "type": "tool_use",
                "id": "agent_1",
                "name": "Agent",
                "input": {
                    "description": "Map payment retries",
                    "subagent_type": "Explore",
                    "prompt": "private-agent-prompt"
                }
            }]}
        })));
        frames.extend(turn.on_cli_event(&json!({
            "type": "assistant",
            "parent_tool_use_id": "agent_1",
            "message": {"content": [{
                "type": "tool_use",
                "id": "read_1",
                "name": "Read",
                "input": {"file_path": "/workspace/src/payments/retry.rs"}
            }]}
        })));
        frames.extend(turn.on_cli_event(&json!({
            "type": "assistant",
            "parent_tool_use_id": "agent_1",
            "message": {"content": [{
                "type": "text",
                "text": "Found the retry boundary."
            }]}
        })));
        frames.extend(turn.on_cli_event(&json!({
            "type": "assistant",
            "message": {"content": [{"type": "text", "text": "Preparing the final answer."}]}
        })));
        frames.extend(turn.on_cli_event(&json!({
            "type": "result",
            "is_error": false,
            "result": "Final answer.",
            "usage": {"input_tokens": 10, "output_tokens": 3}
        })));
        frames.extend(turn.finish());

        let events = parsed_stream_events(frames);
        let progress: String = events
            .iter()
            .filter(|(_, data)| {
                data.pointer("/delta/type").and_then(Value::as_str) == Some("thinking_delta")
            })
            .filter_map(|(_, data)| data.pointer("/delta/thinking").and_then(Value::as_str))
            .collect();
        let answer: String = events
            .iter()
            .filter(|(_, data)| {
                data.pointer("/delta/type").and_then(Value::as_str) == Some("text_delta")
            })
            .filter_map(|(_, data)| data.pointer("/delta/text").and_then(Value::as_str))
            .collect();

        assert!(progress.contains("Subagent started: Map payment retries (Explore)"));
        assert!(progress.contains("Subagent tool: Read, src/payments/retry.rs"));
        assert!(progress.contains("Subagent update: Found the retry boundary."));
        assert!(progress.contains("Claude update: Preparing the final answer."));
        assert!(!progress.contains("private-agent-prompt"));
        assert_eq!(answer, "Final answer.");
    }

    #[test]
    fn streamed_tool_activity_redacts_secrets_and_reports_completion() {
        // No `description`: the command itself has to be previewed, which is
        // the only path on which redaction runs at all. With one present the
        // command is never rendered, and this test would pass with every
        // branch of `safe_progress_preview` deleted.
        let mut turn = StreamTurn::new("msg_1", "claude-opus-5");
        let mut frames = turn.on_cli_event(&json!({
            "type": "assistant",
            "message": {"content": [{
                "type": "tool_use",
                "id": "bash_1",
                "name": "Bash",
                "input": {
                    "command": "API_KEY=super-secret curl -H \"x-api-key: header-secret\" \
                                -u admin:basic-secret https://example.test/v1"
                }
            }]}
        }));
        frames.extend(turn.on_cli_event(&json!({
            "type": "user",
            "message": {"content": [{
                "type": "tool_result",
                "tool_use_id": "bash_1",
                "content": "request complete"
            }]}
        })));

        let serialized = parsed_stream_events(frames)
            .into_iter()
            .map(|(_, data)| data.to_string())
            .collect::<String>();
        assert!(
            serialized.contains("Tool started: Bash"),
            "the tool start must be reported: {serialized}"
        );
        assert!(
            serialized.contains("Tool completed: Bash"),
            "the tool completion must be reported: {serialized}"
        );
        // The surviving text proves the command was previewed, so the absences
        // below are redaction rather than a path that renders nothing.
        assert!(
            serialized.contains("curl"),
            "the command must reach the preview: {serialized}"
        );
        for secret in ["super-secret", "header-secret", "basic-secret"] {
            assert!(
                !serialized.contains(secret),
                "{secret} survived redaction: {serialized}"
            );
        }
    }

    #[test]
    fn redaction_covers_the_shapes_credentials_actually_arrive_in() {
        // Each pair is one credential shape and the substring that must not
        // survive it. Whitespace-separated word equality caught only the
        // first: a quote or a colon is enough to stop it matching.
        for (input, secret) in [
            ("API_KEY=aaa111", "aaa111"),
            ("curl -H \"x-api-key: bbb222\"", "bbb222"),
            ("curl -u admin:ccc333 https://example.test", "ccc333"),
            ("{\"api_key\":\"ddd444\"}", "ddd444"),
            ("Authorization: Bearer eee555", "eee555"),
            ("Authorization: Token fff666", "fff666"),
            ("sk-ant-api03-ggg777", "ggg777"),
            ("ghp_hhh888iii999jjj000", "hhh888"),
            ("xoxb-111-222-kkk111", "kkk111"),
            ("glpat-lll222mmm333", "lll222"),
            ("AKIAIOSFODNN7EXAMPLE", "AKIAIOSFODNN7EXAMPLE"),
        ] {
            let preview = safe_progress_preview(input);
            assert!(
                !preview.contains(secret),
                "{input} leaked {secret} as {preview}"
            );
        }
    }

    #[test]
    fn redaction_leaves_ordinary_text_readable() {
        // Over-redaction is its own failure: progress nobody can read is the
        // same as no progress.
        for text in [
            "Run the payment retry suite",
            "curl https://example.test/v1/health",
            "grep -rn TODO src/",
        ] {
            assert_eq!(safe_progress_preview(text), text, "over-redacted");
        }
    }

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
    fn injected_claude_settings_contains_only_gate_allowlist() {
        let path = injected_claude_settings().unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let settings: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let allow = settings["permissions"]["allow"].as_array().unwrap();
        assert_eq!(allow.len(), INJECTED_CLAUDE_ALLOW.len());
        assert_eq!(allow[0], "WebSearch");
        assert!(allow.contains(&json!("WebFetch")));
        assert!(allow.contains(&json!("Bash(curl:*)")));
        assert!(!raw.contains("Bash(*)"));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn trust_claude_project_marks_only_this_project() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("repo");
        let config = dir.path().join(".claude.json");
        std::fs::create_dir_all(&project).unwrap();

        trust_claude_project_in(&config, &project).unwrap();

        let parsed: Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
        let key = project
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(parsed["projects"][key]["hasTrustDialogAccepted"], true);
        assert!(!serde_json::to_string(&parsed)
            .unwrap()
            .contains("access_token"));
    }

    #[test]
    fn trusting_project_preserves_existing_claude_config() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join(".claude.json");
        let project_dir = dir.path().join("workspace");
        std::fs::create_dir(&project_dir).unwrap();
        let project_key = project_dir
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        std::fs::write(
            &config_path,
            serde_json::to_vec(&json!({
                "theme": "dark",
                "projects": {
                    (project_key.clone()): {
                        "allowedTools": ["Read"],
                        "hasTrustDialogAccepted": false
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        trust_claude_project_in(&config_path, &project_dir).unwrap();

        let config: Value = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
        assert_eq!(config["theme"], "dark");
        assert_eq!(config["projects"][&project_key]["allowedTools"][0], "Read");
        assert_eq!(
            config["projects"][&project_key]["hasTrustDialogAccepted"],
            true
        );
        assert!(dir.path().join(".claude.json.bak").is_file());
    }

    #[test]
    fn trusting_new_project_creates_projects_map() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join(".claude.json");
        let project_dir = dir.path().join("workspace");
        std::fs::create_dir(&project_dir).unwrap();

        trust_claude_project_in(&config_path, &project_dir).unwrap();

        let config: Value = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
        let project_key = project_dir
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            config["projects"][&project_key]["hasTrustDialogAccepted"],
            true
        );
    }

    #[test]
    fn invalid_claude_config_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join(".claude.json");
        std::fs::write(&config_path, b"not-json").unwrap();

        assert!(trust_claude_project_in(&config_path, dir.path()).is_err());
        assert_eq!(std::fs::read(&config_path).unwrap(), b"not-json");
        assert!(!dir.path().join(".claude.json.bak").exists());
    }

    /// `LOOM_CLAUDE_PERMISSION_MODE` is process-wide, so the test that sets it
    /// and the test that asserts the default must not run at the same time.
    static PERMISSION_MODE_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn proxy_print_turns_do_not_persist_claude_sessions() {
        let _guard = PERMISSION_MODE_ENV
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut command = std::process::Command::new("claude");
        configure_print_command(&mut command, "claude-opus-5");
        let args: Vec<_> = command.get_args().collect();

        assert_eq!(
            args,
            [
                "-p",
                "--safe-mode",
                "--permission-mode",
                "acceptEdits",
                "--no-session-persistence",
                "--prompt-suggestions",
                "false",
                "--model",
                "claude-opus-5"
            ]
        );

        assert_eq!(
            command
                .get_envs()
                .find(|(key, _)| *key
                    == std::ffi::OsStr::new("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC")),
            Some((
                std::ffi::OsStr::new("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"),
                Some(std::ffi::OsStr::new("1"))
            ))
        );
        assert!(command
            .get_envs()
            .all(|(key, _)| key != std::ffi::OsStr::new("CLAUDE_CODE_SKIP_BACKGROUND_PREFETCH")));
    }

    #[test]
    fn claude_permission_mode_can_be_overridden_for_proxy_turns() {
        let _guard = PERMISSION_MODE_ENV
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let saved = std::env::var("LOOM_CLAUDE_PERMISSION_MODE").ok();
        // SAFETY: no other test reads this var while the guard is held.
        unsafe { std::env::set_var("LOOM_CLAUDE_PERMISSION_MODE", "bypassPermissions") }
        let mut command = std::process::Command::new("claude");
        configure_print_command(&mut command, "claude-opus-5");
        let args: Vec<_> = command.get_args().collect();
        unsafe {
            match saved {
                Some(value) => std::env::set_var("LOOM_CLAUDE_PERMISSION_MODE", value),
                None => std::env::remove_var("LOOM_CLAUDE_PERMISSION_MODE"),
            }
        }
        let position = args
            .iter()
            .position(|arg| *arg == "--permission-mode")
            .unwrap();
        assert_eq!(args[position + 1], "bypassPermissions");
    }

    #[test]
    fn messages_have_images_detects_structured_image_parts() {
        let messages = serde_json::json!([
            {"role": "user", "content": [{"type": "input_image", "image_url": "data:image/png;base64,AAAA"}]}
        ]);
        assert!(messages_have_images(messages.as_array().unwrap()));

        let text_only = serde_json::json!([
            {"role": "user", "content": [{"type": "text", "text": "No image"}]}
        ]);
        assert!(!messages_have_images(text_only.as_array().unwrap()));
    }

    #[test]
    fn render_stream_json_preserves_text_and_image_blocks() {
        let messages = serde_json::json!([
            {"role": "system", "content": [{"type": "input_text", "text": "Be brief."}]},
            {"role": "assistant", "content": [{"type": "output_text", "text": "Okay."}], "tool_calls": [{"id": "t1"}]},
            {"role": "user", "content": [
                {"type": "input_text", "text": "Inspect this."},
                {"type": "input_image", "image_url": "data:image/png;base64,aGVsbG8="},
                {"type": "image_url", "image_url": {"url": "https://example.test/a.png"}}
            ]}
        ]);
        let out = render_stream_json(messages.as_array().unwrap()).unwrap();
        assert!(out.contains("System instructions:\\nBe brief."));
        assert!(!out.contains("Okay."));
        assert!(out.contains("User:\\nInspect this."));
        assert!(out.contains(r#""media_type":"image/png""#));
        assert!(out.contains(r#""data":"aGVsbG8=""#));
        assert!(out.contains(r#""url":"https://example.test/a.png""#));
    }

    #[test]
    fn parse_stream_result_reads_final_result_line() {
        let stdout = concat!(
            "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s1\"}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"partial\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"session_id\":\"s2\",\"is_error\":false,\"total_cost_usd\":0.02,\"result\":\"Done.\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}\n",
        );
        let r = parse_stream_result(stdout.as_bytes()).unwrap();
        assert_eq!(r.text, "Done.");
        assert_eq!(r.session_id, "s2");
        assert_eq!(r.input_tokens, 10);
        assert_eq!(r.output_tokens, 5);
        assert!((r.total_cost_usd - 0.02).abs() < 1e-9);
    }

    #[test]
    fn parse_stream_result_flags_is_error() {
        let stdout = br#"{"type":"result","is_error":true,"result":"oops"}"#;
        assert!(parse_stream_result(stdout).is_err());
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
