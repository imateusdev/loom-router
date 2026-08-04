//! Codex integration: the piece that makes external models appear in
//! Codex's own model picker alongside the native GPT models.
//!
//! Two artifacts:
//!   1. A marked managed block in `~/.codex/config.toml` pointing
//!      `openai_base_url` at the LoomRouter proxy and `model_catalog_json`
//!      at the merged catalog.
//!   2. `~/.codex/loom-router/merged-models.json`: the native Codex catalog
//!      merged with every enabled external model.
//!
//! Everything LoomRouter writes is wrapped in BEGIN/END markers so removal
//! is exact and the user's own settings are never touched.

use crate::config::AppConfig;
use serde::Serialize;
use serde_json::{json, Value};
use std::path::PathBuf;

pub const BEGIN_MARK: &str = "# BEGIN loom-router-managed";
pub const END_MARK: &str = "# END loom-router-managed";

#[derive(Debug, Clone, Serialize)]
pub struct CodexStatus {
    pub codex_home: String,
    pub config_exists: bool,
    pub managed_block_present: bool,
    pub merged_catalog_present: bool,
    pub merged_model_count: usize,
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

pub fn status(config: &AppConfig) -> CodexStatus {
    let home = codex_home();
    let cfg_path = home.join("config.toml");
    let raw = std::fs::read_to_string(&cfg_path).unwrap_or_default();
    let catalog = std::fs::read_to_string(merged_catalog_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());
    let count = catalog
        .as_ref()
        .and_then(|c| c.get("models").and_then(Value::as_array).map(Vec::len))
        .unwrap_or(0);
    CodexStatus {
        codex_home: home.display().to_string(),
        config_exists: cfg_path.exists(),
        managed_block_present: raw.contains(BEGIN_MARK),
        merged_catalog_present: catalog.is_some(),
        merged_model_count: count.max(enabled_model_count(config)),
    }
}

fn enabled_model_count(config: &AppConfig) -> usize {
    config
        .providers
        .values()
        .filter(|p| p.enabled)
        .map(|p| p.models.iter().filter(|m| m.enabled).count())
        .sum()
}

/// Build the merged catalog: native models (read from the existing Codex
/// catalog if present) plus one entry per enabled external model.
pub fn build_merged_catalog(config: &AppConfig) -> Value {
    let mut models: Vec<Value> = Vec::new();

    // Preserve native entries if Codex already has a catalog on disk.
    // (The exact upstream source of the native catalog is Codex-internal;
    // keeping any pre-existing entries avoids clobbering GPT models.)
    if let Ok(raw) = std::fs::read_to_string(merged_catalog_path()) {
        if let Ok(existing) = serde_json::from_str::<Value>(&raw) {
            if let Some(arr) = existing.get("models").and_then(Value::as_array) {
                for m in arr {
                    let external = m
                        .get("x-loom-router")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if !external {
                        models.push(m.clone());
                    }
                }
            }
        }
    }

    for p in config.providers.values().filter(|p| p.enabled) {
        for m in p.models.iter().filter(|m| m.enabled) {
            let slug = format!("{}/{}", p.id, m.id);
            models.push(json!({
                "slug": slug,
                "name": m.label.clone().unwrap_or_else(|| m.id.clone()),
                "provider": p.id,
                "x-loom-router": true,
            }));
        }
    }

    json!({ "models": models })
}

/// Apply the integration: write merged catalog + managed config block.
pub fn apply(config: &AppConfig, port: u16) -> anyhow::Result<()> {
    std::fs::create_dir_all(loom_dir())?;

    let catalog = build_merged_catalog(config);
    let catalog_path = merged_catalog_path();
    std::fs::write(&catalog_path, serde_json::to_string_pretty(&catalog)?)?;

    let block = format!(
        "{BEGIN_MARK}\nopenai_base_url = \"http://127.0.0.1:{port}/v1\"\nmodel_catalog_json = \"{}\"\n{END_MARK}",
        catalog_path.display().to_string().replace('\\', "/")
    );

    let cfg_path = codex_home().join("config.toml");
    let raw = std::fs::read_to_string(&cfg_path).unwrap_or_default();
    let stripped = strip_managed_block(&raw);
    let mut out = stripped.trim_end().to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(&block);
    out.push('\n');
    std::fs::write(&cfg_path, out)?;
    Ok(())
}

/// Remove the integration: delete managed block and merged catalog.
pub fn remove() -> anyhow::Result<()> {
    let cfg_path = codex_home().join("config.toml");
    if let Ok(raw) = std::fs::read_to_string(&cfg_path) {
        let stripped = strip_managed_block(&raw);
        std::fs::write(&cfg_path, stripped)?;
    }
    let catalog = merged_catalog_path();
    if catalog.exists() {
        std::fs::remove_file(catalog)?;
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

    #[test]
    fn strip_only_managed_block() {
        let raw = "model = \"gpt-5\"\n\n# BEGIN loom-router-managed\nopenai_base_url = \"x\"\n# END loom-router-managed\n\n[profiles.work]\n";
        let out = strip_managed_block(raw);
        assert!(out.contains("model = \"gpt-5\""));
        assert!(out.contains("[profiles.work]"));
        assert!(!out.contains("openai_base_url"));
    }
}
