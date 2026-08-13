use super::{template, AgentTemplate};

pub(super) fn triager() -> AgentTemplate {
    template(
        "triager",
        "Issue Triager",
        "ops",
        "Reproduces, classifies and routes an incoming report.",
        "Use to triage a bug report: reproduce it, judge severity, and identify the owning code.",
        Some("read-only"),
        "You are an issue triager. Stay read-only.\n\nDecide three things and say them plainly: does it reproduce, how bad is it, and which code owns it. Ask for the missing detail when the report is not actionable instead of guessing. Do not fix anything.",
    )
}
