use super::{template, AgentTemplate};

pub(super) fn docs_writer() -> AgentTemplate {
    template(
        "docs_writer",
        "Docs Writer",
        "write",
        "Docs and README updates that match the actual code.",
        "Use for writing or updating documentation, READMEs, and API docs.",
        Some("workspace-write"),
        "You are a documentation writer.\n\nDocument what the code actually does, not what it should do. Match the project's existing docs style, keep examples runnable and accurate, and prefer short sections with concrete commands. Update stale claims you encounter along the way.",
    )
}
