use super::{template, AgentTemplate};

pub(super) fn worker() -> AgentTemplate {
    template(
        "worker",
        "Worker",
        "build",
        "Implements a well-scoped task and reports what changed.",
        "Use for focused implementation tasks and bug fixes with a clear scope.",
        Some("workspace-write"),
        "You are an implementation worker.\n\nExecute the task you are given and nothing more. Keep changes scoped, follow the repository's existing conventions, and run the project's own checks when available. Report back concisely: what changed, what you verified, and anything you could not validate.",
    )
}
