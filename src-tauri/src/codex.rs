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
        native_catalog_present: native_catalog_path().exists(),
        merged_catalog_present: merged_catalog_path().exists(),
        merged_model_count: count,
        codex_cli_available: codex_bin().is_some(),
        integration_enabled: config.codex_integration,
    }
}

fn codex_bin() -> Option<String> {
    if let Ok(bin) = std::env::var("CODEX_BIN") {
        if !bin.is_empty() {
            return Some(bin);
        }
    }
    // Probe PATH for a runnable Codex CLI.
    let candidate = if cfg!(windows) { "codex.cmd" } else { "codex" };
    for name in [candidate, "codex"] {
        if std::process::Command::new(name)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return Some(name.to_string());
        }
    }
    bundled_desktop_cli()
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
    // Context window. The Kimi-specific name heuristic (K3 = 1M tokens;
    // 256k-class models = 256k) only applies to Kimi-family providers —
    // applying it to e.g. claude-sonnet-5 or grok-4.5 would publish a
    // window those models do not have. Every other provider uses its
    // explicit `context_window` override when configured, otherwise the
    // conservative DEFAULT_CONTEXT_WINDOW. Vision-capable per Kimi docs.
    let window: i64 = match crate::proxy::family_of(provider) {
        crate::proxy::ProviderFamily::Kimi => {
            if model_id.contains("256k") {
                262_144
            } else if model_id.contains("k3") {
                1_000_000
            } else {
                262_144
            }
        }
        _ => provider
            .context_window
            .map(i64::from)
            .unwrap_or(DEFAULT_CONTEXT_WINDOW),
    };
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
    let out = insert_root_block(&stripped, &block);
    write_config_atomic(&cfg_path, &out)?;
    Ok(())
}

