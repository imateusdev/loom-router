use super::*;

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
        tags: vec![
            "review".into(),
            "security".into(),
            "REVIEW".into(),
            " ".into(),
        ],
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
    assert_eq!(listed[0].tags, vec!["review", "security"]);

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
    assert_eq!(
        parsed["tags"].as_array().map(|tags| tags
            .iter()
            .filter_map(toml::Value::as_str)
            .collect::<Vec<_>>()),
        Some(vec!["review", "security"])
    );

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
            tags: vec![],
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
        tags: vec![],
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
        tags: vec![],
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
        tags: vec![],
    };
    agents_upsert_in(&agents, &agent).unwrap();

    let skill_path = home.join("skills/loom-orchestrator/SKILL.md");
    let raw = std::fs::read_to_string(&skill_path).unwrap();
    assert!(raw.starts_with("---\nname: loom-orchestrator"));
    // The roster carries the name, routed model and description.
    assert!(raw.contains("**reviewer** (model: `deepseek/deepseek-chat`)"));
    assert!(raw.contains("Use for read-only code review."));
    assert!(raw.contains("## Model routing"));
    assert!(raw.contains("Do not replace them with Claude Code's built-in models"));
    assert!(raw.contains("A user-requested model is not limited to the saved roster"));
    assert!(raw.contains("derive an ad hoc worker"));
    assert!(raw.contains("Saved agents have priority over ad hoc workers"));
    assert!(raw.contains("Treat omissions as delegation authority"));
    assert!(raw.contains("or if an underspecified task materially benefits from one"));
    assert!(raw.contains("An absent roster match is not a reason to refuse delegation"));
    assert!(raw.contains("Report completions in the chat as they arrive"));
    assert!(raw.contains("delegation tree is complete"));

    // Empty description in the roster falls back to the derived one.
    assert!(!raw.contains("(model: `inherits the current LoomRouter model`)"));

    // Deleting the last agent removes the skill entirely.
    agents_delete_in(&agents, "reviewer").unwrap();
    assert!(!skill_path.exists());
}

fn write_agent_for_orchestrator(
    agents: &std::path::Path,
    name: &str,
    description: &str,
    model: Option<&str>,
) {
    agents_upsert_in(
        agents,
        &AgentInfo {
            name: name.into(),
            description: description.into(),
            model: model.map(str::to_string),
            effort: None,
            sandbox_mode: Some("read-only".into()),
            instructions: format!("Perform the {name} role."),
            tags: vec![],
        },
    )
    .unwrap();
}

#[test]
fn orchestrator_skill_prioritizes_saved_agents_before_ad_hoc_workers() {
    let dir = tempfile::tempdir().unwrap();
    write_agent_for_orchestrator(
        &dir.path().join("agents"),
        "reviewer",
        "Use for code review.",
        Some("deepseek/deepseek-chat"),
    );

    let raw =
        std::fs::read_to_string(dir.path().join("skills/loom-orchestrator/SKILL.md")).unwrap();
    let saved_priority = raw.find("Saved agents have priority").unwrap();
    let ad_hoc_fallback = raw.find("When no saved specialist fits").unwrap();

    assert!(saved_priority < ad_hoc_fallback);
    assert!(raw.contains("unless the user explicitly requests a different model or rules"));
}

#[test]
fn orchestrator_skill_delegates_underspecified_requests_without_questions() {
    let dir = tempfile::tempdir().unwrap();
    write_agent_for_orchestrator(
        &dir.path().join("agents"),
        "planner",
        "Use to decompose broad requests.",
        None,
    );

    let raw =
        std::fs::read_to_string(dir.path().join("skills/loom-orchestrator/SKILL.md")).unwrap();

    for inferred in ["useful roles", "worker count", "models", "fan-out", "depth"] {
        assert!(
            raw.contains(inferred),
            "missing automatic choice: {inferred}"
        );
    }
    assert!(raw.contains("Delegate immediately without asking the user"));
    assert!(raw.contains("Treat omissions as delegation authority, not as blockers"));
}

