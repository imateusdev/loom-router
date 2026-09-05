use crate::codex::codex_home;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Orchestrator skill (~/.codex/skills/loom-orchestrator/SKILL.md)
//
// Codex skills activate implicitly when the user's request matches the
// skill description (progressive disclosure: only name+description sit in
// context until then). This skill is the missing link between natural
// language ("use multi agents with deepseek to investigate this project")
// and actual delegation.
//
// It drives Codex's *native* multi-agent tools rather than any LoomRouter
// tool of our own. The native path is what produces the child-agent thread
// the user can watch, plus `wait_agent`/`list_agents` for progress and the
// session's real concurrency accounting. LoomRouter's contribution is that
// its models are already in Codex's catalog, so a native spawn can target
// them by slug.
//
// One subtlety the text has to carry: Codex only lets a child differ from
// its parent when the user, AGENTS.md, or *skill instructions* ask for it,
// and only when `fork_turns` is not a full-history fork. This skill is those
// skill instructions, so it says so explicitly and pins `fork_turns`.
//
// The text is static: there is no agent roster to interpolate. It is
// rewritten on every integration apply and at every app launch, so
// skill-text improvements reach existing installs.
// ---------------------------------------------------------------------------

const SKILL: &str = "---\n\
name: loom-orchestrator\n\
description: \"Use when the user asks to run tasks with multiple agents, subagents or specialists, delegate or fan out work, or get parallel reviews - for example 'use multi agents to review this', 'use multi agents with deepseek to investigate the project', 'spawn agents to check this', 'have specialists look at this'.\"\n\
---\n\
\n\
# LoomRouter Agent Orchestration\n\
\n\
Delegate with the native multi-agent tools: `spawn_agent`, `followup_task`, `send_message`, `wait_agent`, `interrupt_agent`, `list_agents`. What LoomRouter adds is reach: every model it publishes is already in your model catalog, so a subagent can run on any of them while you keep the normal child-agent thread, its live progress, and the concurrency accounting.\n\
\n\
When a request involves delegating, fanning out, or using multiple agents or specialists, act automatically. Workers are defined at spawn time from the request itself - there is no roster to consult and nothing for the user to pre-create. Never ask the user to create an agent first.\n\
\n\
## Choosing the model\n\
\n\
LoomRouter models appear in your catalog under their own slugs, usually `provider/model` (for example `opencode-go/deepseek-v4-pro`). When the user names a model loosely (\"deepseek\", \"the fast one\"), match it against that catalog and pass the slug exactly. Do not substitute a native GPT model for a slug the user asked for.\n\
\n\
Setting `model` on a spawn is authorized here: these are skill instructions, which is one of the conditions Codex requires before a child may differ from its parent. Two rules follow from that:\n\
\n\
- A spawn that sets `model` must also set `fork_turns`, because a full-history fork inherits the parent model and ignores the override - the worker would silently run on the wrong model while reporting success.\n\
- Prefer a small positive integer string, such as `fork_turns: \"2\"`. That satisfies the override rule and still hands the child the recent turns, which is what keeps it from having to reconstruct its own context.\n\
- Use `fork_turns: \"none\"` only when this conversation must not reach the model at all - remember that a LoomRouter slug is often a third-party provider. With `\"none\"` the child sees nothing of this conversation, so the task text has to carry every fact it needs.\n\
\n\
When the user names no model, omit `model` and let the child inherit.\n\
\n\
## Worker boundaries\n\
\n\
A worker that cannot tell what it was asked to do must say so and stop. Put that in the task text, along with the scope. It must never go looking for its own instructions: no reading session, rollout or transcript files, no grepping the machine for its own task name, no enumerating environment variables, and no reading, decrypting or reporting credentials, tokens or key material. None of that is ever part of the job, and an underspecified task is a reason to stop, not a licence to explore.\n\
\n\
This is not hypothetical. A worker spawned with no context and a thin task went hunting for its own task definition, and from there into secret scanning. Give it the context and the boundary instead.\n\
\n\
## Operating rules (single injection)\n\
\n\
Keep this block as the single source of truth. Do not duplicate it in prompts.\n\
\n\
- Parallel budget: `P = min(task_width, concurrency_slots, token_budget)`. Raise P only when the task graph has real parallel width, and never promise more simultaneous agents than the session has slots.\n\
- All agents share one working directory: edits by one are immediately visible to the others. No two agents may edit the same file in the same wave.\n\
- Token budget: 60% workers, 25% orchestrator/synthesis, 15% retries/review.\n\
\n\
## How to delegate\n\
\n\
Spawning is a tool call, never a shell command. Do NOT look for a CLI, script, or executable named after an agent tool - none exists, and running one only wastes a turn. Collaboration tools cannot be called from inside `functions.exec`; call them directly, using the recipient their definitions show (such as `to=functions.collaboration.spawn_agent`).\n\
\n\
1. Split the request into independent units of work and derive one worker per unit: a task name, the model slug when the user named one, and a complete self-contained task.\n\
2. Treat omissions as delegation authority, not as blockers. When the request is subjective, broad or underspecified, infer the useful roles, task decomposition, worker count, models, fan-out and depth from the request, the catalog, the concurrency slots and the token budget. Delegate immediately without asking the user to specify those parameters.\n\
3. Call `spawn_agent` once per worker, in parallel when their tasks are independent; chain them when one needs another's output. Whenever the user asked for a specific model, set `model` to the LoomRouter slug and pair it with `fork_turns` as described above.\n\
4. Give each worker a focused, self-contained task carrying the scope and the boundaries above. State the files, directories or question it covers; a worker that has to guess its scope will invent one.\n\
5. Track the workers instead of guessing. Use `list_agents` to see who is running, `wait_agent` to block until results arrive (prefer longer waits, minutes, over busy polling), `followup_task` to give a finished agent more work, and `interrupt_agent` to stop one.\n\
6. If the user requests a hierarchy, or an underspecified task materially benefits from one, tell each parent worker to repeat this routing procedure for its children, including the selected child model, rules, fan-out and remaining depth. Respect the concurrency slots and never promise unbounded or exponential simultaneous execution.\n\
7. Report completions in the chat as they arrive. For each finished subagent, emit a concise status containing the agent name, `completed` or `failed`, and a one-line result summary. Do not leave successful subagent work visible only in tool output.\n\
8. Wait for all reachable workers, then explicitly state that the delegation tree is complete and consolidate their results into one answer. If some workers failed or were blocked, name them and preserve the successful results.\n\
\n\
If `spawn_agent` rejects a LoomRouter slug outright, say so and name the slug it refused rather than silently downgrading the worker to a native model - the user asked for that model on purpose. Do not substitute thread tools such as `create_thread`, and do not quietly do the whole task yourself: the user asked for delegation, so a single-agent answer that does not mention the problem is a wrong answer.\n\
\n\
If the multi-agent tools are absent entirely, stop and say so, and point the user at `[features] multi_agent_v2 = true` plus re-applying the LoomRouter Codex integration.\n\
\n\
Refuse only when the multi-agent tools are unavailable, the requested model is not in the catalog, or the concurrency policy blocks another child; name that concrete constraint.\n";

