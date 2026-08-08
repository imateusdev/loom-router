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
use serde_json::Value;
use std::path::Path;
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
        if crate::cli_locator::find_in_path(name).is_some() && runs(Path::new(name)) {
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
                if path.is_file() && runs(&path) {
                    return Some(path.display().to_string());
                }
            }
        }
    }

    bundled_desktop_cli()
}

/// Whether this command answers `--version` successfully.
fn runs(bin: &Path) -> bool {
    crate::cli_locator::executable_runs(bin, |_| {})
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
    let path = crate::cli_locator::login_shell_lookup("command -v codex", runs)?;
    let path = path.to_string_lossy().to_string();
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

// Keep the established `codex::*` catalog API while isolating catalog ownership.
#[path = "codex/catalog.rs"]
mod catalog;
pub use catalog::{
    build_merged_catalog, capture_native_catalog, context_window_for, ContextWindow,
};
use catalog::{load_native_catalog, loom_dir, merged_catalog_path, native_catalog_path};

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
mod tests;

#[cfg(test)]
mod cli_lookup_tests;