#[test]
fn orchestrator_skill_supports_recursive_but_bounded_delegation() {
    let dir = tempfile::tempdir().unwrap();
    write_agent_for_orchestrator(
        &dir.path().join("agents"),
        "planner",
        "Use to plan hierarchical work.",
        None,
    );

    let raw =
        std::fs::read_to_string(dir.path().join("skills/loom-orchestrator/SKILL.md")).unwrap();

    assert!(raw.contains("tell each parent worker to repeat this routing procedure"));
    assert!(raw.contains("selected child model, rules, fan-out and remaining depth"));
    assert!(raw.contains("Respect the session's concurrency slots"));
    assert!(raw.contains("never promise unbounded or exponential simultaneous execution"));
}

#[test]
fn orchestrator_skill_distinguishes_host_model_limits_from_roster_membership() {
    let dir = tempfile::tempdir().unwrap();
    write_agent_for_orchestrator(
        &dir.path().join("agents"),
        "reviewer",
        "Use for code review.",
        Some("deepseek/deepseek-chat"),
    );

    let raw =
        std::fs::read_to_string(dir.path().join("skills/loom-orchestrator/SKILL.md")).unwrap();

    assert!(raw.contains("If the spawn tool accepts a free-form model"));
    assert!(raw.contains("If its schema exposes a closed model list"));
    assert!(raw.contains("host tool rejected the model"));
    assert!(raw.contains("never claim that LoomRouter itself lacks the model"));
}

#[test]
fn orchestrator_skill_lists_all_saved_agents_in_stable_order() {
    let dir = tempfile::tempdir().unwrap();
    let agents = dir.path().join("agents");
    write_agent_for_orchestrator(
        &agents,
        "zeta_reviewer",
        "Use for final review.",
        Some("deepseek/deepseek-chat"),
    );
    write_agent_for_orchestrator(&agents, "alpha_planner", "Use for initial planning.", None);

    let raw =
        std::fs::read_to_string(dir.path().join("skills/loom-orchestrator/SKILL.md")).unwrap();
    let alpha = raw.find("**alpha_planner**").unwrap();
    let zeta = raw.find("**zeta_reviewer**").unwrap();

    assert!(alpha < zeta);
    assert!(raw.contains("**alpha_planner** (model: `inherits the current LoomRouter model`)"));
    assert!(raw.contains("**zeta_reviewer** (model: `deepseek/deepseek-chat`)"));
}

#[test]
fn orchestrator_skill_regeneration_removes_deleted_agent_only() {
    let dir = tempfile::tempdir().unwrap();
    let agents = dir.path().join("agents");
    write_agent_for_orchestrator(&agents, "planner", "Use for planning.", None);
    write_agent_for_orchestrator(&agents, "reviewer", "Use for review.", None);

    agents_delete_in(&agents, "planner").unwrap();

    let skill_path = dir.path().join("skills/loom-orchestrator/SKILL.md");
    let raw = std::fs::read_to_string(&skill_path).unwrap();
    assert!(!raw.contains("**planner**"));
    assert!(raw.contains("**reviewer**"));
    assert!(skill_path.exists());
}

#[test]
fn orchestrator_skill_reports_each_subagent_completion_in_chat() {
    let dir = tempfile::tempdir().unwrap();
    write_agent_for_orchestrator(
        &dir.path().join("agents"),
        "reviewer",
        "Use for review.",
        None,
    );

    let raw =
        std::fs::read_to_string(dir.path().join("skills/loom-orchestrator/SKILL.md")).unwrap();

    assert!(raw.contains("For each finished subagent"));
    assert!(raw.contains("agent name, `completed` or `failed`"));
    assert!(raw.contains("one-line result summary"));
    assert!(raw.contains("Do not leave successful subagent work visible only in tool output"));
}

#[test]
fn orchestrator_skill_reports_final_tree_status_and_partial_failures() {
    let dir = tempfile::tempdir().unwrap();
    write_agent_for_orchestrator(
        &dir.path().join("agents"),
        "planner",
        "Use for planning.",
        None,
    );

    let raw =
        std::fs::read_to_string(dir.path().join("skills/loom-orchestrator/SKILL.md")).unwrap();

    assert!(raw.contains("explicitly state that the delegation tree is complete"));
    assert!(raw.contains("If some workers failed or were blocked, name them"));
    assert!(raw.contains("preserve the successful results"));
}
