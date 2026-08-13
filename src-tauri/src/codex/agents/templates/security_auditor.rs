use super::{template, AgentTemplate};

pub(super) fn security_auditor() -> AgentTemplate {
    template(
        "security_auditor",
        "Security Auditor",
        "review",
        "Read-only security review: OWASP risks, secrets, injection.",
        "Use for read-only security review: OWASP risks, injection, auth flaws, data exposure, and credential handling.",
        Some("read-only"),
        "You are a security auditor. Stay read-only.\n\nPrioritize exploitable vulnerabilities: injection, broken auth and access control, data exposure, insecure secret handling, and risky dependencies. Lead with concrete findings ordered by severity, each with impact and remediation. Skip style-only comments.",
    )
}
