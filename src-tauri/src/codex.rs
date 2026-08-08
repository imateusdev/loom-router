//! Codex integration: the piece that makes external models appear in
//! Codex's own model picker alongside the native GPT models.
//!
//! Artifacts:
//!   1. A marked managed block in `~/.codex/config.toml` pointing
//!      `openai_base_url` at the LoomRouter proxy and `model_catalog_json`
//!      at the merged catalog.
//!   2. `~/.codex/loom-router/native-models.json`: the native catalog,
//!      captured from `codex debug models`.
//!   3. `~/.codex/loom-router/merged-models.json`: native entries plus one
//!      entry per enabled external model, built by cloning a native template
//!      (the same schema Codex itself emits).
//!
//! Everything LoomRouter writes to config.toml is wrapped in BEGIN/END
//! markers so removal is exact and the user's own settings are untouched.
//!
//! ## Slug modes (`AppConfig::native_slug_mode`)
//!
//! Research summary (Codex CLI source, `codex-rs/model-provider-info` and
//! `codex-rs/models-manager`, plus the official configuration reference):
//! whether Codex demands an OpenAI/ChatGPT login is a *provider-level*
//! decision (`model_providers.<id>.requires_openai_auth`), not a slug-format
//! decision. The picker lists whatever `model_catalog_json` contains; the
//! slug shape only affects metadata lookup (a `namespace/model` slug gets a
//! single leading segment stripped for longest-prefix matching) and display.
//!
//! - **Routed mode (default, `native_slug_mode = false`)**: external models
//!   are published as `provider/model` slugs and the managed provider keeps
//!   `requires_openai_auth = true`, so ChatGPT login is required and native
//!   GPT models keep working through the proxy passthrough (Codex forwards
//!   its ChatGPT token). This matches the Codex Desktop picker quirk of only
//!   rendering custom models for OpenAI-auth providers.
//! - **Native slug mode (`native_slug_mode = true`)**: for users without an
//!   OpenAI login. The managed provider flips to
//!   `requires_openai_auth = false` (auth is the local proxy bearer token in
//!   `http_headers`, which Codex always sends), and external models are
//!   republished under *bare* slugs (the raw model id, e.g. `k3` instead of
//!   `kimi-coding/k3`) so they look and resolve like native entries. The
//!   LoomRouter proxy resolves bare ids to the unique enabled provider
//!   serving that model, so no proxy change is needed. Native GPT entries
//!   are dropped from the merged catalog in this mode: without a ChatGPT
//!   token they can only fail, and leaving them in the picker is noise.

use crate::config::AppConfig;
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::path::PathBuf;

#[path = "codex/config_patch.rs"]
mod config_patch;
use config_patch::write_config_atomic;
pub use config_patch::{
    active_slug, apply, current_root_model, multi_agent_enabled, owns_slug, published_slug, remove,
    set_multi_agent, BEGIN_MARK, END_MARK,
};

#[derive(Debug, Clone, Serialize)]
pub struct CodexStatus {
    pub codex_home: String,
    pub config_exists: bool,
    pub managed_block_present: bool,
    /// A `# BEGIN` marker without a matching `# END`: an external rewrite of
    /// config.toml (e.g. the Codex desktop app re-serializing the file)
    /// dropped the trailing comment. `strip_managed_block` recovers from
    /// this by ownership, but the UI should surface it instead of showing a
    /// dead Apply/Remove button.
    pub managed_block_orphaned: bool,
    pub native_catalog_present: bool,
    pub merged_catalog_present: bool,
    pub merged_model_count: usize,
    pub codex_cli_available: bool,
    /// Whether auto-apply is on (user clicked Apply at least once).
    pub integration_enabled: bool,
}

