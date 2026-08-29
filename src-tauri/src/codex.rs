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
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[path = "codex/config_patch.rs"]
mod config_patch;
#[path = "codex/subagents.rs"]
mod subagents;
pub use config_patch::{
    active_slug, apply, current_root_model, multi_agent_enabled, owns_slug, published_slug, remove,
    set_multi_agent, BEGIN_MARK, END_MARK,
};
pub use subagents::serve_subagent_mcp;

#[derive(Debug, Clone, Serialize)]
pub struct CodexStatus {
    pub codex_home: String,
    pub config_exists: bool,
    pub config_parseable: bool,
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
    /// Whether `codex doctor` can load the current merged catalog.
    pub codex_config_loads: bool,
    /// Human-readable reason when `codex_config_loads` is false.
    pub codex_config_error: Option<String>,
    /// Whether auto-apply is on (user clicked Apply at least once).
    pub integration_enabled: bool,
    /// Presence and expiry of the local Codex session, never its token.
    pub session: CodexSessionStatus,
}

#[derive(Debug, Clone, Serialize)]
pub struct CodexSessionStatus {
    pub path: String,
    pub present: bool,
    pub usable: bool,
    pub has_account_id: bool,
    pub expired: bool,
    pub expires_in_hours: Option<f64>,
    pub age_hours: Option<f64>,
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
    let config_parseable = toml::from_str::<toml::Value>(&raw).is_ok();
    let count = std::fs::read_to_string(merged_catalog_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|c| c.get("models").and_then(Value::as_array).map(Vec::len))
        .unwrap_or(0);
    let session = codex_session_status(&home.join("auth.json"));
    let merged_catalog_present = merged_catalog_path().exists();
    // `codex doctor` is a subprocess with a 10s ceiling on every screen open.
    // There is nothing to validate until the integration has published a
    // catalog, so do not pay for it on the onboarding path.
    let (codex_config_loads, codex_config_error) =
        if merged_catalog_present && config.codex_integration {
            catalog::validate_merged_catalog()
        } else {
            (false, None)
        };
    CodexStatus {
        codex_home: home.display().to_string(),
        config_exists: cfg_path.exists(),
        config_parseable,
        managed_block_present: raw.contains(BEGIN_MARK),
        managed_block_orphaned: raw.contains(BEGIN_MARK) && !raw.contains(END_MARK),
        native_catalog_present: native_catalog_path().exists(),
        merged_catalog_present,
        merged_model_count: count,
        codex_cli_available: codex_bin().is_some(),
        codex_config_loads,
        codex_config_error,
        integration_enabled: config.codex_integration,
        session,
    }
}

pub fn codex_session_status(path: &Path) -> CodexSessionStatus {
    let present = path.is_file();
    let age_hours = if present {
        std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .map(|modified| {
                let age = SystemTime::now()
                    .duration_since(modified)
                    .map(|duration| duration.as_secs_f64() / 3600.0)
                    .unwrap_or_default();
                round_hours(age)
            })
    } else {
        None
    };
    let session = if present {
        read_codex_auth_summary(path)
    } else {
        None
    };

    let has_account_id = session
        .as_ref()
        .is_some_and(|session| session.has_account_id);
    let expires_in_hours = session.as_ref().and_then(|session| {
        session
            .expires_at_ms
            .map(|expires_at_ms| round_hours((expires_at_ms - now_ms()) as f64 / 3_600_000.0))
    });
    let expired = session
        .as_ref()
        .and_then(|session| session.expires_at_ms)
        .is_some_and(|expires_at_ms| expires_at_ms - EXPIRY_SKEW_MS <= now_ms());
    let usable = session
        .as_ref()
        .is_some_and(|session| session.access_token_present)
        && !expired;

    CodexSessionStatus {
        path: path.display().to_string(),
        present,
        usable,
        has_account_id,
        expired,
        expires_in_hours,
        age_hours,
    }
}

const EXPIRY_SKEW_MS: i64 = 120_000;