fn orchestrator_skill_dir_in(codex_home: &std::path::Path) -> PathBuf {
    codex_home.join("skills").join("loom-orchestrator")
}

/// Write the orchestrator skill under `codex_home`. Idempotent: the text is
/// static, so this is a plain overwrite.
pub(super) fn sync_orchestrator_skill_in(codex_home: &std::path::Path) -> anyhow::Result<()> {
    let dir = orchestrator_skill_dir_in(codex_home);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("SKILL.md"), SKILL)?;
    Ok(())
}

/// Install the orchestrator skill into the real Codex home.
pub fn sync_orchestrator_skill() -> anyhow::Result<()> {
    sync_orchestrator_skill_in(&codex_home())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill_in_temp_home() -> String {
        let dir = tempfile::tempdir().unwrap();
        sync_orchestrator_skill_in(dir.path()).unwrap();
        std::fs::read_to_string(dir.path().join("skills/loom-orchestrator/SKILL.md")).unwrap()
    }

    #[test]
    fn skill_is_written_without_any_agent_roster() {
        let raw = skill_in_temp_home();
        assert!(raw.starts_with("---\nname: loom-orchestrator"));
        assert!(!raw.contains("## Available agents"));
        assert!(!raw.contains("Saved agents have priority"));
        assert!(raw.contains("there is no roster to consult"));
        assert!(raw.contains("Never ask the user to create an agent first"));
    }

    #[test]
    fn skill_description_triggers_on_natural_language_delegation() {
        let raw = skill_in_temp_home();
        let description = raw
            .lines()
            .find(|line| line.starts_with("description:"))
            .unwrap();
        for phrase in [
            "multiple agents",
            "subagents",
            "fan out",
            "use multi agents with deepseek to investigate the project",
        ] {
            assert!(description.contains(phrase), "missing trigger: {phrase}");
        }
    }

    #[test]
    fn skill_drives_the_native_multi_agent_tools() {
        let raw = skill_in_temp_home();
        for tool in [
            "spawn_agent",
            "followup_task",
            "wait_agent",
            "interrupt_agent",
            "list_agents",
        ] {
            assert!(raw.contains(tool), "native tool missing: {tool}");
        }
        // The whole point of going native: the user gets a child-agent thread
        // they can watch instead of one opaque blocking call.
        assert!(raw.contains("child-agent thread"));
        assert!(raw.contains("Track the workers instead of guessing"));
    }

    #[test]
    fn skill_pins_fork_turns_whenever_it_sets_a_model() {
        // Codex silently ignores a `model` override on a full-history fork,
        // which would run the worker on the parent's model while reporting
        // success. The skill has to pin `fork_turns` every time it says
        // `model`, and has to say why.
        let raw = skill_in_temp_home();
        assert!(raw.contains("these are skill instructions"));
        assert!(raw.contains("must also set `fork_turns`"));
        assert!(raw.contains("inherits the parent model and ignores the override"));
    }

    #[test]
    fn skill_prefers_a_context_carrying_fork_over_a_blind_one() {
        // Regression: pinning `fork_turns: "none"` starved a worker of every
        // fact about its own task. It went looking for the task definition on
        // the machine and ended up scanning for secrets. A small integer
        // satisfies the same override rule while handing over recent turns.
        let raw = skill_in_temp_home();
        assert!(raw.contains("Prefer a small positive integer string"));
        assert!(raw.contains("fork_turns: \"2\""));
        let prefer = raw.find("Prefer a small positive integer").unwrap();
        let none_case = raw.find("Use `fork_turns: \"none\"` only when").unwrap();
        assert!(
            prefer < none_case,
            "the context-carrying fork has to be presented as the default"
        );
        // The privacy cost of the integer form is stated, not hidden.
        assert!(raw.contains("often a third-party provider"));
    }

    #[test]
    fn skill_forbids_a_worker_from_hunting_its_own_instructions_or_secrets() {
        let raw = skill_in_temp_home();
        assert!(raw.contains("must say so and stop"));
        assert!(raw.contains("no reading session, rollout or transcript files"));
        assert!(raw.contains("no enumerating environment variables"));
        assert!(raw.contains("no reading, decrypting or reporting credentials"));
        assert!(raw.contains("a reason to stop, not a licence to explore"));
    }

    #[test]
    fn skill_carries_no_loomrouter_spawn_tool() {
        // Delegation goes through Codex's own tools now; a LoomRouter spawn
        // tool would only cost the user the child thread and its progress.
        let raw = skill_in_temp_home();
        assert!(!raw.contains("loom_spawn_agents"));
        assert!(!raw.contains("loom_list_subagent_models"));
        assert!(!raw.contains("MCP namespace"));
    }

    #[test]
    fn skill_refuses_to_silently_downgrade_a_requested_model() {
        let raw = skill_in_temp_home();
        assert!(raw.contains("name the slug it refused"));
        assert!(raw.contains("rather than silently downgrading"));
        assert!(raw.contains("Do not substitute a native GPT model"));
    }

    #[test]
    fn skill_delegates_underspecified_requests_without_questions() {
        let raw = skill_in_temp_home();
        for inferred in ["useful roles", "worker count", "models", "fan-out", "depth"] {
            assert!(
                raw.contains(inferred),
                "missing automatic choice: {inferred}"
            );
        }
        assert!(raw.contains("Delegate immediately without asking the user"));
        assert!(raw.contains("Treat omissions as delegation authority, not as blockers"));
    }

    #[test]
    fn skill_bounds_parallelism_by_the_sessions_own_slots() {
        let raw = skill_in_temp_home();
        assert!(raw.contains("concurrency_slots"));
        assert!(raw.contains("never promise more simultaneous agents than the session has slots"));
        // Native agents share one directory, so the old per-worker sandbox
        // advice would have been a lie.
        assert!(raw.contains("All agents share one working directory"));
        assert!(raw.contains("No two agents may edit the same file in the same wave"));
    }

    #[test]
    fn skill_reports_completions_and_final_tree_status() {
        let raw = skill_in_temp_home();
        assert!(raw.contains("For each finished subagent"));
        assert!(raw.contains("agent name, `completed` or `failed`"));
        assert!(raw.contains("Do not leave successful subagent work visible only in tool output"));
        assert!(raw.contains("explicitly state that the delegation tree is complete"));
        assert!(raw.contains("If some workers failed or were blocked, name them"));
        assert!(raw.contains("preserve the successful results"));
    }

    #[test]
    fn syncing_twice_leaves_a_single_stable_skill_file() {
        let dir = tempfile::tempdir().unwrap();
        sync_orchestrator_skill_in(dir.path()).unwrap();
        let first =
            std::fs::read_to_string(dir.path().join("skills/loom-orchestrator/SKILL.md")).unwrap();
        sync_orchestrator_skill_in(dir.path()).unwrap();
        let second =
            std::fs::read_to_string(dir.path().join("skills/loom-orchestrator/SKILL.md")).unwrap();
        assert_eq!(first, second);
    }
}
