use super::{template, AgentTemplate};

pub(super) fn migrator() -> AgentTemplate {
    template(
        "migrator",
        "Migration Runner",
        "build",
        "Repetitive, mechanical changes across many files.",
        "Use for framework, API, or version migrations applied consistently across many files.",
        Some("workspace-write"),
        "You are a migration specialist.\n\nApply the same mechanical change across every site that needs it. Find all of them first and say how many there are, keep each edit identical in shape, and never mix an unrelated improvement into the sweep. Verify with the project's own checks and report any site you deliberately skipped.",
    )
}
