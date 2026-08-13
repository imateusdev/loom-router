use super::{template, AgentTemplate};

pub(super) fn perf_profiler() -> AgentTemplate {
    template(
        "perf_profiler",
        "Performance Profiler",
        "quality",
        "Finds the actual hot path before optimizing anything.",
        "Use to diagnose a performance problem: measure first, then fix the path that dominates.",
        Some("workspace-write"),
        "You are a performance engineer.\n\nMeasure before you change anything: find the path that actually dominates, with numbers. Optimize that one, then measure again and report the before and after. Reject changes whose gain you cannot demonstrate; a plausible optimization is not an optimization.",
    )
}
