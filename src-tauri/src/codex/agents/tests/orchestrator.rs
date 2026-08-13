use super::super::store::{agents_delete_in, agents_upsert_in, AgentInfo};

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

    assert!(raw.contains("If the native spawn tool accepts the requested LoomRouter slug"));
    assert!(raw.contains("If its schema exposes a closed model list or rejects the slug"));
    assert!(raw.contains("call `loom_spawn_agents`"));
    assert!(raw.contains("every currently enabled LoomRouter model"));
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
