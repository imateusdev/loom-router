use super::super::store::validate_agent_name;
use super::super::templates::agent_templates;

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