pub fn codex_home() -> PathBuf {
    if let Ok(custom) = std::env::var("CODEX_HOME") {
        return PathBuf::from(custom);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex")
}

#[cfg(test)]
fn codex_home_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn loom_dir() -> PathBuf {
    codex_home().join("loom-router")
}

fn merged_catalog_path() -> PathBuf {
    loom_dir().join("merged-models.json")
}

fn native_catalog_path() -> PathBuf {
    loom_dir().join("native-models.json")
}

pub fn status(config: &AppConfig) -> CodexStatus {
    let home = codex_home();
    let cfg_path = home.join("config.toml");
    let raw = std::fs::read_to_string(&cfg_path).unwrap_or_default();
    let count = std::fs::read_to_string(merged_catalog_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|c| c.get("models").and_then(Value::as_array).map(Vec::len))
        .unwrap_or(0);
    CodexStatus {
        codex_home: home.display().to_string(),
        config_exists: cfg_path.exists(),
        managed_block_present: raw.contains(BEGIN_MARK),
        managed_block_orphaned: raw.contains(BEGIN_MARK) && !raw.contains(END_MARK),
        native_catalog_present: native_catalog_path().exists(),
        merged_catalog_present: merged_catalog_path().exists(),
        merged_model_count: count,
        codex_cli_available: codex_bin().is_some(),
        integration_enabled: config.codex_integration,
    }
}

/// Locate the Codex CLI, cached for the process.
///
/// A macOS app launched from Finder does not inherit the shell's PATH — it
/// gets launchd's, which is `/usr/bin:/bin:/usr/sbin:/sbin` and contains no
/// package manager's bin directory. Codex installs into `~/.local/bin`,
/// `/opt/homebrew/bin`, `~/.bun/bin` and friends, so probing PATH alone
/// finds it when the app is started from a terminal and never finds it when
/// the app is double-clicked. That looked like "the integration just does
/// not work on this Mac": no CLI means no native catalog, which means no
/// merged catalog, so three of the four status rows stay red at once.
///
/// The lookup is cached because `status()` runs on every screen open and the
/// login-shell probe spawns a shell.
fn codex_bin() -> Option<String> {
    static RESOLVED: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    RESOLVED.get_or_init(resolve_codex_bin).clone()
}

fn resolve_codex_bin() -> Option<String> {
    // An explicit override always wins, and is the documented escape hatch
    // when the heuristics below miss.
    if let Ok(bin) = std::env::var("CODEX_BIN") {
        if !bin.is_empty() {
            return Some(bin);
        }
    }

    // 1. Whatever PATH this process has. Correct when launched from a shell.
    let candidate = if cfg!(windows) { "codex.cmd" } else { "codex" };
    for name in [candidate, "codex"] {
        if runs(name) {
            return Some(name.to_string());
        }
    }

    // 2. Ask the user's login shell where it is. This is the only way to
    //    honour a PATH they set in their own rc files, and it is what other
    //    GUI developer tools do for exactly this reason.
    #[cfg(unix)]
    if let Some(found) = login_shell_lookup() {
        return Some(found);
    }

    // 3. Well-known install locations, for the case where the login shell is
    //    unavailable or non-interactive.
    #[cfg(unix)]
    {
        if let Some(home) = dirs::home_dir() {
            let candidates = [
                home.join(".local/bin/codex"),
                home.join(".bun/bin/codex"),
                home.join(".volta/bin/codex"),
                home.join(".npm-global/bin/codex"),
                home.join(".yarn/bin/codex"),
                std::path::PathBuf::from("/opt/homebrew/bin/codex"),
                std::path::PathBuf::from("/usr/local/bin/codex"),
                std::path::PathBuf::from("/opt/local/bin/codex"),
            ];
            for path in candidates {
                if path.is_file() && runs(&path.display().to_string()) {
                    return Some(path.display().to_string());
                }
            }
        }
    }

    bundled_desktop_cli()
}

/// Whether this command answers `--version` successfully.
fn runs(bin: &str) -> bool {
    let mut command = std::process::Command::new(bin);
    hide_console_window(&mut command);
    command
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Resolve `codex` through the user's shell, so the PATH they configured is
/// the one that decides.
///
/// `-lic`, not `-lc`: zsh is the macOS default and only sources `.zshrc` for
/// *interactive* shells, while `.zprofile`/`.zlogin` handle login ones. Most
/// people put their PATH in `.zshrc`, so a login-only probe returns nothing
/// on the exact setup this is meant to rescue — measured on a machine whose
/// PATH lives in `.zshrc`: `-lc` found nothing, `-lic` found the CLI.
///
/// An interactive shell can print a banner or a prompt, so the last non-empty
/// line is taken and then verified by actually running it. It can also hang
/// on a bad rc file, and this runs inside a status call the UI waits on —
/// hence the deadline.
#[cfg(unix)]
fn login_shell_lookup() -> Option<String> {
    use std::io::Read;

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let mut child = std::process::Command::new(&shell)
        .args(["-lic", "command -v codex"])
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
    if !runs(&path) {
        return None;
    }
    tracing::info!(%path, "resolved the Codex CLI through the shell");
    Some(path)
}

/// The Codex desktop app ships a full CLI under
/// `%LOCALAPPDATA%\OpenAI\Codex\bin\<hash>\codex.exe` (Windows).
/// Use the most recently modified one when no PATH install exists.
#[cfg(windows)]
fn bundled_desktop_cli() -> Option<String> {
    let bin_root = dirs::data_local_dir()?
        .join("OpenAI")
        .join("Codex")
        .join("bin");
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(bin_root).ok()?.flatten() {
        let exe = entry.path().join("codex.exe");
        if !exe.exists() {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if newest.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
            newest = Some((mtime, exe));
        }
    }
    newest.map(|(_, p)| p.to_string_lossy().to_string())
}

#[cfg(not(windows))]
fn bundled_desktop_cli() -> Option<String> {
    None
}

/// Capture the native catalog from the Codex CLI (`codex debug models`,
/// falling back to `--bundled`). Returns the parsed `{models: [...]}`.
/// `exclude_slugs` lists additional slugs to drop (besides the built-in
/// `provider/model` filter): in native slug mode our republished bare slugs
/// echo back through `debug models` and must not pollute the next capture.
pub fn capture_native_catalog(
    exclude_slugs: &std::collections::HashSet<String>,
) -> anyhow::Result<Value> {
    let bin = codex_bin().ok_or_else(|| {
        anyhow::anyhow!("Codex CLI not found on PATH (set CODEX_BIN to its location)")
    })?;
    let run = |extra: &str| -> anyhow::Result<String> {
        let mut command = std::process::Command::new(&bin);
        hide_console_window(&mut command);
        let out = command
            .args(["debug", "models"])
            .args(if extra.is_empty() {
                vec![]
            } else {
                vec![extra]
            })
            .output()?;
        if !out.status.success() {
            anyhow::bail!(
                "codex debug models failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(String::from_utf8(out.stdout)?)
    };
    let raw = run("").or_else(|_| run("--bundled"))?;
    let parsed: Value = serde_json::from_str(&raw)?;
    let models: Vec<Value> = parsed
        .get("models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // When the managed block is active, `debug models` echoes our own merged
    // catalog back. Routed slugs always look like `provider/model`; native
    // OpenAI slugs never contain '/'. Drop them so stale routed entries can
    // never pile up as duplicates in the next merge. Native slug mode
    // publishes bare slugs instead, so the caller passes those explicitly.
    let models: Vec<Value> = models
        .into_iter()
        .filter(|m| {
            m.get("slug")
                .and_then(Value::as_str)
                .map(|s| !s.contains('/') && !exclude_slugs.contains(s))
                .unwrap_or(true)
        })
        .collect();
    if models.is_empty() {
        anyhow::bail!("Codex returned an empty or invalid model catalog");
    }
    let mut catalog = json!({ "models": models });
    ensure_native_catalog_backfills(&mut catalog);
    std::fs::create_dir_all(loom_dir())?;
    std::fs::write(
        native_catalog_path(),
        serde_json::to_string_pretty(&catalog)?,
    )?;
    Ok(catalog)
}

#[cfg(windows)]
fn hide_console_window(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;

    // The Desktop CLI has no UI; hiding its inherited console avoids a flash.
    command.creation_flags(0x0800_0000);
}

#[cfg(not(windows))]
fn hide_console_window(_: &mut std::process::Command) {}

fn load_native_catalog() -> Value {
    let mut catalog = std::fs::read_to_string(native_catalog_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({ "models": [] }));
    ensure_native_catalog_backfills(&mut catalog);
    catalog
}

/// Keep a release-known native entry available when an older or sandboxed
/// Codex CLI omits it from `debug models`. Clone Terra's real schema instead
/// of inventing one, so the picker gets the same contract Codex expects.
fn ensure_native_catalog_backfills(catalog: &mut Value) {
    let Some(models) = catalog.get_mut("models").and_then(Value::as_array_mut) else {
        return;
    };
    if models
        .iter()
        .any(|model| model.get("slug").and_then(Value::as_str) == Some("gpt-5.6-sol"))
    {
        return;
    }
    let Some(mut sol) = models
        .iter()
        .find(|model| model.get("slug").and_then(Value::as_str) == Some("gpt-5.6-terra"))
        .cloned()
    else {
        return;
    };
    sol["slug"] = json!("gpt-5.6-sol");
    sol["display_name"] = json!("GPT-5.6-Sol");
    sol["priority"] = json!(4);
    models.push(sol);
}

/// Conservative fallback context window (tokens) for providers without an
/// explicit `context_window` override. Under-estimating is safe — the agent
/// just compacts earlier — while over-estimating makes Codex plan turns
/// against a window the model does not have.
const DEFAULT_CONTEXT_WINDOW: i64 = 131_072;

/// The context window LoomRouter publishes for one model, and where the
/// number came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct ContextWindow {
    /// Tokens.
    pub window: i64,
    /// False when `window` is only the conservative fallback, i.e. nothing
    /// is actually known about this model. The UI must not present a guess
    /// as if it were the model's published limit.
    pub known: bool,
}

/// Context window (tokens) for a model, and whether it is a real value.
///
/// Single source of truth. This is the number written into Codex's catalog,
/// so anything that displays a limit has to read it from here — a second
/// copy of the heuristic would drift and show the user a window Codex was
/// never told about.
///
/// Precedence: a per-model value learned during discovery (or hand-set in
/// the config) wins over everything; then the Kimi name heuristic (K3 = 1M
/// tokens; 256k-class = 256k), which applies only to Kimi-family providers:
/// applying it to e.g. claude-sonnet-5 or grok-4.5 would publish a window
/// those models do not have. Everything else uses the provider's explicit
/// override when configured, and otherwise falls back — under-estimating is
/// safe, since the agent just compacts earlier, while over-estimating makes
/// Codex plan turns against a window it does not have.
pub fn context_window_for(provider: &crate::config::Provider, model_id: &str) -> ContextWindow {
    if let Some(w) = provider
        .models
        .iter()
        .find(|m| m.id == model_id)
        .and_then(|m| m.context_window)
    {
        return ContextWindow {
            window: i64::from(w),
            known: true,
        };
    }
    match crate::proxy::family_of(provider) {
        crate::proxy::ProviderFamily::Kimi => {
            let window = if model_id.contains("256k") {
                262_144
            } else if model_id.contains("k3") {
                1_000_000
            } else {
                262_144
            };
            ContextWindow {
                window,
                known: true,
            }
        }
        _ => match provider.context_window {
            Some(w) => ContextWindow {
                window: i64::from(w),
                known: true,
            },
            None => ContextWindow {
                window: DEFAULT_CONTEXT_WINDOW,
                known: false,
            },
        },
    }
}

/// Field overrides applied to a cloned native template for each external
/// model. Mirrors the schema Codex emits for its own models.
///
/// `native_slug_mode` selects the published slug: routed mode uses
/// `provider/model` (unambiguous next to native GPT models); native slug
/// mode uses the bare model id so entries look and resolve like native
/// ones (see module docs).
fn routed_model(
    template: &Value,
    provider: &crate::config::Provider,
    model_id: &str,
    label: Option<&str>,
    priority: i64,
    native_slug_mode: bool,
    supports_image_input: bool,
) -> Value {
    let mut m: Map<String, Value> = template.as_object().cloned().unwrap_or_default();
    let slug = if native_slug_mode {
        model_id.to_string()
    } else {
        format!("{}/{}", provider.id, model_id)
    };
    m.insert("slug".into(), json!(slug));
    // The cloned template's system prompt says "based on GPT-5", which
    // makes external models introduce themselves as GPT-5. Rewrite the
    // identity line to be model-neutral.
    if let Some(instructions) = m
        .get("base_instructions")
        .and_then(Value::as_str)
        .map(str::to_string)
    {
        let patched = instructions.replace("an agent based on GPT-5", "a coding agent");
        m.insert("base_instructions".into(), json!(patched));
    }
    m.insert(
        "display_name".into(),
        json!(label.unwrap_or(model_id).to_string()),
    );
    m.insert(
        "description".into(),
        json!(format!(
            "{} via LoomRouter ({}){}",
            model_id,
            provider.id,
            if provider.id == crate::providers::CLAUDE_CODE_PROVIDER_ID
                && crate::providers::claude_code_fast_mode(model_id)
            {
                " · fast mode"
            } else {
                ""
            }
        )),
    );
    m.insert("priority".into(), json!(priority));
    m.insert("visibility".into(), json!("list"));
    m.insert("supported_in_api".into(), json!(true));
    // Reasoning levels shown in Codex's effort picker. Codex only renders
    // the canonical set (low/medium/high/xhigh); other values are hidden.
    m.insert(
        "supported_reasoning_levels".into(),
        json!([
            {"effort": "low", "description": "Fast responses with lighter reasoning"},
            {"effort": "medium", "description": "Balances speed and reasoning depth for everyday tasks"},
            {"effort": "high", "description": "Greater reasoning depth for complex problems"},
            {"effort": "xhigh", "description": "Maximum reasoning depth"}
        ]),
    );
    m.insert("default_reasoning_level".into(), json!("high"));
    let window = context_window_for(provider, model_id).window;
    m.insert("context_window".into(), json!(window));
    m.insert("max_context_window".into(), json!(window));
    m.insert("effective_context_window_percent".into(), json!(95));
    m.insert(
        "input_modalities".into(),
        if supports_image_input {
            json!(["text", "image"])
        } else {
            json!(["text"])
        },
    );
    m.insert("additional_speed_tiers".into(), json!([]));
    m.insert("service_tiers".into(), json!([]));
    m.insert("availability_nux".into(), Value::Null);
    m.insert("upgrade".into(), Value::Null);
    m.insert("supports_reasoning_summaries".into(), json!(true));
    m.insert("default_reasoning_summary".into(), json!("auto"));
    m.insert("support_verbosity".into(), json!(false));
    m.insert("default_verbosity".into(), Value::Null);
    // Deferred tool loading. With this on, Codex stops inlining every tool
    // definition in every request and advertises only `tool_search`; the
    // model searches (BM25 runs client-side in Codex) and the discovered
    // specs arrive in a `tool_search_output` item on the next request, where
    // translate.rs activates them into the Chat tool list. Requires
    // namespace_tools, which custom providers already get.
    m.insert("supports_search_tool".into(), json!(true));
    m.insert("supports_image_detail_original".into(), json!(false));
    m.insert("use_responses_lite".into(), json!(false));
    // This field decides which multi-agent tool surface Codex builds for the
    // model, and "v1" was the wrong side of that fork.
    //
    // Codex resolves the version as
    // `multi_agent_version_override().or(model_multi_agent_version)`, so the
    // value written here is what a routed model gets unless the user sets
    // `[features] multi_agent_v2`. The two versions then register the spawn
    // tool under different names: v1 as `ToolName::namespaced(
    // MULTI_AGENT_V1_NAMESPACE, "spawn_agent")` — the "collaboration"
    // namespace — and v2 as `ToolName::plain("spawn_agent")`.
    //
    // The orchestrator skill below tells the model to call `spawn_agent`.
    // Under v1 no tool by that name exists, only `collaboration.spawn_agent`,
    // so the model reported having no such tool, tried `spawn_agent --help`
    // as a shell command, and fell back to doing the whole task itself.
    //
    // Native entries in the catalog already ship "v2" (gpt-5.6-terra), so
    // matching it here puts routed models on the same surface the skill and
    // the rest of the ecosystem assume.
    m.insert("multi_agent_version".into(), json!("v2"));
    Value::Object(m)
}

/// Build the merged catalog. Routed mode: every native model (so GPT stays
/// in the picker) plus one entry per enabled external model cloned from a
/// native template. Native slug mode: external entries only, published
/// under bare slugs — native GPT models require the ChatGPT login this mode
/// exists to avoid (see module docs).
pub fn build_merged_catalog(config: &AppConfig, native: &Value) -> Value {
    let native_slug_mode = config.native_slug_mode;
    let native_models: Vec<Value> = native
        .get("models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let template = native_models
        .iter()
        .find(|m| m.get("slug").and_then(Value::as_str) == Some("gpt-5.5"))
        .or_else(|| {
            native_models
                .iter()
                .find(|m| m.get("visibility").and_then(Value::as_str) == Some("list"))
        })
        .or_else(|| native_models.first())
        .cloned()
        .unwrap_or_else(|| json!({}));

    let mut models = if native_slug_mode {
        Vec::new()
    } else {
        native_models
    };
    // External entries start after native priorities.
    let mut priority = 100_i64;
    let bridge_supports_images = crate::visual::has_valid_configuration(config);
    for p in config.providers.values().filter(|p| p.enabled) {
        for m in p.models.iter().filter(|m| m.enabled) {
            models.push(routed_model(
                &template,
                p,
                &m.id,
                m.label.as_deref(),
                priority,
                native_slug_mode,
                m.supports_vision || bridge_supports_images,
            ));
            priority += 1;
        }
    }

    // Dedupe by slug. In routed mode, routed entries win over any stale
    // native-copy entry (reverse iteration keeps the last of each slug).
    // In native slug mode, two providers serving the same bare model id
    // collide; the first provider in config order (BTreeMap) wins.
    let mut seen = std::collections::HashSet::new();
    let mut deduped: Vec<Value> = Vec::with_capacity(models.len());
    if native_slug_mode {
        for m in models.into_iter() {
            let slug = m
                .get("slug")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if seen.insert(slug) {
                deduped.push(m);
            }
        }
    } else {
        for m in models.into_iter().rev() {
            let slug = m
                .get("slug")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if seen.insert(slug) {
                deduped.push(m);
            }
        }
        deduped.reverse();
    }
    let mut models = deduped;

    models.sort_by_key(|m| m.get("priority").and_then(Value::as_i64).unwrap_or(999));
    json!({ "models": models })
}

// ---------------------------------------------------------------------------
// Custom agents (~/.codex/agents/*.toml)
//
// Format per the official Codex subagents documentation
// (https://developers.openai.com/codex/subagents): one standalone TOML file
// per agent under `~/.codex/agents/` (personal) or `.codex/agents/`
// (project). Codex requires `name`, `description` and
// `developer_instructions`; `model` and `model_reasoning_effort` are
// optional, and any other session config key (`sandbox_mode`,
// `mcp_servers`, `skills.config`, ...) may also appear. The `name` field —
// not the filename — is the source of truth, though matching both is the
// documented convention; this module enforces that convention.
// ---------------------------------------------------------------------------

// Keep the established `codex::*` API while isolating agent-file ownership.
#[path = "codex/agents.rs"]
mod agents;
pub use agents::{
    agent_templates, agents_delete, agents_list, agents_upsert, sync_orchestrator_skill, AgentInfo,
    AgentTemplate,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Provider, ProviderModel, ProviderProtocol};

    use std::collections::BTreeMap;

    #[test]
    fn status_flags_orphaned_managed_block() {
        let _guard = codex_home_guard();
        let tmp = std::env::temp_dir().join(format!("loom-codex-status-{}", std::process::id()));
        std::env::set_var("CODEX_HOME", &tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        // Orphan: BEGIN without END (the external-rewrite signature).
        std::fs::write(
            tmp.join("config.toml"),
            "# BEGIN loom-router-managed\nmodel_provider = \"loomrouter\"\n",
        )
        .unwrap();
        let status = status(&demo_config());
        assert!(status.managed_block_present);
        assert!(status.managed_block_orphaned);
        std::env::remove_var("CODEX_HOME");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn status_does_not_flag_complete_managed_block() {
        let _guard = codex_home_guard();
        let tmp = std::env::temp_dir().join(format!("loom-codex-status-ok-{}", std::process::id()));
        std::env::set_var("CODEX_HOME", &tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(
            tmp.join("config.toml"),
            "# BEGIN loom-router-managed\nmodel_provider = \"loomrouter\"\n# END loom-router-managed\n",
        )
        .unwrap();
        let status = status(&demo_config());
        assert!(status.managed_block_present);
        assert!(!status.managed_block_orphaned);
        std::env::remove_var("CODEX_HOME");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    fn demo_config() -> AppConfig {
        let mut providers = BTreeMap::new();
        providers.insert(
            "deepseek".into(),
            Provider {
                id: "deepseek".into(),
                name: "DeepSeek".into(),
                protocol: ProviderProtocol::OpenAI,
                base_url: "https://api.deepseek.com/v1".into(),
                api_key: None,
                has_key: false,
                context_window: None,
                user_agent: None,
                models: vec![ProviderModel {
                    id: "deepseek-chat".into(),
                    label: Some("DeepSeek Chat".into()),
                    context_window: None,
                    protocol: None,
                    fast_mode: false,
                    enabled: true,
                    supports_vision: false,
                }],
                enabled: true,
            },
        );
        AppConfig {
            port: 4180,
            providers,
            ..AppConfig::default()
        }
    }

    #[test]
    fn merged_catalog_keeps_native_and_adds_external() {
        let native = json!({"models": [
            {"slug": "gpt-5.5", "display_name": "GPT-5.5", "priority": 1, "visibility": "list",
             "base_instructions": "You are Codex.", "supported_reasoning_levels": ["low","high"]}
        ]});
        let merged = build_merged_catalog(&demo_config(), &native);
        let models = merged["models"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        // Native entry preserved untouched.
        assert_eq!(models[0]["slug"], "gpt-5.5");
        assert_eq!(
            models[0]["supported_reasoning_levels"],
            json!(["low", "high"])
        );
        // External entry cloned from the template with overrides.
        let ext = &models[1];
        assert_eq!(ext["slug"], "deepseek/deepseek-chat");
        assert_eq!(ext["display_name"], "DeepSeek Chat");
        assert_eq!(ext["visibility"], "list");
        assert_eq!(ext["supported_in_api"], true);
        assert_eq!(ext["base_instructions"], "You are Codex.");
        // DeepSeek is not a Kimi-family provider, so the Kimi name
        // heuristic must NOT apply; without an explicit override the
        // conservative default is published.
        assert_eq!(ext["context_window"], DEFAULT_CONTEXT_WINDOW);
    }

    #[test]
    fn native_catalog_backfills_sol_from_terra_when_the_cli_omits_it() {
        let mut native = json!({"models": [
            {"slug": "gpt-5.6-terra", "display_name": "GPT-5.6-Terra", "priority": 2,
             "visibility": "list", "supported_in_api": true}
        ]});

        ensure_native_catalog_backfills(&mut native);

        let sol = native["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["slug"] == "gpt-5.6-sol")
            .unwrap();
        assert_eq!(sol["display_name"], "GPT-5.6-Sol");
        assert_eq!(sol["priority"], 4);
        assert_eq!(sol["visibility"], "list");
    }

    #[test]
    fn kimi_heuristic_applies_only_to_kimi_family() {
        let mut cfg = demo_config();
        let kimi = crate::providers::PRESETS
            .iter()
            .find(|p| p.id == "kimi-coding")
            .unwrap();
        cfg.providers.insert(
            "kimi-coding".into(),
            crate::config::Provider::from_preset(kimi),
        );
        let merged = build_merged_catalog(&cfg, &json!({"models": []}));
        let models = merged["models"].as_array().unwrap();
        let k3 = models
            .iter()
            .find(|m| m["slug"].as_str() == Some("kimi-coding/k3"))
            .unwrap();
        assert_eq!(k3["context_window"], 1_000_000);
        let k3_256k = models
            .iter()
            .find(|m| m["slug"].as_str() == Some("kimi-coding/k3-256k"))
            .unwrap();
        assert_eq!(k3_256k["context_window"], 262_144);
        // The non-Kimi provider in the same catalog keeps the default.
        let ds = models
            .iter()
            .find(|m| m["slug"].as_str() == Some("deepseek/deepseek-chat"))
            .unwrap();
        assert_eq!(ds["context_window"], DEFAULT_CONTEXT_WINDOW);
    }

    #[test]
    fn context_window_marks_guesses_as_unknown() {
        // The UI shows this value as the model's limit, so a fallback must
        // be distinguishable from a real number — otherwise every provider
        // without an override appears to be a 128k model.
        let kimi = crate::providers::PRESETS
            .iter()
            .find(|p| p.id == "kimi-coding")
            .unwrap();
        let kimi = crate::config::Provider::from_preset(kimi);
        assert_eq!(
            context_window_for(&kimi, "k3"),
            ContextWindow {
                window: 1_000_000,
                known: true
            }
        );
        assert_eq!(
            context_window_for(&kimi, "k3-256k"),
            ContextWindow {
                window: 262_144,
                known: true
            }
        );

        let ds = crate::providers::PRESETS
            .iter()
            .find(|p| p.id == "deepseek")
            .unwrap();
        let mut ds = crate::config::Provider::from_preset(ds);
        assert_eq!(
            context_window_for(&ds, "deepseek-chat"),
            ContextWindow {
                window: DEFAULT_CONTEXT_WINDOW,
                known: false
            },
            "an unconfigured provider must report its window as a guess"
        );

        // An explicit override is a real value, not a guess.
        ds.context_window = Some(64_000);
        assert_eq!(
            context_window_for(&ds, "deepseek-chat"),
            ContextWindow {
                window: 64_000,
                known: true
            }
        );
    }

    #[test]
    fn model_level_context_window_wins_over_provider_and_heuristic() {
        // Discovery fills ProviderModel.context_window from the provider's
        // catalog or models.dev; that real per-model number must beat both
        // the provider-wide override and the Kimi family heuristic.
        let kimi = crate::providers::PRESETS
            .iter()
            .find(|p| p.id == "kimi-coding")
            .unwrap();
        let mut kimi = crate::config::Provider::from_preset(kimi);
        // Heuristic would say 1_000_000 for k3; a discovered 262_144 wins.
        kimi.models
            .iter_mut()
            .find(|m| m.id == "k3")
            .unwrap()
            .context_window = Some(262_144);
        assert_eq!(
            context_window_for(&kimi, "k3"),
            ContextWindow {
                window: 262_144,
                known: true
            }
        );

        let ds = crate::providers::PRESETS
            .iter()
            .find(|p| p.id == "deepseek")
            .unwrap();
        let mut ds = crate::config::Provider::from_preset(ds);
        ds.context_window = Some(64_000);
        // Provider-wide override would say 64_000; a discovered 1M wins.
        ds.models.push(crate::config::ProviderModel {
            id: "deepseek-chat".into(),
            label: None,
            context_window: Some(1_048_576),
            protocol: None,
            fast_mode: false,
            enabled: true,
            supports_vision: false,
        });
        assert_eq!(
            context_window_for(&ds, "deepseek-chat"),
            ContextWindow {
                window: 1_048_576,
                known: true
            }
        );
    }

    #[test]
    fn merged_catalog_works_without_native() {
        let merged = build_merged_catalog(&demo_config(), &json!({"models": []}));
        let models = merged["models"].as_array().unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["slug"], "deepseek/deepseek-chat");
    }

    #[test]
    fn catalog_advertises_native_vision_without_a_bridge() {
        let mut config = demo_config();
        config.providers.get_mut("deepseek").unwrap().models[0].supports_vision = true;

        let merged = build_merged_catalog(&config, &json!({"models": []}));
        assert_eq!(
            merged["models"][0]["input_modalities"],
            json!(["text", "image"])
        );
    }

    #[test]
    fn catalog_keeps_text_only_model_text_only_when_bridge_is_disabled() {
        let merged = build_merged_catalog(&demo_config(), &json!({"models": []}));
        assert_eq!(merged["models"][0]["input_modalities"], json!(["text"]));
    }

    #[test]
    fn catalog_advertises_images_for_text_only_model_with_valid_bridge() {
        let mut config = demo_config();
        config.providers.get_mut("deepseek").unwrap().api_key = Some("destination-key".into());
        config.providers.insert(
            "vision".into(),
            Provider {
                id: "vision".into(),
                name: "Vision".into(),
                protocol: ProviderProtocol::OpenAI,
                base_url: "https://vision.example/v1".into(),
                api_key: Some("assistant-key".into()),
                has_key: true,
                context_window: None,
                user_agent: None,
                models: vec![ProviderModel {
                    id: "vision-model".into(),
                    label: None,
                    context_window: None,
                    protocol: None,
                    enabled: true,
                    supports_vision: true,
                    fast_mode: false,
                }],
                enabled: true,
            },
        );
        config.visual_assistance = crate::config::VisualAssistanceConfig {
            enabled: true,
            assistant_model: Some("vision/vision-model".into()),
            fallback_models: vec![],
        };

        let merged = build_merged_catalog(&config, &json!({"models": []}));
        assert_eq!(
            merged["models"][0]["input_modalities"],
            json!(["text", "image"])
        );
    }

    #[test]
    fn catalog_keeps_text_only_model_text_only_when_bridge_is_invalid() {
        let mut config = demo_config();
        config.visual_assistance = crate::config::VisualAssistanceConfig {
            enabled: true,
            assistant_model: Some("deepseek/deepseek-chat".into()),
            fallback_models: vec![],
        };

        let merged = build_merged_catalog(&config, &json!({"models": []}));
        assert_eq!(merged["models"][0]["input_modalities"], json!(["text"]));
    }

    // ---------------------------------------------------------------------
    // Native slug mode (use Codex without an OpenAI login)
    // ---------------------------------------------------------------------

    #[test]
    fn native_slug_mode_publishes_bare_slugs_and_drops_natives() {
        let mut cfg = demo_config();
        cfg.native_slug_mode = true;
        let native = json!({"models": [
            {"slug": "gpt-5.5", "display_name": "GPT-5.5", "priority": 1, "visibility": "list",
             "base_instructions": "You are Codex.", "supported_reasoning_levels": ["low","high"]}
        ]});
        let merged = build_merged_catalog(&cfg, &native);
        let models = merged["models"].as_array().unwrap();
        // Native GPT entries require the login this mode avoids: dropped.
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["slug"], "deepseek-chat");
        assert_eq!(models[0]["display_name"], "DeepSeek Chat");
        // Metadata still cloned from the native template.
        assert_eq!(models[0]["visibility"], "list");
    }

    #[test]
    fn native_slug_mode_bare_slug_collision_first_provider_wins() {
        let mut cfg = demo_config();
        cfg.native_slug_mode = true;
        // "aaa-other" sorts before "deepseek" in the BTreeMap and serves the
        // same model id, so it must win the bare slug.
        cfg.providers.insert(
            "aaa-other".into(),
            Provider {
                id: "aaa-other".into(),
                name: "Other".into(),
                protocol: ProviderProtocol::OpenAI,
                base_url: "https://example.com/v1".into(),
                api_key: None,
                has_key: false,
                context_window: None,
                user_agent: None,
                models: vec![ProviderModel {
                    id: "deepseek-chat".into(),
                    label: Some("Other Chat".into()),
                    context_window: None,
                    protocol: None,
                    fast_mode: false,
                    enabled: true,
                    supports_vision: false,
                }],
                enabled: true,
            },
        );
        let merged = build_merged_catalog(&cfg, &json!({"models": []}));
        let models = merged["models"].as_array().unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["slug"], "deepseek-chat");
        assert_eq!(models[0]["display_name"], "Other Chat");
    }
}

#[cfg(test)]
mod cli_lookup_tests {
    /// Regression: an app launched from Finder inherits launchd's PATH
    /// (`/usr/bin:/bin:/usr/sbin:/sbin`), which contains no package-manager
    /// bin directory. Probing PATH alone therefore found the CLI when the app
    /// was started from a terminal and never when it was double-clicked —
    /// and with no CLI there is no native catalog and no merged catalog, so
    /// three status rows went red together and the integration looked broken.
    ///
    /// Mutates PATH for the process, so it is deliberately the only test in
    /// this module.
    #[cfg(unix)]
    #[test]
    fn resolves_the_cli_under_the_launchd_path() {
        let had_cli = super::resolve_codex_bin().is_some();
        if !had_cli {
            eprintln!("no Codex CLI installed here; nothing to resolve");
            return;
        }

        let saved = std::env::var("PATH").ok();
        // SAFETY: single-threaded test, restored below.
        unsafe {
            std::env::set_var("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
            std::env::remove_var("CODEX_BIN");
        }
        let found = super::resolve_codex_bin();
        unsafe {
            match saved {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
        }

        assert!(
            found.is_some(),
            "the CLI must still be found without a useful PATH — this is the \
             exact state a Finder-launched app runs in"
        );
    }
}
