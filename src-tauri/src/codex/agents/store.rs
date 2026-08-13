use super::orchestrator::sync_orchestrator_skill_in;
use crate::codex::codex_home;
use crate::codex::config_patch::write_config_atomic;
use std::path::PathBuf;

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
    /// Free-form labels shown as colored tags in the UI and used to filter
    /// the roster. Stored as `tags` in the agent TOML.
    #[serde(default)]
    pub tags: Vec<String>,
}

fn agents_dir() -> PathBuf {
    codex_home().join("agents")
}

/// Safe file/name slug: no path separators, no traversal, no leading-dot
/// tricks. Codex's own examples use `snake_case` names.
pub(super) fn validate_agent_name(name: &str) -> anyhow::Result<()> {
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
    let tags = table
        .get("tags")
        .and_then(toml::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    AgentInfo {
        name: get_str("name").unwrap_or(fallback_name).to_string(),
        description: get_str("description")
            .map(str::to_string)
            .unwrap_or_else(|| derived_description(&instructions)),
        model: get_str("model").map(str::to_string),
        effort: get_str("model_reasoning_effort").map(str::to_string),
        sandbox_mode: get_str("sandbox_mode").map(str::to_string),
        instructions,
        tags,
    }
}

/// List implementation against an explicit directory, so tests never touch
/// the real `~/.codex` (and avoid CODEX_HOME env races between tests).
pub(super) fn agents_list_in(dir: &std::path::Path) -> anyhow::Result<Vec<AgentInfo>> {
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

pub(super) fn agents_upsert_in(dir: &std::path::Path, agent: &AgentInfo) -> anyhow::Result<()> {
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
    // Tags are a LoomRouter UI concept, so they are written as a plain
    // `tags` array that Codex ignores safely. Normalize duplicate labels so
    // roster filtering remains stable across edits.
    let mut tags = Vec::new();
    let mut seen = Vec::<String>::new();
    for raw in &agent.tags {
        let tag = raw.trim().to_string();
        if tag.is_empty() || seen.iter().any(|known| known.eq_ignore_ascii_case(&tag)) {
            continue;
        }
        seen.push(tag.clone());
        tags.push(toml::Value::String(tag));
    }
    table.insert("tags".into(), toml::Value::Array(tags));
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

pub(super) fn agents_delete_in(dir: &std::path::Path, name: &str) -> anyhow::Result<()> {
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
