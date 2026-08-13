// Index for the LoomRouter custom-agent surface: file persistence lives in
// `store`, the built-in gallery lives in `templates`, and the generated
// orchestrator skill lives in `orchestrator`.

#[path = "agents/orchestrator.rs"]
mod orchestrator;
#[path = "agents/store.rs"]
mod store;
#[path = "agents/templates/mod.rs"]
mod templates;

pub use orchestrator::sync_orchestrator_skill;
pub use store::{agents_delete, agents_list, agents_upsert, AgentInfo};
pub use templates::{agent_templates, AgentTemplate};

#[cfg(test)]
#[path = "agents/tests/mod.rs"]
mod tests;
