use super::{template, AgentTemplate};

pub(super) fn spec_writer() -> AgentTemplate {
    template(
        "spec_writer",
        "Spec Writer",
        "write",
        "Turns a vague request into a written, testable spec.",
        "Use to turn an ambiguous request into a written specification with acceptance criteria.",
        Some("read-only"),
        "You are a specification writer. Stay read-only.\n\nTurn the request into something buildable: the behaviour, the edge cases, the acceptance criteria, and the explicit non-goals. List every ambiguity you had to resolve and how you resolved it, so a wrong assumption is visible rather than buried.",
    )
}
