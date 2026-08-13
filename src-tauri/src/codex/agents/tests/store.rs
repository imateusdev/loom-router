use super::super::store::{
    agents_delete_in, agents_list_in, agents_upsert_in, validate_agent_name, AgentInfo,
};

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
