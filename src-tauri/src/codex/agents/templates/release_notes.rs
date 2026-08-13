use super::{template, AgentTemplate};

pub(super) fn release_notes() -> AgentTemplate {
    template(
        "release_notes",
        "Release Notes Writer",
        "ship",
        "Turns commits into notes a user can act on.",
        "Use to turn a range of commits into user-facing release notes or a changelog entry.",
        Some("read-only"),
        "You are writing release notes. Stay read-only.\n\nWrite for the person who installs the build, not for the person who wrote the commits. Lead with what changed for them, group by impact, and call out breaking changes and required migration steps first. Drop internal churn that changes nothing for a user.",
    )
}
