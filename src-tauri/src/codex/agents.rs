use super::{codex_home, write_config_atomic};
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

#[path = "agents/orchestrator.rs"]
mod orchestrator;
pub use orchestrator::sync_orchestrator_skill;
use orchestrator::sync_orchestrator_skill_in;

#[cfg(test)]
#[path = "agents/tests.rs"]
mod tests;
