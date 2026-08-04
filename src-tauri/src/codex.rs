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

pub fn status(_config: &AppConfig) -> CodexStatus {
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
    let bin_root = dirs::data_local_dir()?.join("OpenAI").join("Codex").join("bin");
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
pub fn capture_native_catalog() -> anyhow::Result<Value> {
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
            anyhow::bail!("codex debug models failed: {}", String::from_utf8_lossy(&out.stderr));
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
    // never pile up as duplicates in the next merge.
    let models: Vec<Value> = models
        .into_iter()
        .filter(|m| {
            m.get("slug")
                .and_then(Value::as_str)
                .map(|s| !s.contains('/'))
                .unwrap_or(true)
        })
        .collect();
    if models.is_empty() {
        anyhow::bail!("Codex returned an empty or invalid model catalog");
    }
    let catalog = json!({ "models": models });
    std::fs::create_dir_all(loom_dir())?;
    std::fs::write(native_catalog_path(), serde_json::to_string_pretty(&catalog)?)?;
    Ok(catalog)
}

fn load_native_catalog() -> Value {
    std::fs::read_to_string(native_catalog_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({ "models": [] }))
}

/// Field overrides applied to a cloned native template for each external
/// model. Mirrors the schema Codex emits for its own models.
fn routed_model(template: &Value, provider_id: &str, model_id: &str, label: Option<&str>, priority: i64) -> Value {
    let mut m: Map<String, Value> = template.as_object().cloned().unwrap_or_default();
    m.insert("slug".into(), json!(format!("{provider_id}/{model_id}")));
    m.insert(
        "display_name".into(),
        json!(label.unwrap_or(model_id).to_string()),
    );
    m.insert("description".into(), json!(format!("{} via LoomRouter ({})", model_id, provider_id)));
    m.insert("priority".into(), json!(priority));
    m.insert("visibility".into(), json!("list"));
    m.insert("supported_in_api".into(), json!(true));
    // Reasoning levels the model actually supports (Kimi K3 per docs:
    // low/high/max). Enables the effort picker in Codex.
    m.insert(
        "supported_reasoning_levels".into(),
        json!([
            {"effort": "low", "description": "Fast responses with lighter reasoning"},
            {"effort": "high", "description": "Greater reasoning depth for complex problems"},
            {"effort": "max", "description": "Maximum reasoning depth"}
        ]),
    );
    m.insert("default_reasoning_level".into(), json!("high"));
    m.insert("context_window".into(), json!(128_000));
    m.insert("max_context_window".into(), json!(128_000));
    m.insert("effective_context_window_percent".into(), json!(95));
    m.insert("input_modalities".into(), json!(["text"]));
    m.insert("additional_speed_tiers".into(), json!([]));
    m.insert("service_tiers".into(), json!([]));
    m.insert("availability_nux".into(), Value::Null);
    m.insert("upgrade".into(), Value::Null);
    m.insert("supports_reasoning_summaries".into(), json!(false));
    m.insert("default_reasoning_summary".into(), json!("none"));
    m.insert("support_verbosity".into(), json!(false));
    m.insert("default_verbosity".into(), Value::Null);
    m.insert("supports_search_tool".into(), json!(false));
    m.insert("supports_image_detail_original".into(), json!(false));
    m.insert("use_responses_lite".into(), json!(false));
    m.insert("multi_agent_version".into(), json!("v1"));
    Value::Object(m)
}

/// Build the merged catalog: every native model (so GPT stays in the picker)
/// plus one entry per enabled external model cloned from a native template.
pub fn build_merged_catalog(config: &AppConfig, native: &Value) -> Value {
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

    let mut models = native_models;
    // External entries start after native priorities.
    let mut priority = 100_i64;
    for p in config.providers.values().filter(|p| p.enabled) {
        for m in p.models.iter().filter(|m| m.enabled) {
            models.push(routed_model(&template, &p.id, &m.id, m.label.as_deref(), priority));
            priority += 1;
        }
    }

    // Dedupe by slug; routed entries win over any stale native-copy entry.
    let mut seen = std::collections::HashSet::new();
    let mut deduped: Vec<Value> = Vec::with_capacity(models.len());
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
    let mut models = deduped;

    models.sort_by_key(|m| {
        m.get("priority").and_then(Value::as_i64).unwrap_or(999)
    });
    json!({ "models": models })
}

/// Apply the integration: refresh native catalog (best effort), write the
/// merged catalog, and install the managed config block.
pub fn apply(config: &AppConfig, port: u16) -> anyhow::Result<()> {
    std::fs::create_dir_all(loom_dir())?;

    // Refresh the native catalog when the CLI is available; otherwise reuse
    // the previous capture (or an empty set with a warning).
    if codex_bin().is_some() {
        if let Err(e) = capture_native_catalog() {
            tracing::warn!("native catalog capture failed, reusing previous: {e}");
        }
    } else {
        tracing::warn!("Codex CLI not found; merged catalog will only include external models");
    }
    let native = load_native_catalog();

    let catalog = build_merged_catalog(config, &native);
    let catalog_path = merged_catalog_path();
    std::fs::write(&catalog_path, serde_json::to_string_pretty(&catalog)?)?;

    let block = format!(
        "{BEGIN_MARK}\nopenai_base_url = \"http://127.0.0.1:{port}/v1\"\nmodel_catalog_json = \"{}\"\n{END_MARK}",
        catalog_path.display().to_string().replace('\\', "/")
    );

    let cfg_path = codex_home().join("config.toml");
    let raw = std::fs::read_to_string(&cfg_path).unwrap_or_default();
    let stripped = strip_managed_block(&raw);
    let out = insert_root_block(&stripped, &block);
    std::fs::write(&cfg_path, out)?;
    Ok(())
}

/// TOML root keys must appear before the first `[table]` header; appending
/// at EOF would nest them under the last table. Insert the managed block
/// right before the first table header, or at the end if there are none.
fn insert_root_block(raw: &str, block: &str) -> String {
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
            lines.insert(insert_at, block);
            out.push_str(&lines.join("\n"));
            out.push('\n');
        }
        None => {
            out.push_str(raw.trim_end());
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(block);
            out.push('\n');
        }
    }
    out
}

