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

pub const BEGIN_MARK: &str = "# BEGIN loom-router-managed";
pub const END_MARK: &str = "# END loom-router-managed";

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
    std::process::Command::new(bin)
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
        let out = std::process::Command::new(&bin)
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
    let catalog = json!({ "models": models });
    std::fs::create_dir_all(loom_dir())?;
    std::fs::write(
        native_catalog_path(),
        serde_json::to_string_pretty(&catalog)?,
    )?;
    Ok(catalog)
}

fn load_native_catalog() -> Value {
    std::fs::read_to_string(native_catalog_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({ "models": [] }))
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
/// The Kimi name heuristic (K3 = 1M tokens; 256k-class = 256k) applies only
/// to Kimi-family providers: applying it to e.g. claude-sonnet-5 or grok-4.5
/// would publish a window those models do not have. Everything else uses the
/// provider's explicit override when configured, and otherwise falls back —
/// under-estimating is safe, since the agent just compacts earlier, while
/// over-estimating makes Codex plan turns against a window it does not have.
pub fn context_window_for(provider: &crate::config::Provider, model_id: &str) -> ContextWindow {
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
        json!(format!("{} via LoomRouter ({})", model_id, provider.id)),
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
    m.insert("input_modalities".into(), json!(["text", "image"]));
    m.insert("additional_speed_tiers".into(), json!([]));
    m.insert("service_tiers".into(), json!([]));
    m.insert("availability_nux".into(), Value::Null);
    m.insert("upgrade".into(), Value::Null);
    m.insert("supports_reasoning_summaries".into(), json!(true));
    m.insert("default_reasoning_summary".into(), json!("auto"));
    m.insert("support_verbosity".into(), json!(false));
    m.insert("default_verbosity".into(), Value::Null);
    m.insert("supports_search_tool".into(), json!(false));
    m.insert("supports_image_detail_original".into(), json!(false));
    m.insert("use_responses_lite".into(), json!(false));
    m.insert("multi_agent_version".into(), json!("v1"));
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
    for p in config.providers.values().filter(|p| p.enabled) {
        for m in p.models.iter().filter(|m| m.enabled) {
            models.push(routed_model(
                &template,
                p,
                &m.id,
                m.label.as_deref(),
                priority,
                native_slug_mode,
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

/// Apply the integration: refresh native catalog (best effort), write the
/// merged catalog, and install the managed config block.
pub fn apply(config: &AppConfig, port: u16) -> anyhow::Result<()> {
    std::fs::create_dir_all(loom_dir())?;

    // Refresh the native catalog when the CLI is available; otherwise reuse
    // the previous capture (or an empty set with a warning). In native slug
    // mode our own bare slugs echo back through `debug models`, so exclude
    // every enabled external model id from the capture.
    let exclude: std::collections::HashSet<String> = if config.native_slug_mode {
        config
            .providers
            .values()
            .filter(|p| p.enabled)
            .flat_map(|p| p.models.iter().filter(|m| m.enabled).map(|m| m.id.clone()))
            .collect()
    } else {
        std::collections::HashSet::new()
    };
    if codex_bin().is_some() {
        if let Err(e) = capture_native_catalog(&exclude) {
            tracing::warn!("native catalog capture failed, reusing previous: {e}");
        }
    } else {
        tracing::warn!("Codex CLI not found; merged catalog will only include external models");
    }
    let native = load_native_catalog();

    let catalog = build_merged_catalog(config, &native);
    let model_count = catalog
        .get("models")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    if model_count == 0 {
        // Codex refuses to load config.toml when `model_catalog_json` has no
        // models, which breaks the whole app (threads cannot even be
        // resumed). Never leave Codex in that state: roll back any managed
        // block we previously wrote and fail loudly instead.
        // `Some(config)`: apply() is re-entrant, so an earlier successful
        // run may well have published the root `model` key. Rolling back
        // with `None` left it pointing at a slug that no catalog contains.
        let _ = remove(Some(config));
        anyhow::bail!(
            "no models to publish: enable at least one provider model before \
             applying the Codex integration"
        );
    }
    let catalog_path = merged_catalog_path();
    std::fs::write(&catalog_path, serde_json::to_string_pretty(&catalog)?)?;

    let block = managed_block(
        port,
        &catalog_path.display().to_string().replace('\\', "/"),
        config.native_slug_mode,
    );

    let cfg_path = codex_home().join("config.toml");
    let raw = std::fs::read_to_string(&cfg_path).unwrap_or_default();
    let stripped = strip_managed_block(&raw)?;
    // Pre-marker installs left the provider block unmarked; re-applying on
    // top of one duplicates the owned root keys, so migrate it away first.
    let stripped = strip_legacy_install(&stripped);
    // The active model is a plain root key, so it has to be reconciled on
    // the *stripped* text: writing it inside the managed block would collide
    // with a `model` the user already has at the root, and a duplicated key
    // makes Codex reject the whole config.toml.
    let stripped = reconcile_root_model(&stripped, config);
    let out = insert_root_block(&stripped, &block);
    // Last line of defence: a config.toml Codex cannot parse breaks the app
    // completely (threads cannot even be resumed), so never write one — a
    // shape we failed to anticipate stops here instead of at Codex startup.
    if let Err(e) = toml::from_str::<toml::Value>(&out) {
        anyhow::bail!("refusing to write an invalid Codex config.toml: {e}");
    }
    write_config_atomic(&cfg_path, &out)?;
    Ok(())
}

/// Decide what the root `model` key should say, given what LoomRouter has
/// selected and what was there before it.
///
/// The rule is ownership: LoomRouter only ever rewrites a value it put
/// there itself. A `model` the user wrote by hand is restored, not deleted
/// — the previous version of this deleted it on the first auto-apply.
fn reconcile_root_model(stripped: &str, config: &AppConfig) -> String {
    if let Some(slug) = active_slug(config) {
        return set_root_model_key(stripped, Some(&slug));
    }
    match root_model_key(stripped) {
        // Nothing selected and the key is ours: hand it back to whatever
        // Codex used before us, or to Codex's own default.
        Some(current) if owns_slug(config, &current) => {
            set_root_model_key(stripped, config.codex_model_backup.as_deref())
        }
        // Someone else's model (or none at all): leave it alone.
        _ => stripped.to_string(),
    }
}

/// Whether a root `model` value is one LoomRouter published. Both slug
/// shapes are accepted regardless of the current mode, because
/// `native_slug_mode` may have been flipped since the key was written.
pub fn owns_slug(config: &AppConfig, value: &str) -> bool {
    config.providers.iter().any(|(provider_id, provider)| {
        provider
            .models
            .iter()
            .any(|m| value == format!("{provider_id}/{}", m.id) || value == m.id)
    })
}

/// The root `model` currently in Codex's config, ignoring anything inside
/// our managed block. Used to remember a user's own model before replacing
/// it for the first time.
pub fn current_root_model() -> Option<String> {
    let raw = std::fs::read_to_string(codex_home().join("config.toml")).ok()?;
    let stripped = strip_managed_block(&raw).ok()?;
    root_model_key(&stripped)
}

/// The slug a model is published under: `provider/model` in routed mode,
/// the bare model id in native slug mode. Single source of truth for the
/// merged catalog, the tray menu and the root `model` key.
pub fn published_slug(provider_id: &str, model_id: &str, native_slug_mode: bool) -> String {
    if native_slug_mode {
        model_id.to_string()
    } else {
        format!("{provider_id}/{model_id}")
    }
}

/// Resolve `config.active_model` ("provider/model") to the slug that is
/// actually in the published catalog. Returns `None` when nothing is
/// selected or the selection no longer exists / is disabled — a stale
/// pointer must not be written to Codex, which would show a model it
/// cannot route.
pub fn active_slug(config: &AppConfig) -> Option<String> {
    let active = config.active_model.as_deref()?;
    let (provider_id, model_id) = active.split_once('/')?;
    let provider = config.providers.get(provider_id)?;
    if !provider.enabled {
        return None;
    }
    if !provider
        .models
        .iter()
        .any(|m| m.enabled && m.id == model_id)
    {
        return None;
    }
    Some(published_slug(
        provider_id,
        model_id,
        config.native_slug_mode,
    ))
}

/// Set (or clear) the root `model` key of a `config.toml` whose managed
/// block has already been stripped.
///
/// Textual patch rather than a TOML round-trip, for the same reason
/// `set_multi_agent_in` uses one: re-serializing would drop the user's
/// comments and our own BEGIN/END markers. Root keys must precede the first
/// `[table]`, so the search stops there.
/// Does this line assign the root `model` key?
///
/// `model_provider`, `model_catalog_json` and `model_reasoning_effort` all
/// share the prefix, so the match is on the key boundary.
fn is_root_model_line(line: &str) -> bool {
    is_root_assignment(line, "model")
}

/// Does this line assign the given key? TOML also allows the key to be
/// quoted (`"key" = …`, `'key' = …`), and missing that form would leave the
/// old assignment in place next to a new one — a duplicate key, which makes
/// Codex reject the whole file.
fn is_root_assignment(line: &str, key: &str) -> bool {
    let t = line.trim_start();
    for form in [key.to_string(), format!("\"{key}\""), format!("'{key}'")] {
        if let Some(rest) = t.strip_prefix(&form) {
            if rest.trim_start().starts_with('=') {
                return true;
            }
        }
    }
    false
}

fn set_root_model_key(stripped: &str, slug: Option<&str>) -> String {
    let nl = detect_newline(stripped);
    let mut lines: Vec<String> = stripped.lines().map(str::to_string).collect();
    let first_table = lines
        .iter()
        .position(|l| {
            let t = l.trim_start();
            t.starts_with('[') && !t.starts_with('#')
        })
        .unwrap_or(lines.len());
    let existing = lines[..first_table]
        .iter()
        .position(|l| is_root_model_line(l));

    match (slug, existing) {
        (Some(slug), Some(idx)) => lines[idx] = format!("model = \"{}\"", escape_toml(slug)),
        (Some(slug), None) => lines.insert(0, format!("model = \"{}\"", escape_toml(slug))),
        (None, Some(idx)) => {
            lines.remove(idx);
        }
        (None, None) => {}
    }

    let mut out = lines.join(nl);
    if !out.is_empty() {
        out.push_str(nl);
    }
    out
}

/// Read the current root `model` value of a stripped `config.toml`.
fn root_model_key(stripped: &str) -> Option<String> {
    let first_table = stripped
        .lines()
        .position(|l| {
            let t = l.trim_start();
            t.starts_with('[') && !t.starts_with('#')
        })
        .unwrap_or(usize::MAX);
    stripped
        .lines()
        .take(first_table)
        .filter(|l| is_root_model_line(l))
        .find_map(|l| {
            let value = l.split_once('=')?.1;
            // Strip a trailing comment before unquoting: `model = "x" # note`
            // must read as `x`, not as `x" # note`.
            let value = value.trim();
            let quoted = value.strip_prefix('"')?;
            let (inner, _) = quoted.split_once('"')?;
            Some(inner.to_string())
        })
}

fn escape_toml(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Write the Codex `config.toml` atomically, keeping a `.bak` of the
/// previous contents.
///
/// This file carries the local proxy token inside the managed block, so it
/// is written owner-only. It used to be created with a plain `fs::write`
/// (0644 on Unix), which handed any local process the token — and with it
/// the ability to spend the stored API keys through the proxy. The write
/// now shares one implementation with the credential config; see
/// `secure_fs`.
fn write_config_atomic(path: &std::path::Path, contents: &str) -> anyhow::Result<()> {
    crate::secure_fs::write_private_with_backup(path, contents.as_bytes())?;
    Ok(())
}

/// The managed config block. We pin an explicit provider and point
/// `model_provider` at it; `supports_websockets = true` advertises the
/// Responses-over-WS transport the proxy implements (Codex v2 protocol),
/// with plain HTTP/SSE as the fallback.
///
/// `requires_openai_auth` is the login gate (Codex source:
/// `model-provider-info`). Routed mode keeps it `true` so ChatGPT login
/// stays available and native GPT models keep working through the
/// passthrough. Native slug mode sets it `false`: Codex then authenticates
/// only with the static `http_headers` below — the local proxy bearer
/// token — and never asks for an OpenAI login.
///
/// The proxy requires a local bearer token (generated at startup); Codex
/// authenticates with it through the provider's `http_headers`.
fn managed_block(port: u16, catalog_path: &str, native_slug_mode: bool) -> String {
    // The token is generated by us (hex), but escape defensively anyway so
    // the block can never become invalid TOML.
    let token = crate::proxy::local_token()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!(
        "{BEGIN_MARK}\n\
         model_provider = \"loomrouter\"\n\
         openai_base_url = \"http://127.0.0.1:{port}/v1\"\n\
         model_catalog_json = \"{catalog_path}\"\n\
         \n\
         [model_providers.loomrouter]\n\
         name = \"OpenAI\"\n\
         base_url = \"http://127.0.0.1:{port}/v1\"\n\
         wire_api = \"responses\"\n\
         requires_openai_auth = {requires_openai_auth}\n\
         supports_websockets = true\n\
         http_headers = {{ \"x-loomrouter-token\" = \"{token}\", \"Authorization\" = \"Bearer {token}\" }}\n\
         {END_MARK}",
        requires_openai_auth = !native_slug_mode,
    )
}

/// TOML root keys must appear before the first `[table]` header; appending
/// at EOF would nest them under the last table. Insert the managed block
/// right before the first table header, or at the end if there are none.
/// Preserves the file's original line endings (CRLF on Windows).
fn insert_root_block(raw: &str, block: &str) -> String {
    let nl = detect_newline(raw);
    let block = if nl == "\r\n" {
        block.replace('\n', "\r\n")
    } else {
        block.to_string()
    };
    let first_table = raw
        .lines()
        .enumerate()
        .find(|(_, l)| {
            let t = l.trim_start();
            t.starts_with('[') && !t.starts_with("#")
        })
        .map(|(i, _)| i);

    let mut out = String::new();
    match first_table {
        Some(idx) => {
            let mut lines: Vec<&str> = raw.lines().collect();
            // Trim trailing blank lines of the root section.
            let mut insert_at = idx;
            while insert_at > 0 && lines[insert_at - 1].trim().is_empty() {
                insert_at -= 1;
            }
            lines.insert(insert_at, &block);
            out.push_str(&lines.join(nl));
            out.push_str(nl);
        }
        None => {
            out.push_str(raw.trim_end());
            if !out.is_empty() {
                out.push_str(nl);
                out.push_str(nl);
            }
            out.push_str(&block);
            out.push_str(nl);
        }
    }
    out
}

/// Remove the integration: delete managed block and merged catalog.
///
/// `config` is what lets this tell our own root `model` from the user's:
/// a slug we published is replaced by whatever Codex used before us
/// (`codex_model_backup`), and anything else is left untouched. Passing
/// `None` skips the key entirely — used by the rollback inside `apply`,
/// where nothing of ours has been published yet.
pub fn remove(config: Option<&AppConfig>) -> anyhow::Result<()> {
    let cfg_path = codex_home().join("config.toml");
    if let Ok(raw) = std::fs::read_to_string(&cfg_path) {
        let stripped = strip_managed_block(&raw)?;
        // A legacy unmarked install is ours too: it leaves with us.
        let stripped = strip_legacy_install(&stripped);
        let restored = match (config, root_model_key(&stripped)) {
            (Some(cfg), Some(current)) if owns_slug(cfg, &current) => {
                set_root_model_key(&stripped, cfg.codex_model_backup.as_deref())
            }
            _ => stripped,
        };
        write_config_atomic(&cfg_path, &restored)?;
    }
    for path in [merged_catalog_path()] {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
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

/// One custom Codex agent as managed by the LoomRouter UI.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentInfo {
    pub name: String,
    /// What Codex reads to decide when this agent fits a task. Empty on
    /// save means "derive from the first instruction line" (legacy
    /// behavior); on read it is always populated.
    #[serde(default)]
    pub description: String,
    /// Slug "provider/model" routed by LoomRouter, or None = Codex default.
    pub model: Option<String>,
    /// e.g. "low" | "medium" | "high", None = Codex default.
    pub effort: Option<String>,
    /// "read-only" | "workspace-write", None = inherit the session policy.
    #[serde(default)]
    pub sandbox_mode: Option<String>,
    /// System instructions of the agent (`developer_instructions`).
    pub instructions: String,
}

fn agents_dir() -> PathBuf {
    codex_home().join("agents")
}

/// Safe file/name slug: no path separators, no traversal, no leading-dot
/// tricks. Codex's own examples use `snake_case` names.
fn validate_agent_name(name: &str) -> anyhow::Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !ok {
        anyhow::bail!(
            "invalid agent name '{name}': use 1-64 characters of [A-Za-z0-9_-] \
             (no path separators or dots)"
        );
    }
    Ok(())
}

fn agent_file(dir: &std::path::Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.toml"))
}

/// Codex requires a `description`. When the user (or this UI) never wrote
/// one, derive a stable fallback from the first instruction line.
fn derived_description(instructions: &str) -> String {
    let first = instructions
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("Custom agent managed by LoomRouter");
    first.chars().take(120).collect()
}

fn agent_from_table(table: &toml::map::Map<String, toml::Value>, fallback_name: &str) -> AgentInfo {
    let get_str = |key: &str| table.get(key).and_then(toml::Value::as_str);
    let instructions = get_str("developer_instructions")
        .unwrap_or_default()
        .to_string();
    AgentInfo {
        name: get_str("name").unwrap_or(fallback_name).to_string(),
        description: get_str("description")
            .map(str::to_string)
            .unwrap_or_else(|| derived_description(&instructions)),
        model: get_str("model").map(str::to_string),
        effort: get_str("model_reasoning_effort").map(str::to_string),
        sandbox_mode: get_str("sandbox_mode").map(str::to_string),
        instructions,
    }
}

/// List implementation against an explicit directory, so tests never touch
/// the real `~/.codex` (and avoid CODEX_HOME env races between tests).
fn agents_list_in(dir: &std::path::Path) -> anyhow::Result<Vec<AgentInfo>> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let fallback = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| toml::from_str::<toml::Value>(&raw).ok());
        match parsed.and_then(|v| v.as_table().cloned()) {
            Some(table) => out.push(agent_from_table(&table, &fallback)),
            // Unreadable/invalid files are skipped, never fatal for the list.
            None => tracing::warn!("skipping invalid agent file {}", path.display()),
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// List every custom agent in `~/.codex/agents/`.
pub fn agents_list() -> anyhow::Result<Vec<AgentInfo>> {
    agents_list_in(&agents_dir())
}

fn agents_upsert_in(dir: &std::path::Path, agent: &AgentInfo) -> anyhow::Result<()> {
    validate_agent_name(&agent.name)?;
    // Sandbox mode is a Codex enum; reject typos instead of writing a
    // config Codex would fail to load.
    if let Some(mode) = agent.sandbox_mode.as_deref() {
        if !matches!(mode, "read-only" | "workspace-write") {
            anyhow::bail!(
                "invalid sandbox_mode '{mode}': expected \"read-only\" or \"workspace-write\""
            );
        }
    }
    std::fs::create_dir_all(dir)?;
    let path = agent_file(dir, &agent.name);
    // Round-trip preservation: load the existing file and patch only the
    // fields AgentInfo models, keeping anything else the user or the Codex
    // CLI wrote (sandbox extras, mcp_servers, skills.config...).
    let mut table: toml::map::Map<String, toml::Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| toml::from_str::<toml::Value>(&raw).ok())
        .and_then(|v| v.as_table().cloned())
        .unwrap_or_default();
    table.insert("name".into(), toml::Value::String(agent.name.clone()));
    // Codex requires `description`. An explicit one always wins; when the
    // UI sends an empty one, keep the existing text or derive it from the
    // first instruction line (legacy behavior).
    let description = if agent.description.trim().is_empty() {
        table
            .get("description")
            .and_then(toml::Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| derived_description(&agent.instructions))
    } else {
        agent.description.clone()
    };
    table.insert("description".into(), toml::Value::String(description));
    table.insert(
        "developer_instructions".into(),
        toml::Value::String(agent.instructions.clone()),
    );
    // Modeled optional fields follow AgentInfo exactly: None means "Codex
    // default", so the key is removed rather than left stale.
    match &agent.model {
        Some(model) => {
            table.insert("model".into(), toml::Value::String(model.clone()));
        }
        None => {
            table.remove("model");
        }
    }
    match &agent.effort {
        Some(effort) => {
            table.insert(
                "model_reasoning_effort".into(),
                toml::Value::String(effort.clone()),
            );
        }
        None => {
            table.remove("model_reasoning_effort");
        }
    }
    match &agent.sandbox_mode {
        Some(mode) => {
            table.insert("sandbox_mode".into(), toml::Value::String(mode.clone()));
        }
        None => {
            table.remove("sandbox_mode");
        }
    }
    let rendered = toml::to_string_pretty(&toml::Value::Table(table))?;
    // Same atomicity discipline as the config.toml writer (tmp + rename).
    write_config_atomic(&path, &rendered)?;
    // The orchestrator skill embeds the agent roster; keep it in sync.
    // The agents dir is always <codex home>/agents, so its parent is the
    // home — tests pass a temp dir and never touch the real ~/.codex.
    if let Some(home) = dir.parent() {
        if let Err(e) = sync_orchestrator_skill_in(home) {
            tracing::warn!("orchestrator skill sync failed: {e}");
        }
    }
    Ok(())
}

/// Create or update one custom agent (creates `~/.codex/agents/` if needed).
pub fn agents_upsert(agent: &AgentInfo) -> anyhow::Result<()> {
    agents_upsert_in(&agents_dir(), agent)
}

fn agents_delete_in(dir: &std::path::Path, name: &str) -> anyhow::Result<()> {
    validate_agent_name(name)?;
    // Idempotent: deleting an absent agent is a no-op.
    match std::fs::remove_file(agent_file(dir, name)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    // The orchestrator skill embeds the agent roster; keep it in sync
    // (home derived from the agents dir, so tests stay in temp dirs).
    if let Some(home) = dir.parent() {
        if let Err(e) = sync_orchestrator_skill_in(home) {
            tracing::warn!("orchestrator skill sync failed: {e}");
        }
    }
    Ok(())
}

/// Delete one custom agent by name. Idempotent.
pub fn agents_delete(name: &str) -> anyhow::Result<()> {
    agents_delete_in(&agents_dir(), name)
}

// ---------------------------------------------------------------------------
// Agent templates
//
// Curated starter agents following the community conventions (VoltAgent's
// awesome-codex-subagents, the official Codex subagents docs): reviewers and
// auditors are read-only, builders are workspace-write, and every template
// carries a delegation-ready `description`. Models are intentionally NOT
// pinned — the user picks a routed LoomRouter slug (or the Codex default)
// in the dialog.
// ---------------------------------------------------------------------------

/// A ready-made agent recipe shown in the template gallery.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentTemplate {
    /// Suggested agent name (also the TOML filename stem).
    pub id: &'static str,
    /// Short UI label.
    pub label: &'static str,
    /// One-line UI summary of what the agent is for.
    pub blurb: &'static str,
    /// The `description` field written to the TOML — the text Codex reads
    /// when deciding which agent fits a delegation request.
    pub description: &'static str,
    /// The `developer_instructions` written to the TOML.
    pub instructions: &'static str,
    /// Suggested sandbox mode; None = inherit the session policy.
    pub sandbox_mode: Option<&'static str>,
    /// Grouping for the gallery, so a catalogue this size stays scannable.
    /// One of: review, build, investigate, quality, ship, write, data, ops.
    pub category: &'static str,
}

/// A catalogue of agent roles, not a list of Codex features.
///
/// These are the delegation patterns that recur across the whole coding-agent
/// ecosystem — reviewer, planner, debugger, test writer, migration runner and
/// so on. They are transcribed here as plain role definitions so that picking
/// one writes a Codex agent into `~/.codex/agents`: the pattern is the
/// portable part, the TOML file is the Codex-specific part.
///
/// Instructions are agent-facing and stay in English regardless of UI
/// language — they are read by the model, not by the user.
pub fn agent_templates() -> Vec<AgentTemplate> {
    vec![
        AgentTemplate {
            id: "reviewer",
            label: "Reviewer",
            category: "review",
            blurb: "Read-only code review: correctness, regressions, missing tests.",
            description: "Use for read-only code review focused on correctness, regressions, edge cases, and missing tests.",
            instructions: "You are a code reviewer. Stay read-only.\n\nReview the changes you are given like an owner: prioritize correctness bugs, regressions, unhandled edge cases, and missing test coverage. Report findings ordered by severity with file and line references. Do not edit files; end with a short verdict (approve / changes needed).",
            sandbox_mode: Some("read-only"),
        },
        AgentTemplate {
            id: "security_auditor",
            label: "Security Auditor",
            category: "review",
            blurb: "Read-only security review: OWASP risks, secrets, injection.",
            description: "Use for read-only security review: OWASP risks, injection, auth flaws, data exposure, and credential handling.",
            instructions: "You are a security auditor. Stay read-only.\n\nPrioritize exploitable vulnerabilities: injection, broken auth and access control, data exposure, insecure secret handling, and risky dependencies. Lead with concrete findings ordered by severity, each with impact and remediation. Skip style-only comments.",
            sandbox_mode: Some("read-only"),
        },
        AgentTemplate {
            id: "worker",
            label: "Worker",
            category: "build",
            blurb: "Implements a well-scoped task and reports what changed.",
            description: "Use for focused implementation tasks and bug fixes with a clear scope.",
            instructions: "You are an implementation worker.\n\nExecute the task you are given and nothing more. Keep changes scoped, follow the repository's existing conventions, and run the project's own checks when available. Report back concisely: what changed, what you verified, and anything you could not validate.",
            sandbox_mode: Some("workspace-write"),
        },
        AgentTemplate {
            id: "explorer",
            label: "Explorer",
            category: "investigate",
            blurb: "Read-only codebase exploration: find and map code fast.",
            description: "Use for read-only codebase exploration: locating code, mapping call paths, and summarizing how things work.",
            instructions: "You are a codebase explorer. Stay read-only.\n\nFind what the parent asked for as fast as possible: locate the relevant files, trace the owning code paths, and summarize how the pieces fit together. Return concrete file and symbol references. Do not propose fixes unless asked.",
            sandbox_mode: Some("read-only"),
        },
        AgentTemplate {
            id: "tester",
            label: "Test Engineer",
            category: "quality",
            blurb: "Writes and extends tests following the project's setup.",
            description: "Use for writing or extending automated tests for a specific module or change.",
            instructions: "You are a test engineer.\n\nWrite tests for the code you are given, following the project's existing test framework, naming, and fixture patterns. Cover the happy path, edge cases, and error paths. Run the tests when possible and report results; when you cannot run them, state the exact command the parent should run.",
            sandbox_mode: Some("workspace-write"),
        },
        AgentTemplate {
            id: "refactorer",
            label: "Refactorer",
            category: "build",
            blurb: "Behavior-preserving refactors with a minimal diff.",
            description: "Use for behavior-preserving refactoring: simplifying, renaming, extracting, and deduplicating code.",
            instructions: "You are a refactoring specialist.\n\nImprove structure without changing behavior: simplify, extract, rename, and deduplicate. Keep the diff minimal and reviewable, do not mix in feature changes, and verify with the project's existing tests. Report what changed and why it is safe.",
            sandbox_mode: Some("workspace-write"),
        },
        AgentTemplate {
            id: "debugger",
            label: "Debugger",
            category: "investigate",
            blurb: "Investigates a failure to its root cause before fixing.",
            description: "Use for investigating bugs: reproduce, isolate the root cause, then propose the smallest fix.",
            instructions: "You are a debugging specialist.\n\nInvestigate before you fix: reproduce the failure, isolate the root cause with evidence (logs, traces, minimal repro), and only then propose the smallest change that fixes it. Never paper over symptoms. Report the root cause, the fix, and how you verified it.",
            sandbox_mode: Some("workspace-write"),
        },
        AgentTemplate {
            id: "docs_writer",
            label: "Docs Writer",
            category: "write",
            blurb: "Docs and README updates that match the actual code.",
            description: "Use for writing or updating documentation, READMEs, and API docs.",
            instructions: "You are a documentation writer.\n\nDocument what the code actually does, not what it should do. Match the project's existing docs style, keep examples runnable and accurate, and prefer short sections with concrete commands. Update stale claims you encounter along the way.",
            sandbox_mode: Some("workspace-write"),
        },
        AgentTemplate {
            id: "planner",
            label: "Planner",
            category: "build",
            blurb: "Turns a goal into an ordered plan before any code.",
            description: "Use to break a broad goal into an ordered, reviewable implementation plan before writing code.",
            instructions: "You are a planner. Stay read-only.\n\nTurn the goal into an ordered plan: what to change, in what sequence, and why that order. Name the concrete files and the risky steps, call out what you are unsure about, and stop at the plan. Do not implement anything.",
            sandbox_mode: Some("read-only"),
        },
        AgentTemplate {
            id: "researcher",
            label: "Researcher",
            category: "investigate",
            blurb: "Gathers external knowledge: APIs, libraries, prior art.",
            description: "Use to research an unfamiliar library, API, protocol, or approach before committing to it.",
            instructions: "You are a researcher. Stay read-only.\n\nAnswer the question with evidence: how the library or API actually behaves, which version introduced what, and what the trade-offs are. Prefer primary sources and cite them. Say plainly when something could not be confirmed rather than filling the gap.",
            sandbox_mode: Some("read-only"),
        },
        AgentTemplate {
            id: "red_team",
            label: "Adversarial Critic",
            category: "review",
            blurb: "Tries to refute a proposed change instead of approving it.",
            description: "Use to attack a proposed design or change: find the case where it breaks before it ships.",
            instructions: "You are an adversarial critic. Stay read-only.\n\nYour job is to refute, not to approve. Look for the input, ordering, concurrency, failure or scale case where the proposal breaks. Default to rejection when uncertain and say exactly which scenario you cannot rule out. A finding with no concrete failing case is not a finding.",
            sandbox_mode: Some("read-only"),
        },
        AgentTemplate {
            id: "a11y_auditor",
            label: "Accessibility Auditor",
            category: "review",
            blurb: "WCAG review: contrast, keyboard, focus, semantics.",
            description: "Use for accessibility review: contrast, keyboard navigation, focus order, ARIA and semantic markup.",
            instructions: "You are an accessibility auditor. Stay read-only.\n\nCheck against WCAG AA: colour contrast, keyboard reachability and focus order, semantic structure and landmarks, form labelling, and reduced-motion handling. Report each issue with the element, the rule it breaks, and the fix.",
            sandbox_mode: Some("read-only"),
        },
        AgentTemplate {
            id: "perf_profiler",
            label: "Performance Profiler",
            category: "quality",
            blurb: "Finds the actual hot path before optimizing anything.",
            description: "Use to diagnose a performance problem: measure first, then fix the path that dominates.",
            instructions: "You are a performance engineer.\n\nMeasure before you change anything: find the path that actually dominates, with numbers. Optimize that one, then measure again and report the before and after. Reject changes whose gain you cannot demonstrate; a plausible optimization is not an optimization.",
            sandbox_mode: Some("workspace-write"),
        },
        AgentTemplate {
            id: "migrator",
            label: "Migration Runner",
            category: "build",
            blurb: "Repetitive, mechanical changes across many files.",
            description: "Use for framework, API, or version migrations applied consistently across many files.",
            instructions: "You are a migration specialist.\n\nApply the same mechanical change across every site that needs it. Find all of them first and say how many there are, keep each edit identical in shape, and never mix an unrelated improvement into the sweep. Verify with the project's own checks and report any site you deliberately skipped.",
            sandbox_mode: Some("workspace-write"),
        },
        AgentTemplate {
            id: "api_designer",
            label: "API Designer",
            category: "build",
            blurb: "Designs endpoints, schemas and contracts before code.",
            description: "Use to design an API surface: endpoints, payloads, error shapes, and versioning.",
            instructions: "You are an API designer.\n\nDesign the contract before the implementation: resources, payload shapes, status and error semantics, pagination, and how it will version. Follow the conventions already in this codebase. Show the surface as a concrete schema or signature, and name what it deliberately does not support.",
            sandbox_mode: Some("read-only"),
        },
        AgentTemplate {
            id: "dep_upgrader",
            label: "Dependency Upgrader",
            category: "ops",
            blurb: "Bumps dependencies and repairs what the bump breaks.",
            description: "Use to upgrade dependencies and fix the breakage the upgrade causes.",
            instructions: "You are a dependency upgrader.\n\nUpgrade what was asked, then read the changelog for the versions you crossed and fix the breakage it names. Keep the dependency bump and the repairs it forces in one coherent change, run the project's checks, and report any breaking change you could not resolve.",
            sandbox_mode: Some("workspace-write"),
        },
        AgentTemplate {
            id: "triager",
            label: "Issue Triager",
            category: "ops",
            blurb: "Reproduces, classifies and routes an incoming report.",
            description: "Use to triage a bug report: reproduce it, judge severity, and identify the owning code.",
            instructions: "You are an issue triager. Stay read-only.\n\nDecide three things and say them plainly: does it reproduce, how bad is it, and which code owns it. Ask for the missing detail when the report is not actionable instead of guessing. Do not fix anything.",
            sandbox_mode: Some("read-only"),
        },
        AgentTemplate {
            id: "incident_responder",
            label: "Incident Responder",
            category: "ops",
            blurb: "Works a live failure from symptom to mitigation.",
            description: "Use during an incident: read the signals, form a hypothesis, and propose the fastest safe mitigation.",
            instructions: "You are an incident responder. Stay read-only.\n\nMitigation first, root cause second. Read the logs, metrics and recent changes, state your leading hypothesis with the evidence for it, and propose the fastest safe mitigation and how to verify it worked. Flag anything that needs a human decision rather than deciding it.",
            sandbox_mode: Some("read-only"),
        },
        AgentTemplate {
            id: "pr_describer",
            label: "PR Describer",
            category: "ship",
            blurb: "Writes the pull request body from the actual diff.",
            description: "Use to write a pull request description from the changes on the branch.",
            instructions: "You are writing a pull request description. Stay read-only.\n\nDescribe what the diff actually does and why, not what the branch name suggests. Lead with the problem being solved, then the approach, then anything a reviewer should look at closely. Note what is deliberately out of scope. Keep it short enough to be read.",
            sandbox_mode: Some("read-only"),
        },
        AgentTemplate {
            id: "release_notes",
            label: "Release Notes Writer",
            category: "ship",
            blurb: "Turns commits into notes a user can act on.",
            description: "Use to turn a range of commits into user-facing release notes or a changelog entry.",
            instructions: "You are writing release notes. Stay read-only.\n\nWrite for the person who installs the build, not for the person who wrote the commits. Lead with what changed for them, group by impact, and call out breaking changes and required migration steps first. Drop internal churn that changes nothing for a user.",
            sandbox_mode: Some("read-only"),
        },
        AgentTemplate {
            id: "spec_writer",
            label: "Spec Writer",
            category: "write",
            blurb: "Turns a vague request into a written, testable spec.",
            description: "Use to turn an ambiguous request into a written specification with acceptance criteria.",
            instructions: "You are a specification writer. Stay read-only.\n\nTurn the request into something buildable: the behaviour, the edge cases, the acceptance criteria, and the explicit non-goals. List every ambiguity you had to resolve and how you resolved it, so a wrong assumption is visible rather than buried.",
            sandbox_mode: Some("read-only"),
        },
        AgentTemplate {
            id: "data_analyst",
            label: "Data Analyst",
            category: "data",
            blurb: "Queries and summarizes data, and states its limits.",
            description: "Use to query a dataset or database and summarize what the numbers actually support.",
            instructions: "You are a data analyst.\n\nAnswer the question with the query you ran and the result, not with a summary alone. State the sample, the time range and the filters, and say what the data cannot answer. Never present a correlation as a cause.",
            sandbox_mode: Some("read-only"),
        },
    ]
}

// ---------------------------------------------------------------------------
// Orchestrator skill (~/.codex/skills/loom-orchestrator/SKILL.md)
//
// Codex skills activate implicitly when the user's request matches the
// skill description (progressive disclosure: only name+description sit in
// context until then). This generated skill is the missing link between
// natural language ("use multi agents to review this") and explicit
// subagent delegation: it carries the *current* agent roster with
// delegation-ready descriptions, so the main model knows exactly which
// agents exist and when to spawn each one. Regenerated on every agent
// upsert/delete; removed when no custom agents remain.
// ---------------------------------------------------------------------------

fn orchestrator_skill_dir_in(codex_home: &std::path::Path) -> PathBuf {
    codex_home.join("skills").join("loom-orchestrator")
}

/// Rewrite the orchestrator skill from the current agent roster. With no
/// custom agents the skill is removed entirely — built-in agents need no
/// routing help.
fn sync_orchestrator_skill_in(codex_home: &std::path::Path) -> anyhow::Result<()> {
    let dir = orchestrator_skill_dir_in(codex_home);
    let agents = agents_list_in(&codex_home.join("agents"))?;
    if agents.is_empty() {
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        return Ok(());
    }

    let mut roster = String::new();
    for a in &agents {
        let model = a.model.as_deref().unwrap_or("inherits the session model");
        roster.push_str(&format!(
            "- **{}** (model: `{}`): {}\n",
            a.name,
            model,
            a.description.trim()
        ));
    }

    let skill = format!(
        "---\n\
         name: loom-orchestrator\n\
         description: \"Use when the user asks to run tasks with multiple agents, subagents or specialists, delegate or fan out work, or get parallel reviews — for example 'use multi agents to review this', 'spawn agents to check this', 'have specialists look at this'.\"\n\
         ---\n\
         \n\
         # LoomRouter Agent Orchestration\n\
         \n\
         The user has custom Codex subagents installed (managed by LoomRouter). When a request involves delegating, fanning out, or using multiple agents or specialists, use this roster to pick the right agents — do not ask the user which ones to use.\n\
         \n\
         ## Available agents\n\
         \n\
         {roster}\n\
         ## How to delegate\n\
         \n\
         1. Map each part of the user's request to the agent whose description matches it best.\n\
         2. Spawn the selected agents in parallel when their tasks are independent; chain them when one needs another's output.\n\
         3. Give each spawned agent a focused, self-contained task — subagents start with a fresh context.\n\
         4. Wait for all of them, then consolidate their results into one answer.\n\
         \n\
         If no custom agent fits, fall back to the built-in agents (`worker` for implementation, `explorer` for read-only codebase exploration).\n"
    );

    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("SKILL.md"), skill)?;
    Ok(())
}

/// Ensure the orchestrator skill reflects the current roster (e.g. after
/// the user edits TOML files by hand outside LoomRouter).
pub fn sync_orchestrator_skill() -> anyhow::Result<()> {
    sync_orchestrator_skill_in(&codex_home())
}

// ---------------------------------------------------------------------------
// Multi-agent feature flag ([features] multi_agent in config.toml)
//
// Subagent spawning requires the Codex multi-agent feature. The flag lives
// in the user's own config, outside the LoomRouter managed block, so we
// patch it textually: rewriting the file through a TOML serializer would
// drop every comment (including our BEGIN/END markers).
// ---------------------------------------------------------------------------

/// Whether `features.multi_agent` is enabled in ~/.codex/config.toml.
pub fn multi_agent_enabled() -> bool {
    multi_agent_enabled_in(&codex_home().join("config.toml"))
}

fn multi_agent_enabled_in(cfg_path: &std::path::Path) -> bool {
    std::fs::read_to_string(cfg_path)
        .ok()
        .and_then(|raw| toml::from_str::<toml::Value>(&raw).ok())
        .and_then(|v| v.get("features")?.get("multi_agent")?.as_bool())
        .unwrap_or(false)
}

/// Set `features.multi_agent` without disturbing anything else in the
/// file. Returns the new state.
pub fn set_multi_agent(enabled: bool) -> anyhow::Result<bool> {
    set_multi_agent_in(&codex_home().join("config.toml"), enabled)
}

fn set_multi_agent_in(cfg_path: &std::path::Path, enabled: bool) -> anyhow::Result<bool> {
    let raw = std::fs::read_to_string(cfg_path).unwrap_or_default();
    let nl = detect_newline(&raw);
    let value = if enabled { "true" } else { "false" };

    let mut lines: Vec<String> = raw.lines().map(str::to_string).collect();
    // Find the [features] table header and its body span (up to the next
    // table header or EOF).
    let header_idx = lines.iter().position(|l| l.trim() == "[features]");
    match header_idx {
        Some(h) => {
            let body_end = lines[h + 1..]
                .iter()
                .position(|l| {
                    let t = l.trim_start();
                    t.starts_with('[') && !t.starts_with('#')
                })
                .map(|p| h + 1 + p)
                .unwrap_or(lines.len());
            let key_idx = lines[h + 1..body_end]
                .iter()
                .position(|l| l.trim_start().starts_with("multi_agent"))
                .map(|p| h + 1 + p);
            match key_idx {
                Some(k) => lines[k] = format!("multi_agent = {value}"),
                None => lines.insert(h + 1, format!("multi_agent = {value}")),
            }
        }
        None => {
            if !lines.last().map(|l| l.trim().is_empty()).unwrap_or(true) {
                lines.push(String::new());
            }
            lines.push("[features]".into());
            lines.push(format!("multi_agent = {value}"));
        }
    }
    let mut out = lines.join(nl);
    out.push_str(nl);
    write_config_atomic(cfg_path, &out)?;
    Ok(enabled)
}

/// Dominant line ending of the original file, so rewrites never flip the
/// user's config from CRLF to LF (Windows).
fn detect_newline(raw: &str) -> &'static str {
    if raw.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

/// Remove the managed block. Line endings of the surviving content are
/// preserved.
///
/// A BEGIN marker without a matching END is what an external rewrite of
/// config.toml leaves behind (the Codex desktop app re-serializes the file
/// and drops the trailing comment). Bricking apply/remove over that would
/// strand the user, so we recover by ownership: drop the orphan marker and
/// strip exactly what we can prove is ours — the owned root keys and the
/// `[model_providers.loomrouter]` table (see `recover_orphan_managed_block`).
/// Content that is not ours is never touched.
fn strip_managed_block(raw: &str) -> anyhow::Result<String> {
    let nl = detect_newline(raw);
    let mut out = String::new();
    let mut inside = false;
    let mut saw_begin = false;
    let mut saw_end = false;
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed == BEGIN_MARK {
            inside = true;
            saw_begin = true;
            continue;
        }
        if trimmed == END_MARK {
            inside = false;
            saw_end = true;
            continue;
        }
        if !inside {
            out.push_str(line);
            out.push_str(nl);
        }
    }
    if saw_begin && !saw_end {
        return recover_orphan_managed_block(raw, nl);
    }
    Ok(out)
}

/// Whether a `config.toml` line is the `[model_providers.loomrouter]` table
/// header (or one of its sub-tables).
fn is_loomrouter_table(line: &str) -> bool {
    let t = line.trim();
    let Some(inner) = t.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
        return false;
    };
    let inner = inner.trim();
    inner == "model_providers.loomrouter" || inner.starts_with("model_providers.loomrouter.")
}

/// Whether a `config.toml` carries content LoomRouter demonstrably owns: a
/// `loomrouter` provider table, or a root `model_provider` pointing at it.
fn has_loomrouter_content(raw: &str) -> bool {
    raw.lines().any(|l| {
        is_loomrouter_table(l)
            || (is_root_assignment(l, "model_provider")
                && l.split_once('=')
                    .is_some_and(|(_, v)| v.contains("loomrouter")))
    })
}

/// Recover from a `# BEGIN` marker with no matching `# END`.
///
/// The orphan is the signature of an external rewrite (e.g. the Codex
/// desktop app round-tripping config.toml and dropping the trailing
/// comment), not necessarily an interrupted write. Removing the marker and
/// then stripping by ownership is safe: only the owned root keys and the
/// `[model_providers.loomrouter]` table are deleted, and anything that is
/// not ours — marketplaces, plugins, hooks, mcp_servers — survives intact.
/// When nothing of ours is present the marker is genuinely ambiguous, and
/// we keep the defensive refusal instead of guessing.
fn recover_orphan_managed_block(raw: &str, nl: &str) -> anyhow::Result<String> {
    let without_marker: String = raw
        .lines()
        .filter(|l| l.trim() != BEGIN_MARK)
        .collect::<Vec<_>>()
        .join(nl);
    if !has_loomrouter_content(&without_marker) {
        anyhow::bail!(
            "config.toml has a loom-router BEGIN marker without a matching END \
             and no loom-router-managed content to remove; refusing to modify \
             the file. Restore it from config.toml.bak or remove the marker \
             manually."
        );
    }
    Ok(strip_legacy_install(&without_marker))
}

/// Root keys the integration owns. The managed block re-declares all of
/// them, so any copy left outside the markers is a duplicate key in the
/// making.
const OWNED_ROOT_KEYS: [&str; 3] = ["model_provider", "openai_base_url", "model_catalog_json"];

/// Strip a legacy, unmarked LoomRouter install.
///
/// Versions before the BEGIN/END markers wrote the provider block straight
/// into `config.toml`: the owned root keys plus a
/// `[model_providers.loomrouter]` table (and its `.http_headers` sub-table).
/// `strip_managed_block` cannot see that shape, so applying on top of it
/// duplicated `model_provider` at the root and the parse check refused
/// every write — the integration could never apply again.
///
/// Detection is by ownership: a `loomrouter` provider table, or a root
/// `model_provider` pointing at it. A user's own `model_provider` naming
/// any other provider, and profile-level keys, are left untouched.
fn strip_legacy_install(stripped: &str) -> String {
    if !has_loomrouter_content(stripped) {
        return stripped.to_string();
    }

    let nl = detect_newline(stripped);
    let mut out: Vec<&str> = Vec::new();
    let mut seen_table = false;
    let mut in_legacy_table = false;
    for line in stripped.lines() {
        let t = line.trim_start();
        if t.starts_with('[') {
            seen_table = true;
            in_legacy_table = is_loomrouter_table(line);
            if in_legacy_table {
                continue;
            }
        // One condition, because both drop the line: inside the legacy
        // table, or a root key we own from before there was a table.
        } else if in_legacy_table
            || (!seen_table && OWNED_ROOT_KEYS.iter().any(|k| is_root_assignment(line, k)))
        {
            continue;
        }
        out.push(line);
    }
    let mut joined = out.join(nl);
    if !joined.is_empty() {
        joined.push_str(nl);
    }
    joined
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Provider, ProviderModel, ProviderProtocol};

    /// Serializes the tests that mutate `CODEX_HOME` (and `CODEX_BIN`): cargo
    /// runs tests in parallel threads, and env vars are process-global, so
    /// two such tests writing different temp dirs would clobber each other.
    fn codex_home_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    use std::collections::BTreeMap;

    #[test]
    fn strip_only_managed_block() {
        let raw = "model = \"gpt-5\"\n\n# BEGIN loom-router-managed\nopenai_base_url = \"x\"\n# END loom-router-managed\n\n[profiles.work]\n";
        let out = strip_managed_block(raw).unwrap();
        assert!(out.contains("model = \"gpt-5\""));
        assert!(out.contains("[profiles.work]"));
        assert!(!out.contains("openai_base_url"));
    }

    #[test]
    fn strip_refuses_begin_without_end() {
        // An orphan BEGIN with *no* loom-router content is genuinely
        // ambiguous (a stray marker with nothing of ours behind it); the
        // defensive refusal stays so we never guess and delete user data.
        let raw = "model = \"gpt-5\"\n# BEGIN loom-router-managed\nopenai_base_url = \"x\"\n[profiles.work]\n";
        assert!(strip_managed_block(raw).is_err());
    }

    #[test]
    fn orphan_begin_is_recovered_by_ownership() {
        // Replicates a real breakage: the Codex desktop app re-serialized
        // config.toml and dropped the `# END` comment, leaving an orphan
        // BEGIN with our owned root keys + provider table inside it. The
        // strip must remove exactly what is ours and keep the rest.
        let raw = "notify = [\"turn-ended\"]\napproval_policy = \"on-request\"\n# BEGIN loom-router-managed\nmodel_provider = \"loomrouter\"\nopenai_base_url = \"http://127.0.0.1:4180/v1\"\nmodel_catalog_json = \"~/.codex/loom-router/merged-models.json\"\nmodel = \"opencode-zen-chat/deepseek-v4-flash\"\nmodel_reasoning_effort = \"medium\"\n\n[model_providers.loomrouter]\nname = \"OpenAI\"\nbase_url = \"http://127.0.0.1:4180/v1\"\nwire_api = \"responses\"\nhttp_headers = { \"x-loomrouter-token\" = \"t\", \"Authorization\" = \"Bearer t\" }\n\n[marketplaces.openai-bundled]\nlast_updated = \"2026-08-06T13:58:21Z\"\n\n[hooks.state]\n";
        let out = strip_managed_block(raw).unwrap();
        // Our markers and owned root keys are gone.
        assert!(!out.contains(BEGIN_MARK));
        assert!(!out.contains("model_provider"));
        assert!(!out.contains("openai_base_url"));
        assert!(!out.contains("model_catalog_json"));
        assert!(!out.contains("[model_providers.loomrouter]"));
        assert!(!out.contains("x-loomrouter-token"));
        // The user's own settings survive untouched.
        assert!(out.contains("notify = [\"turn-ended\"]"));
        assert!(out.contains("approval_policy = \"on-request\""));
        assert!(out.contains("[marketplaces.openai-bundled]"));
        assert!(out.contains("last_updated = \"2026-08-06T13:58:21Z\""));
        assert!(out.contains("[hooks.state]"));
        // And the result parses cleanly (no duplicate keys left behind).
        let parsed: toml::Value = toml::from_str(&out).unwrap();
        assert_eq!(
            parsed.get("approval_policy").and_then(toml::Value::as_str),
            Some("on-request")
        );
    }

    #[test]
    fn orphan_recovery_preserves_model_key_restore_path() {
        // The orphaned region may also swallow root `model`/effort keys the
        // desktop app re-emitted; `remove()` reconciles the root `model`
        // afterwards, so recovery must leave the stripped text parseable.
        let raw = "# BEGIN loom-router-managed\nmodel_provider = \"loomrouter\"\nopenai_base_url = \"x\"\nmodel_catalog_json = \"y\"\nmodel = \"deepseek/deepseek-chat\"\nmodel_reasoning_effort = \"medium\"\n\n[model_providers.loomrouter]\nwire_api = \"responses\"\n\n[profiles.work]\nmodel = \"gpt-5\"\n";
        let out = strip_managed_block(raw).unwrap();
        assert!(!out.contains(BEGIN_MARK));
        assert!(!out.contains("model_provider"));
        assert!(!out.contains("[model_providers.loomrouter]"));
        assert!(out.contains("[profiles.work]"));
        assert!(out.contains("model = \"gpt-5\""));
    }

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

    #[test]
    fn legacy_unmarked_install_is_stripped() {
        // Pre-marker versions wrote the provider block without BEGIN/END;
        // applying on top duplicated `model_provider` and the parse check
        // refused every write. The legacy shape must migrate away cleanly.
        let raw = "model = \"gpt-5\"\napproval_policy = \"never\"\nmodel_provider = \"loomrouter\"\nopenai_base_url = \"http://127.0.0.1:4180/v1\"\nmodel_catalog_json = \"C:/x/merged-models.json\"\n\n[model_providers.loomrouter]\nname = \"OpenAI\"\nbase_url = \"http://127.0.0.1:4180/v1\"\n\n[model_providers.loomrouter.http_headers]\nAuthorization = \"Bearer t\"\n\n[profiles.work]\nmodel = \"gpt-5\"\nmodel_provider = \"openai\"\n";
        let out = strip_legacy_install(raw);
        // User root keys survive; owned root keys and the provider table go.
        assert!(out.contains("model = \"gpt-5\""));
        assert!(out.contains("approval_policy = \"never\""));
        assert!(!out.contains("openai_base_url"));
        assert!(!out.contains("model_catalog_json"));
        assert!(!out.contains("[model_providers.loomrouter]"));
        assert!(!out.contains("Authorization"));
        // Other tables — including a profile's own `model_provider` — stay.
        assert!(out.contains("[profiles.work]"));
        assert!(out.contains("model_provider = \"openai\""));
        // And a fresh managed block on top parses without duplicate keys.
        let out = insert_root_block(&out, &managed_block(4180, "C:/x/merged-models.json", false));
        let parsed: toml::Value = toml::from_str(&out).unwrap();
        assert_eq!(
            parsed.get("model_provider").and_then(toml::Value::as_str),
            Some("loomrouter")
        );
        assert_eq!(
            parsed["profiles"]["work"]["model_provider"].as_str(),
            Some("openai")
        );
    }

    #[test]
    fn legacy_detection_ignores_other_providers() {
        // A user's own provider with no loomrouter table anywhere is not a
        // legacy install: nothing is touched.
        let raw = "model_provider = \"openai\"\nopenai_base_url = \"http://example/v1\"\n\n[profiles.work]\n";
        assert_eq!(strip_legacy_install(raw), raw);
    }

    #[test]
    fn crlf_files_keep_crlf() {
        let raw = "model = \"gpt-5\"\r\n\r\n# BEGIN loom-router-managed\r\nopenai_base_url = \"x\"\r\n# END loom-router-managed\r\n\r\n[profiles.work]\r\n";
        let stripped = strip_managed_block(raw).unwrap();
        let block =
            "# BEGIN loom-router-managed\nopenai_base_url = \"x\"\n# END loom-router-managed";
        let out = insert_root_block(&stripped, block);
        assert!(out.contains("\r\n"));
        // No bare LF line endings were introduced.
        assert!(
            !out.replace("\r\n", "").contains('\n'),
            "bare LF found:\n{out}"
        );
        let parsed: toml::Value = toml::from_str(&out).unwrap();
        assert_eq!(
            parsed.get("openai_base_url").and_then(toml::Value::as_str),
            Some("x")
        );
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
                    enabled: true,
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
    fn root_model_key_replaces_only_the_model_key() {
        // `model_provider` and `model_catalog_json` share the prefix and
        // must survive; the user's own `model` is the one that moves.
        let stripped = "model = \"gpt-5.5\"\nmodel_provider = \"loomrouter\"\nmodel_catalog_json = \"/tmp/x.json\"\n\n[profiles.work]\nmodel = \"gpt-5\"\n";
        let out = set_root_model_key(stripped, Some("deepseek/deepseek-chat"));
        assert!(out.contains("model = \"deepseek/deepseek-chat\""));
        assert!(out.contains("model_provider = \"loomrouter\""));
        assert!(out.contains("model_catalog_json = \"/tmp/x.json\""));
        // The profile's own model is below the first table header and is
        // not a root key — it must not be rewritten.
        assert!(out.contains("[profiles.work]\nmodel = \"gpt-5\""));
        let parsed: toml::Value = toml::from_str(&out).unwrap();
        assert_eq!(
            parsed.get("model").and_then(toml::Value::as_str),
            Some("deepseek/deepseek-chat")
        );
    }

    #[test]
    fn root_model_key_inserts_and_clears() {
        let inserted = set_root_model_key("model_provider = \"loomrouter\"\n", Some("a/b"));
        assert!(inserted.starts_with("model = \"a/b\"\n"));
        // Exactly one root `model` key: a duplicate makes Codex reject the
        // whole config.toml.
        assert_eq!(
            inserted
                .lines()
                .filter(|l| l.trim_start().starts_with("model ="))
                .count(),
            1
        );
        let cleared = set_root_model_key(&inserted, None);
        assert!(!cleared.contains("model = \"a/b\""));
        assert!(cleared.contains("model_provider"));
    }

    #[test]
    fn a_users_own_model_is_never_deleted_and_is_given_back() {
        let mut cfg = demo_config();
        let user_config = "model = \"gpt-5.5\"\n[profiles.work]\n";

        // Nothing selected: LoomRouter must not touch a key it does not own.
        assert_eq!(reconcile_root_model(user_config, &cfg), user_config);

        // Selecting a model displaces it (state::set_active_model is what
        // records the backup; here it is already recorded).
        cfg.active_model = Some("deepseek/deepseek-chat".into());
        cfg.codex_model_backup = Some("gpt-5.5".into());
        let switched = reconcile_root_model(user_config, &cfg);
        assert!(switched.contains("model = \"deepseek/deepseek-chat\""));
        assert!(!switched.contains("gpt-5.5"));

        // Clearing the selection restores what Codex had before us.
        cfg.active_model = None;
        let restored = reconcile_root_model(&switched, &cfg);
        assert!(restored.contains("model = \"gpt-5.5\""));
        assert!(!restored.contains("deepseek/deepseek-chat"));
    }

    #[test]
    fn a_stale_slug_of_ours_is_dropped_when_there_is_nothing_to_restore() {
        let mut cfg = demo_config();
        // Ours, but the selection is gone and no backup was ever taken.
        cfg.active_model = None;
        let out = reconcile_root_model("model = \"deepseek/deepseek-chat\"\n", &cfg);
        assert!(!out.contains("model ="), "stale slug survived:\n{out}");
    }

    #[test]
    fn quoted_and_commented_model_keys_are_still_recognized() {
        // A quoted key that went unmatched used to leave the old assignment
        // in place next to the new one — a duplicate key Codex rejects.
        let out = set_root_model_key("\"model\" = \"gpt-5.5\"\n", Some("a/b"));
        assert_eq!(
            out.lines().filter(|l| is_root_model_line(l)).count(),
            1,
            "duplicate root model key:\n{out}"
        );
        toml::from_str::<toml::Value>(&out).unwrap();
        // A trailing comment must not leak into the parsed value.
        assert_eq!(
            root_model_key("model = \"gpt-5.5\" # pinned\n").as_deref(),
            Some("gpt-5.5")
        );
    }

    #[test]
    fn active_slug_follows_native_slug_mode_and_drops_stale_picks() {
        let mut cfg = demo_config();
        cfg.active_model = Some("deepseek/deepseek-chat".into());
        assert_eq!(active_slug(&cfg).as_deref(), Some("deepseek/deepseek-chat"));

        cfg.native_slug_mode = true;
        assert_eq!(active_slug(&cfg).as_deref(), Some("deepseek-chat"));

        // Disabling the model (or its provider) makes the pick unroutable,
        // so nothing is published rather than a dangling pointer.
        cfg.native_slug_mode = false;
        cfg.providers.get_mut("deepseek").unwrap().models[0].enabled = false;
        assert_eq!(active_slug(&cfg), None);
        cfg.providers.get_mut("deepseek").unwrap().models[0].enabled = true;
        cfg.providers.get_mut("deepseek").unwrap().enabled = false;
        assert_eq!(active_slug(&cfg), None);

        cfg.providers.get_mut("deepseek").unwrap().enabled = true;
        cfg.active_model = Some("ghost/model".into());
        assert_eq!(active_slug(&cfg), None);
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
    fn merged_catalog_works_without_native() {
        let merged = build_merged_catalog(&demo_config(), &json!({"models": []}));
        let models = merged["models"].as_array().unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["slug"], "deepseek/deepseek-chat");
    }

    #[test]
    fn apply_refuses_empty_catalog_and_rolls_back() {
        // Regression test for the macOS report: applying with zero enabled
        // models wrote an empty merged-models.json, and Codex then refused
        // to load config.toml entirely ("must contain at least one model").
        let _guard = codex_home_guard();
        let tmp = std::env::temp_dir().join(format!("loom-codex-test-{}", std::process::id()));
        std::env::set_var("CODEX_HOME", &tmp);
        // A binary that never runs, so the native capture fails gracefully
        // instead of probing the real CLI.
        std::env::set_var("CODEX_BIN", "loom-router-test-no-such-codex");

        let mut cfg = demo_config();
        for p in cfg.providers.values_mut() {
            p.enabled = false;
        }
        // Seed the broken state: a managed block from a previous apply.
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(
            tmp.join("config.toml"),
            "model = \"gpt-5.5\"\n\n# BEGIN loom-router-managed\nopenai_base_url = \"x\"\n# END loom-router-managed\n",
        )
        .unwrap();

        let result = apply(&cfg, 4180);

        std::env::remove_var("CODEX_HOME");
        std::env::remove_var("CODEX_BIN");
        let written = std::fs::read_to_string(tmp.join("config.toml")).unwrap();
        let _ = std::fs::remove_dir_all(&tmp);

        assert!(result.is_err());
        // The broken managed block was rolled back; user keys survived.
        assert!(!written.contains(BEGIN_MARK));
        assert!(written.contains("model = \"gpt-5.5\""));
    }

    #[test]
    fn root_block_goes_before_first_table() {
        let raw = "model = \"gpt-5.5\"\n\n[plugins.\"a@b\"]\nenabled = true\n\n[[hooks.SessionStart]]\nmatcher = \"startup\"\n";
        let block =
            "# BEGIN loom-router-managed\nopenai_base_url = \"x\"\n# END loom-router-managed";
        let out = insert_root_block(raw, block);
        let block_pos = out.find("openai_base_url").unwrap();
        let table_pos = out.find("[plugins").unwrap();
        assert!(
            block_pos < table_pos,
            "block must be in root section:\n{out}"
        );
        assert!(out.contains("model = \"gpt-5.5\""));
        // A TOML parser must see the keys at root level.
        let parsed: toml::Value = toml::from_str(&out).unwrap();
        assert_eq!(
            parsed.get("openai_base_url").and_then(toml::Value::as_str),
            Some("x")
        );
    }

    #[test]
    fn root_block_appends_when_no_tables() {
        let out = insert_root_block("model = \"gpt-5.5\"\n", "# B\nx = 1\n# E");
        assert!(out.contains("model = \"gpt-5.5\"\n\n# B"));
    }

    #[test]
    fn managed_block_is_valid_toml_with_websockets_on() {
        let block = managed_block(
            4180,
            "C:/Users/x/.codex/loom-router/merged-models.json",
            false,
        );
        let out = insert_root_block(
            "model = \"kimi-coding/k3\"\n\n[plugins.a]\nenabled = true\n",
            &block,
        );
        let parsed: toml::Value = toml::from_str(&out).unwrap();
        assert_eq!(
            parsed.get("model_provider").and_then(toml::Value::as_str),
            Some("loomrouter")
        );
        let provider = &parsed["model_providers"]["loomrouter"];
        assert_eq!(provider["supports_websockets"].as_bool(), Some(true));
        assert_eq!(provider["wire_api"].as_str(), Some("responses"));
        assert_eq!(provider["requires_openai_auth"].as_bool(), Some(true));
        // The block carries the local proxy token so Codex can authenticate.
        let headers = provider["http_headers"].as_table().unwrap();
        assert!(headers.contains_key("x-loomrouter-token"));
        assert!(headers["Authorization"]
            .as_str()
            .unwrap()
            .starts_with("Bearer "));
        // User tables survive intact after the managed provider table.
        assert_eq!(parsed["plugins"]["a"]["enabled"].as_bool(), Some(true));
        // Stripping removes the whole block, including the provider table.
        let stripped = strip_managed_block(&out).unwrap();
        assert!(!stripped.contains("loomrouter"));
        assert!(stripped.contains("[plugins.a]"));
    }

    // ---------------------------------------------------------------------
    // Native slug mode (use Codex without an OpenAI login)
    // ---------------------------------------------------------------------

    #[test]
    fn native_slug_mode_drops_openai_auth_requirement() {
        let block = managed_block(4180, "C:/x/merged-models.json", true);
        // BEGIN/END markers are `#` comments, so the block parses as-is.
        let parsed: toml::Value = toml::from_str(&block).unwrap();
        let provider = &parsed["model_providers"]["loomrouter"];
        // The whole point of the mode: no ChatGPT login gate. Codex then
        // authenticates only with the static proxy-token headers.
        assert_eq!(provider["requires_openai_auth"].as_bool(), Some(false));
        let headers = provider["http_headers"].as_table().unwrap();
        assert!(headers["Authorization"]
            .as_str()
            .unwrap()
            .starts_with("Bearer "));
    }

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
                    enabled: true,
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

    // ---------------------------------------------------------------------
    // Custom agents (~/.codex/agents/*.toml)
    // ---------------------------------------------------------------------

    #[test]
    fn agents_round_trip_list_upsert_delete() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("agents");

        let agent = AgentInfo {
            name: "reviewer".into(),
            description: "Use for read-only code review.".into(),
            model: Some("kimi-coding/k3".into()),
            effort: Some("high".into()),
            sandbox_mode: Some("read-only".into()),
            instructions: "Review code like an owner.\nPrioritize correctness.".into(),
        };
        // Upsert creates the agents directory.
        agents_upsert_in(&agents, &agent).unwrap();

        let listed = agents_list_in(&agents).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, agent.name);
        assert_eq!(listed[0].description, agent.description);
        assert_eq!(listed[0].model, agent.model);
        assert_eq!(listed[0].effort, agent.effort);
        assert_eq!(listed[0].sandbox_mode, agent.sandbox_mode);
        assert_eq!(listed[0].instructions, agent.instructions);

        // Codex-required keys are present in the written file.
        let raw = std::fs::read_to_string(agents.join("reviewer.toml")).unwrap();
        let parsed: toml::Value = toml::from_str(&raw).unwrap();
        assert_eq!(parsed["name"].as_str(), Some("reviewer"));
        assert_eq!(
            parsed["description"].as_str(),
            Some("Use for read-only code review.")
        );
        assert_eq!(
            parsed["developer_instructions"].as_str(),
            Some(agent.instructions.as_str())
        );
        assert_eq!(parsed["model"].as_str(), Some("kimi-coding/k3"));
        assert_eq!(parsed["model_reasoning_effort"].as_str(), Some("high"));
        assert_eq!(parsed["sandbox_mode"].as_str(), Some("read-only"));

        // Update: dropping model/effort/sandbox removes the keys; an empty
        // description keeps the existing one (legacy behavior).
        let updated = AgentInfo {
            model: None,
            effort: None,
            sandbox_mode: None,
            description: String::new(),
            ..agent.clone()
        };
        agents_upsert_in(&agents, &updated).unwrap();
        let raw = std::fs::read_to_string(agents.join("reviewer.toml")).unwrap();
        let parsed: toml::Value = toml::from_str(&raw).unwrap();
        assert!(parsed.get("model").is_none());
        assert!(parsed.get("model_reasoning_effort").is_none());
        assert!(parsed.get("sandbox_mode").is_none());
        assert_eq!(
            parsed["description"].as_str(),
            Some("Use for read-only code review.")
        );

        // Delete is idempotent.
        agents_delete_in(&agents, "reviewer").unwrap();
        assert!(agents_list_in(&agents).unwrap().is_empty());
        agents_delete_in(&agents, "reviewer").unwrap();
    }

    #[test]
    fn agents_reject_malicious_names() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("agents");
        let evil = [
            "../escape",
            "..",
            "a/b",
            "a\\b",
            ".hidden",
            "with.dot",
            "sp ace",
            "",
        ];
        for name in evil {
            assert!(validate_agent_name(name).is_err(), "accepted '{name}'");
            let agent = AgentInfo {
                name: name.into(),
                description: String::new(),
                model: None,
                effort: None,
                sandbox_mode: None,
                instructions: "x".into(),
            };
            assert!(agents_upsert_in(&agents, &agent).is_err());
            assert!(agents_delete_in(&agents, name).is_err());
        }
        // Nothing was created outside or inside the dir.
        assert!(!dir.path().join("escape.toml").exists());
        assert!(agents_list_in(&agents).unwrap().is_empty());
    }

    #[test]
    fn agents_preserve_unknown_fields_on_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        // A file written by the user/CLI with fields AgentInfo does not model.
        std::fs::write(
            agents.join("docs_researcher.toml"),
            "name = \"docs_researcher\"\n\
             description = \"Docs specialist (user-written)\"\n\
             model = \"gpt-5.6-luna\"\n\
             sandbox_mode = \"read-only\"\n\
             developer_instructions = \"Use the docs MCP server.\"\n\
             \n\
             [mcp_servers.openaiDeveloperDocs]\n\
             url = \"https://developers.openai.com/mcp\"\n",
        )
        .unwrap();

        let listed = agents_list_in(&agents).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "docs_researcher");
        assert_eq!(listed[0].model.as_deref(), Some("gpt-5.6-luna"));
        assert_eq!(listed[0].effort, None);

        // Upsert patches modeled fields and keeps everything else.
        let updated = AgentInfo {
            name: "docs_researcher".into(),
            description: String::new(),
            model: Some("deepseek/deepseek-chat".into()),
            effort: Some("medium".into()),
            sandbox_mode: Some("read-only".into()),
            instructions: "Use the docs MCP server. Cite versions.".into(),
        };
        agents_upsert_in(&agents, &updated).unwrap();
        let raw = std::fs::read_to_string(agents.join("docs_researcher.toml")).unwrap();
        let parsed: toml::Value = toml::from_str(&raw).unwrap();
        // User-written description survives (only a missing one is derived).
        assert_eq!(
            parsed["description"].as_str(),
            Some("Docs specialist (user-written)")
        );
        assert_eq!(parsed["sandbox_mode"].as_str(), Some("read-only"));
        assert_eq!(
            parsed["mcp_servers"]["openaiDeveloperDocs"]["url"].as_str(),
            Some("https://developers.openai.com/mcp")
        );
        assert_eq!(parsed["model"].as_str(), Some("deepseek/deepseek-chat"));
        assert_eq!(parsed["model_reasoning_effort"].as_str(), Some("medium"));
        assert_eq!(
            parsed["developer_instructions"].as_str(),
            Some("Use the docs MCP server. Cite versions.")
        );
    }

    #[test]
    fn agents_list_skips_invalid_files() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(agents.join("broken.toml"), "not = [valid toml").unwrap();
        std::fs::write(agents.join("notes.md"), "# not an agent").unwrap();
        assert!(agents_list_in(&agents).unwrap().is_empty());
    }

    #[test]
    fn agents_reject_invalid_sandbox_mode() {
        let dir = tempfile::tempdir().unwrap();
        let agent = AgentInfo {
            name: "reviewer".into(),
            description: String::new(),
            model: None,
            effort: None,
            sandbox_mode: Some("yolo".into()),
            instructions: "x".into(),
        };
        assert!(agents_upsert_in(&dir.path().join("agents"), &agent).is_err());
    }

    #[test]
    fn every_template_carries_a_known_category() {
        // The gallery groups and searches on this; an unknown slug renders
        // as the raw value and silently escapes translation.
        const KNOWN: &[&str] = &[
            "review",
            "build",
            "investigate",
            "quality",
            "ship",
            "write",
            "data",
            "ops",
        ];
        let templates = agent_templates();
        // Large enough that search is the point of the screen, not a nicety.
        assert!(
            templates.len() >= 20,
            "catalogue shrank to {}",
            templates.len()
        );
        for t in &templates {
            assert!(
                KNOWN.contains(&t.category),
                "{}: unknown category {:?}",
                t.id,
                t.category
            );
        }
        // A catalogue that is all one category is not a catalogue.
        let distinct: std::collections::HashSet<_> = templates.iter().map(|t| t.category).collect();
        assert!(
            distinct.len() >= 5,
            "only {} categories used",
            distinct.len()
        );
    }

    #[test]
    fn templates_are_delegation_ready() {
        let templates = agent_templates();
        assert!(templates.len() >= 8);
        let mut names = std::collections::HashSet::new();
        for t in &templates {
            assert!(names.insert(t.id), "duplicate template id {}", t.id);
            validate_agent_name(t.id).unwrap();
            // The description is what Codex reads to route delegations;
            // it must be a real "use when..." sentence, not a placeholder.
            assert!(t.description.len() > 40, "{}: weak description", t.id);
            assert!(!t.instructions.is_empty(), "{}: no instructions", t.id);
            if let Some(mode) = t.sandbox_mode {
                assert!(matches!(mode, "read-only" | "workspace-write"));
            }
        }
        // Reviewers and auditors must never edit files.
        let reviewer = templates.iter().find(|t| t.id == "reviewer").unwrap();
        assert_eq!(reviewer.sandbox_mode, Some("read-only"));
        let auditor = templates
            .iter()
            .find(|t| t.id == "security_auditor")
            .unwrap();
        assert_eq!(auditor.sandbox_mode, Some("read-only"));
    }

    #[test]
    fn orchestrator_skill_tracks_roster() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let agents = home.join("agents");

        let agent = AgentInfo {
            name: "reviewer".into(),
            description: "Use for read-only code review.".into(),
            model: Some("deepseek/deepseek-chat".into()),
            effort: None,
            sandbox_mode: Some("read-only".into()),
            instructions: "Review code.".into(),
        };
        agents_upsert_in(&agents, &agent).unwrap();

        let skill_path = home.join("skills/loom-orchestrator/SKILL.md");
        let raw = std::fs::read_to_string(&skill_path).unwrap();
        assert!(raw.starts_with("---\nname: loom-orchestrator"));
        // The roster carries the name, routed model and description.
        assert!(raw.contains("**reviewer** (model: `deepseek/deepseek-chat`)"));
        assert!(raw.contains("Use for read-only code review."));

        // Empty description in the roster falls back to the derived one.
        assert!(!raw.contains("(model: `inherits the session model`)"));

        // Deleting the last agent removes the skill entirely.
        agents_delete_in(&agents, "reviewer").unwrap();
        assert!(!skill_path.exists());
    }

    #[test]
    fn set_multi_agent_patches_features_without_disturbing_config() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        let original = "# my config\n\
             model = \"gpt-5.5\"\n\
             \n\
             # BEGIN loom-router-managed\n\
             openai_base_url = \"http://127.0.0.1:4180/v1\"\n\
             # END loom-router-managed\n\
             \n\
             [plugins.a]\n\
             enabled = true\n";
        std::fs::write(&cfg, original).unwrap();

        // Enable: creates the [features] table at the end.
        set_multi_agent_in(&cfg, true).unwrap();
        assert!(multi_agent_enabled_in(&cfg));
        let raw = std::fs::read_to_string(&cfg).unwrap();
        assert!(raw.contains("# my config"));
        assert!(raw.contains(BEGIN_MARK));
        assert!(raw.contains("[plugins.a]"));

        // Disable: updates the key in place instead of duplicating it.
        set_multi_agent_in(&cfg, false).unwrap();
        let raw = std::fs::read_to_string(&cfg).unwrap();
        assert!(!multi_agent_enabled_in(&cfg));
        assert_eq!(raw.matches("multi_agent").count(), 1);
        assert_eq!(raw.matches("[features]").count(), 1);

        // Enabling again flips the existing key, and an existing
        // [features] table gains the key without moving other content.
        std::fs::write(
            &cfg,
            "[features]\nfoo = 1\n\n[profiles.work]\nmodel = \"gpt-5.5\"\n",
        )
        .unwrap();
        set_multi_agent_in(&cfg, true).unwrap();
        let raw = std::fs::read_to_string(&cfg).unwrap();
        assert!(multi_agent_enabled_in(&cfg));
        assert!(raw.contains("foo = 1"));
        assert!(raw.contains("[profiles.work]"));
        let features_pos = raw.find("multi_agent").unwrap();
        let profiles_pos = raw.find("[profiles.work]").unwrap();
        assert!(features_pos < profiles_pos, "key must live in [features]");
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
