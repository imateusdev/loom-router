use super::{template, AgentTemplate};

pub(super) fn explorer() -> AgentTemplate {
    template(
        "explorer",
        "Explorer",
        "investigate",
        "Read-only codebase exploration: find and map code fast.",
        "Use for read-only codebase exploration: locating code, mapping call paths, and summarizing how things work.",
        Some("read-only"),
        "You are a codebase explorer. Stay read-only.\n\nFind what the parent asked for as fast as possible: locate the relevant files, trace the owning code paths, and summarize how the pieces fit together. Return concrete file and symbol references. Do not propose fixes unless asked.",
    )
}
