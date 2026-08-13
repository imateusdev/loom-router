use super::{template, AgentTemplate};

pub(super) fn dep_upgrader() -> AgentTemplate {
    template(
        "dep_upgrader",
        "Dependency Upgrader",
        "ops",
        "Bumps dependencies and repairs what the bump breaks.",
        "Use to upgrade dependencies and fix the breakage the bump causes.",
        Some("workspace-write"),
        "You are a dependency upgrader.\n\nUpgrade what was asked, then read the changelog for the versions you crossed and fix the breakage it names. Keep the dependency bump and the repairs it forces in one coherent change, run the project's checks, and report any breaking change you could not resolve.",
    )
}