struct CodexAuthSummary {
    access_token_present: bool,
    has_account_id: bool,
    expires_at_ms: Option<i64>,
}

fn read_codex_auth_summary(path: &Path) -> Option<CodexAuthSummary> {
    let parsed: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let tokens = parsed.get("tokens")?;
    let access_token = tokens
        .get("access_token")
        .and_then(Value::as_str)
        .unwrap_or("");
    let account_id = tokens
        .get("account_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    Some(CodexAuthSummary {
        access_token_present: !access_token.is_empty(),
        has_account_id: !account_id.is_empty(),
        expires_at_ms: token_expiry_ms(access_token),
    })
}

fn token_expiry_ms(access_token: &str) -> Option<i64> {
    let payload = access_token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    let exp = claims.get("exp")?.as_i64()?;
    exp.checked_mul(1_000)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn round_hours(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

/// Locate the Codex CLI, cached for the process.
///
/// A macOS app launched from Finder does not inherit the shell's PATH - it
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
static RESOLVED: std::sync::Mutex<Option<Option<String>>> = std::sync::Mutex::new(None);

fn codex_bin() -> Option<String> {
    // `None` means "not resolved yet"; `Some(None)` means "resolved and absent".
    let mut resolved = RESOLVED.lock().unwrap_or_else(|poison| poison.into_inner());
    if resolved.is_none() {
        *resolved = Some(resolve_codex_bin());
    }
    resolved.clone().unwrap_or_default()
}

pub(crate) fn reset_codex_bin_cache() {
    *RESOLVED.lock().unwrap_or_else(|poison| poison.into_inner()) = None;
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
    for name in path_candidates() {
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
                PathBuf::from("/opt/homebrew/bin/codex"),
                PathBuf::from("/usr/local/bin/codex"),
                PathBuf::from("/opt/local/bin/codex"),
            ];
            for path in candidates {
                if path.is_file() && runs(&path) {
                    return Some(path.display().to_string());
                }
            }
        }
    }

    // 4. Standalone installer location on Windows.
    #[cfg(windows)]
    {
        // why: PATH is not refreshed for this already-running process after
        // the installer writes to %LOCALAPPDATA%\Programs\OpenAI\Codex\bin.
        if let Some(data) = dirs::data_local_dir() {
            let standalone = data
                .join("Programs")
                .join("OpenAI")
                .join("Codex")
                .join("bin")
                .join("codex.exe");
            if standalone.is_file() && runs(&standalone) {
                return Some(standalone.display().to_string());
            }
        }
    }

    bundled_desktop_cli()
}

pub fn ensure_codex_cli() -> anyhow::Result<()> {
    if codex_bin().is_some() {
        return Ok(());
    }

    install_codex_cli()?;
    reset_codex_bin_cache();

    if codex_bin().is_none() {
        anyhow::bail!("Codex CLI still not available after running the installer");
    }
    Ok(())
}

#[cfg(not(test))]
fn install_codex_cli() -> anyhow::Result<()> {
    // why: unit tests must not hit the network or mutate the host install.
    let mut command = if cfg!(windows) {
        let mut command = std::process::Command::new("powershell");
        command.args([
            "-ExecutionPolicy",
            "ByPass",
            "-c",
            "irm https://chatgpt.com/codex/install.ps1 | iex",
        ]);
        command
    } else {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "curl -fsSL https://chatgpt.com/codex/install.sh | sh"]);
        command
    };
    crate::cli_locator::hide_console_window(&mut command);
    crate::cli_locator::scrub_child_env_std(&mut command);

    let status = command.status()?;
    if !status.success() {
        anyhow::bail!("Codex installer exited unsuccessfully: {status}");
    }
    Ok(())
}

#[cfg(test)]
fn install_codex_cli() -> anyhow::Result<()> {
    anyhow::bail!("Codex installer is disabled in unit tests")
}

