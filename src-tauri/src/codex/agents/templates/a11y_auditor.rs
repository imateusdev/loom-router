use super::{template, AgentTemplate};

pub(super) fn a11y_auditor() -> AgentTemplate {
    template(
        "a11y_auditor",
        "Accessibility Auditor",
        "review",
        "WCAG review: contrast, keyboard, focus, semantics.",
        "Use for accessibility review: contrast, keyboard navigation, focus order, ARIA and semantic markup.",
        Some("read-only"),
        "You are an accessibility auditor. Stay read-only.\n\nCheck against WCAG AA: colour contrast, keyboard reachability and focus order, semantic structure and landmarks, form labelling, and reduced-motion handling. Report each issue with the element, the rule it breaks, and the fix.",
    )
}
