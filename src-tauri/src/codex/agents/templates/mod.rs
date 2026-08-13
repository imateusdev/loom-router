mod a11y_auditor;
mod api_designer;
mod data_analyst;
mod debugger;
mod dep_upgrader;
mod docs_writer;
mod explorer;
mod incident_responder;
mod migrator;
mod perf_profiler;
mod planner;
mod pr_describer;
mod red_team;
mod refactorer;
mod release_notes;
mod researcher;
mod reviewer;
mod security_auditor;
mod spec_writer;
mod tester;
mod triager;
mod worker;

/// A ready-made agent recipe shown in the template gallery.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentTemplate {
    /// Suggested agent name (also the TOML filename stem).
    pub id: &'static str,
    /// Short UI label.
    pub label: &'static str,
    /// One-line UI summary of what the agent is for.
    pub blurb: &'static str,
    /// The `description` field written to the TOML — the text Codex reads
    /// when deciding which agent fits a delegation request.
    pub description: &'static str,
    /// The `developer_instructions` written to the TOML.
    pub instructions: &'static str,
    /// Suggested sandbox mode; None = inherit the session policy.
    pub sandbox_mode: Option<&'static str>,
    /// Grouping for the gallery, so a catalogue this size stays scannable.
    /// One of: review, build, investigate, quality, ship, write, data, ops.
    pub category: &'static str,
}

/// Single shared constructor so every template file only carries its data.
fn template(
    id: &'static str,
    label: &'static str,
    category: &'static str,
    blurb: &'static str,
    description: &'static str,
    sandbox_mode: Option<&'static str>,
    instructions: &'static str,
) -> AgentTemplate {
    AgentTemplate {
        id,
        label,
        category,
        blurb,
        description,
        sandbox_mode,
        instructions,
    }
}

/// A catalogue of agent roles, not a list of Codex features.
///
/// These are the delegation patterns that recur across the whole coding-agent
/// ecosystem — reviewer, planner, debugger, test writer, migration runner and
/// so on. They are transcribed here as plain role definitions so that picking
/// one writes a Codex agent into `~/.codex/agents`: the pattern is the
/// portable part, the TOML file is the Codex-specific part.
///
/// Instructions are agent-facing and stay in English regardless of UI
/// language — they are read by the model, not by the user.
pub fn agent_templates() -> Vec<AgentTemplate> {
    vec![
        reviewer::reviewer(),
        security_auditor::security_auditor(),
        worker::worker(),
        explorer::explorer(),
        tester::tester(),
        refactorer::refactorer(),
        debugger::debugger(),
        docs_writer::docs_writer(),
        planner::planner(),
        researcher::researcher(),
        red_team::red_team(),
        a11y_auditor::a11y_auditor(),
        perf_profiler::perf_profiler(),
        migrator::migrator(),
        api_designer::api_designer(),
        dep_upgrader::dep_upgrader(),
        triager::triager(),
        incident_responder::incident_responder(),
        pr_describer::pr_describer(),
        release_notes::release_notes(),
        spec_writer::spec_writer(),
        data_analyst::data_analyst(),
    ]
}
