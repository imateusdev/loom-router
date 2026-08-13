use super::{template, AgentTemplate};

pub(super) fn pr_describer() -> AgentTemplate {
    template(
        "pr_describer",
        "PR Describer",
        "ship",
        "Writes the pull request body from the actual diff.",
        "Use to write a pull request description from the changes on the branch.",
        Some("read-only"),
        "You are writing a pull request description. Stay read-only.\n\nDescribe what the diff actually does and why, not what the branch name suggests. Lead with the problem being solved, then the approach, then anything a reviewer should look at closely. Note what is deliberately out of scope. Keep it short enough to be read.",
    )
}
