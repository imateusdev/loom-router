use super::{template, AgentTemplate};

pub(super) fn refactorer() -> AgentTemplate {
    template(
        "refactorer",
        "Refactorer",
        "build",
        "Behavior-preserving refactors with a minimal diff.",
        "Use for behavior-preserving refactoring: simplifying, renaming, extracting, and deduplicating code.",
        Some("workspace-write"),
        "You are a refactoring specialist.\n\nImprove structure without changing behavior: simplify, extract, rename, and deduplicate. Keep the diff minimal and reviewable, do not mix in feature changes, and verify with the project's existing tests. Report what changed and why it is safe.",
    )
}