fn path_candidates() -> &'static [&'static str] {
    if cfg!(windows) {
        // The native Windows installer places this exact executable on PATH;
        // candidate policy stays here so other CLIs keep their own rules.
        &["codex.cmd", "codex.exe", "codex"]
    } else {
        &["codex"]
    }
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
/// on the exact setup this is meant to rescue - measured on a machine whose
/// PATH lives in `.zshrc`: `-lc` found nothing, `-lic` found the CLI.
///
/// An interactive shell can print a banner or a prompt, so the last non-empty
/// line is taken and then verified by actually running it. It can also hang
/// on a bad rc file, and this runs inside a status call the UI waits on -
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
            .and_then(|metadata| metadata.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if newest
            .as_ref()
            .map(|(time, _)| mtime > *time)
            .unwrap_or(true)
        {
            newest = Some((mtime, exe));
        }
    }
    newest.map(|(_, path)| path.to_string_lossy().to_string())
}

/// The Codex desktop app ships a CLI inside the ChatGPT app bundle on macOS.
/// Check the known macOS locations for a bundled CLI binary when no PATH
/// install exists.
#[cfg(target_os = "macos")]
fn bundled_desktop_cli() -> Option<String> {
    let app_cli = PathBuf::from("/Applications/ChatGPT.app/Contents/Resources/codex");
    if app_cli.is_file() && runs(&app_cli) {
        return Some(app_cli.display().to_string());
    }

    if let Some(data) = dirs::data_local_dir() {
        let codex_dir = data.join("Codex");
        if let Ok(entries) = std::fs::read_dir(&codex_dir) {
            for entry in entries.flatten() {
                let bin = entry.path().join("codex");
                if bin.is_file() && runs(&bin) {
                    return Some(bin.display().to_string());
                }
            }
        }
    }

    None
}

/// Desktop-app-style CLI installation on Linux: check `$XDG_DATA_HOME/codex/`
/// (typically `~/.local/share/codex/`). The well-known PATH-style locations
/// (`~/.local/bin/codex`, `/usr/local/bin/codex`) are already covered by the
/// dirs-based loop in `resolve_codex_bin`; this catches the app-bundle laydown.
#[cfg(target_os = "linux")]
fn bundled_desktop_cli() -> Option<String> {
    if let Some(data) = dirs::data_local_dir() {
        let bin = data.join("codex").join("codex");
        if bin.is_file() && runs(&bin) {
            return Some(bin.display().to_string());
        }
    }
    None
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn bundled_desktop_cli() -> Option<String> {
    None
}

// Keep the established `codex::*` catalog API while isolating catalog ownership.
#[path = "codex/catalog.rs"]
mod catalog;
pub use catalog::{
    build_merged_catalog, capture_native_catalog, context_window_for,
    invalidate_merged_catalog_validation, ContextWindow,
};
use catalog::{load_native_catalog, loom_dir, merged_catalog_path, native_catalog_path};

/// The native model slugs Codex currently serves, in catalog order. Used by
/// the UI so agents can pin a real Codex model (Terra, Sol, etc.) instead of
/// only external provider models.
///
/// Fetch fresh from the Codex CLI (`codex debug models`) instead of trusting
/// the cached catalog alone. In native slug mode our own republished bare
/// slugs echo back, so exclude every enabled external model id just like
/// `apply` does.
pub fn native_model_slugs(config: &AppConfig) -> Vec<String> {
    let exclude = if config.native_slug_mode {
        config
            .providers
            .values()
            .filter(|provider| provider.enabled)
            .flat_map(|provider| {
                provider
                    .models
                    .iter()
                    .filter(|model| model.enabled)
                    .map(|model| model.id.clone())
            })
            .collect()
    } else {
        HashSet::new()
    };
    let native = capture_native_catalog(&exclude).unwrap_or_else(|_| load_native_catalog());
    native
        .get("models")
        .and_then(Value::as_array)
        .map(|models| {
            models
                .iter()
                .filter_map(|model| {
                    model
                        .get("slug")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Custom agents (~/.codex/agents/*.toml)
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