/// Write the Codex `config.toml` atomically: copy the current file to
/// `config.toml.bak`, write the new content to a temp file in the same
/// directory, then rename over the target. A crash mid-write can at worst
/// leave a stale `.tmp` behind; the previous config survives in the backup.
fn write_config_atomic(path: &std::path::Path, contents: &str) -> anyhow::Result<()> {
    if path.exists() {
        let mut bak = path.as_os_str().to_owned();
        bak.push(".bak");
        std::fs::copy(path, PathBuf::from(bak))?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, contents)?;
    // Windows cannot rename over an existing destination; the backup above
    // covers this small remove+rename window.
    #[cfg(windows)]
    let _ = std::fs::remove_file(path);
    std::fs::rename(&tmp, path)?;
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
pub fn remove() -> anyhow::Result<()> {
    let cfg_path = codex_home().join("config.toml");
    if let Ok(raw) = std::fs::read_to_string(&cfg_path) {
        let stripped = strip_managed_block(&raw)?;
        write_config_atomic(&cfg_path, &stripped)?;
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
    /// Slug "provider/model" routed by LoomRouter, or None = Codex default.
    pub model: Option<String>,
    /// e.g. "low" | "medium" | "high", None = Codex default.
    pub effort: Option<String>,
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
    AgentInfo {
        name: get_str("name").unwrap_or(fallback_name).to_string(),
        model: get_str("model").map(str::to_string),
        effort: get_str("model_reasoning_effort").map(str::to_string),
        instructions: get_str("developer_instructions")
            .unwrap_or_default()
            .to_string(),
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
    std::fs::create_dir_all(dir)?;
    let path = agent_file(dir, &agent.name);
    // Round-trip preservation: load the existing file and patch only the
    // fields AgentInfo models, keeping anything else the user or the Codex
    // CLI wrote (description, sandbox_mode, mcp_servers, skills.config...).
    let mut table: toml::map::Map<String, toml::Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| toml::from_str::<toml::Value>(&raw).ok())
        .and_then(|v| v.as_table().cloned())
        .unwrap_or_default();
    table.insert("name".into(), toml::Value::String(agent.name.clone()));
    if !table.contains_key("description") {
        table.insert(
            "description".into(),
            toml::Value::String(derived_description(&agent.instructions)),
        );
    }
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
    let rendered = toml::to_string_pretty(&toml::Value::Table(table))?;
    // Same atomicity discipline as the config.toml writer (tmp + rename).
    write_config_atomic(&path, &rendered)
}

/// Create or update one custom agent (creates `~/.codex/agents/` if needed).
pub fn agents_upsert(agent: &AgentInfo) -> anyhow::Result<()> {
    agents_upsert_in(&agents_dir(), agent)
}

fn agents_delete_in(dir: &std::path::Path, name: &str) -> anyhow::Result<()> {
    validate_agent_name(name)?;
    // Idempotent: deleting an absent agent is a no-op.
    match std::fs::remove_file(agent_file(dir, name)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Delete one custom agent by name. Idempotent.
pub fn agents_delete(name: &str) -> anyhow::Result<()> {
    agents_delete_in(&agents_dir(), name)
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

/// Remove the managed block. A BEGIN marker without a matching END is the
/// signature of a previously interrupted write; in that case we refuse to
/// touch the file with an explicit error instead of silently deleting
/// everything after the marker. Line endings of the surviving content are
/// preserved.
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
        anyhow::bail!(
            "config.toml has a loom-router BEGIN marker without a matching END \
             (likely left over from an interrupted write); refusing to modify \
             the file. Restore it from config.toml.bak or remove the marker \
             manually."
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Provider, ProviderModel, ProviderProtocol};
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
        // Signature of an interrupted write: everything after BEGIN would be
        // silently deleted by the old implementation. Now it is an error.
        let raw = "model = \"gpt-5\"\n# BEGIN loom-router-managed\nopenai_base_url = \"x\"\n[profiles.work]\n";
        assert!(strip_managed_block(raw).is_err());
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
            autostart_server: false,
            codex_integration: false,
            side_call_fallback: None,
            native_slug_mode: false,
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
    fn merged_catalog_works_without_native() {
        let merged = build_merged_catalog(&demo_config(), &json!({"models": []}));
        let models = merged["models"].as_array().unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["slug"], "deepseek/deepseek-chat");
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
            model: Some("kimi-coding/k3".into()),
            effort: Some("high".into()),
            instructions: "Review code like an owner.\nPrioritize correctness.".into(),
        };
        // Upsert creates the agents directory.
        agents_upsert_in(&agents, &agent).unwrap();

        let listed = agents_list_in(&agents).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, agent.name);
        assert_eq!(listed[0].model, agent.model);
        assert_eq!(listed[0].effort, agent.effort);
        assert_eq!(listed[0].instructions, agent.instructions);

        // Codex-required keys are present in the written file.
        let raw = std::fs::read_to_string(agents.join("reviewer.toml")).unwrap();
        let parsed: toml::Value = toml::from_str(&raw).unwrap();
        assert_eq!(parsed["name"].as_str(), Some("reviewer"));
        assert!(parsed["description"]
            .as_str()
            .unwrap()
            .starts_with("Review code like an owner"));
        assert_eq!(
            parsed["developer_instructions"].as_str(),
            Some(agent.instructions.as_str())
        );
        assert_eq!(parsed["model"].as_str(), Some("kimi-coding/k3"));
        assert_eq!(parsed["model_reasoning_effort"].as_str(), Some("high"));

        // Update: dropping model/effort removes the keys (None = Codex default).
        let updated = AgentInfo {
            model: None,
            effort: None,
            ..agent.clone()
        };
        agents_upsert_in(&agents, &updated).unwrap();
        let raw = std::fs::read_to_string(agents.join("reviewer.toml")).unwrap();
        let parsed: toml::Value = toml::from_str(&raw).unwrap();
        assert!(parsed.get("model").is_none());
        assert!(parsed.get("model_reasoning_effort").is_none());

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
                model: None,
                effort: None,
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
            model: Some("deepseek/deepseek-chat".into()),
            effort: Some("medium".into()),
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
}