/// Remove the integration: delete managed block and merged catalog.
pub fn remove() -> anyhow::Result<()> {
    let cfg_path = codex_home().join("config.toml");
    if let Ok(raw) = std::fs::read_to_string(&cfg_path) {
        let stripped = strip_managed_block(&raw);
        std::fs::write(&cfg_path, stripped)?;
    }
    for path in [merged_catalog_path()] {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn strip_managed_block(raw: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in raw.lines() {
        if line.trim() == BEGIN_MARK {
            inside = true;
            continue;
        }
        if line.trim() == END_MARK {
            inside = false;
            continue;
        }
        if !inside {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Provider, ProviderModel, ProviderProtocol};
    use std::collections::BTreeMap;

    #[test]
    fn strip_only_managed_block() {
        let raw = "model = \"gpt-5\"\n\n# BEGIN loom-router-managed\nopenai_base_url = \"x\"\n# END loom-router-managed\n\n[profiles.work]\n";
        let out = strip_managed_block(raw);
        assert!(out.contains("model = \"gpt-5\""));
        assert!(out.contains("[profiles.work]"));
        assert!(!out.contains("openai_base_url"));
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
        assert_eq!(models[0]["supported_reasoning_levels"], json!(["low", "high"]));
        // External entry cloned from the template with overrides.
        let ext = &models[1];
        assert_eq!(ext["slug"], "deepseek/deepseek-chat");
        assert_eq!(ext["display_name"], "DeepSeek Chat");
        assert_eq!(ext["visibility"], "list");
        assert_eq!(ext["supported_in_api"], true);
        assert_eq!(ext["base_instructions"], "You are Codex.");
        assert_eq!(ext["context_window"], 128_000);
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
        let block = "# BEGIN loom-router-managed\nopenai_base_url = \"x\"\n# END loom-router-managed";
        let out = insert_root_block(raw, block);
        let block_pos = out.find("openai_base_url").unwrap();
        let table_pos = out.find("[plugins").unwrap();
        assert!(block_pos < table_pos, "block must be in root section:\n{out}");
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
}
