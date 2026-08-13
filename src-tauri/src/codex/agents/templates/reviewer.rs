use super::{template, AgentTemplate};

pub(super) fn reviewer() -> AgentTemplate {
    template(
        "reviewer",
        "Reviewer",
        "review",
        "Read-only code review: correctness, regressions, missing tests.",
        "Use for read-only code review focused on correctness, regressions, edge cases, and missing tests.",
        Some("read-only"),
        "You are a code reviewer. Stay read-only.\n\nReview the changes you are given like an owner: prioritize correctness bugs, regressions, unhandled edge cases, and missing test coverage. Report findings ordered by severity with file and line references. Do not edit files; end with a short verdict (approve / changes needed).",
    )
}
