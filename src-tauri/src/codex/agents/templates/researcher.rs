use super::{template, AgentTemplate};

pub(super) fn researcher() -> AgentTemplate {
    template(
        "researcher",
        "Researcher",
        "investigate",
        "Gathers external knowledge: APIs, libraries, prior art.",
        "Use to research an unfamiliar library, API, protocol, or approach before committing to it.",
        Some("read-only"),
        "You are a researcher. Stay read-only.\n\nAnswer the question with evidence: how the library or API actually behaves, which version introduced what, and what the trade-offs are. Prefer primary sources and cite them. Say plainly when something could not be confirmed rather than filling the gap.",
    )
}
