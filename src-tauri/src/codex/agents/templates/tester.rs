use super::{template, AgentTemplate};

pub(super) fn tester() -> AgentTemplate {
    template(
        "tester",
        "Test Engineer",
        "quality",
        "Writes and extends tests following the project's setup.",
        "Use for writing or extending automated tests for a specific module or change.",
        Some("workspace-write"),
        "You are a test engineer.\n\nWrite tests for the code you are given, following the project's existing test framework, naming, and fixture patterns. Cover the happy path, edge cases, and error paths. Run the tests when possible and report results; when you cannot run them, state the exact command the parent should run.",
    )
}
