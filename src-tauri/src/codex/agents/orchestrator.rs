use super::agents_list_in;
use crate::codex::codex_home;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Orchestrator skill (~/.codex/skills/loom-orchestrator/SKILL.md)
//
// Codex skills activate implicitly when the user's request matches the
// skill description (progressive disclosure: only name+description sit in
// context until then). This generated skill is the missing link between
// natural language ("use multi agents to review this") and explicit
// subagent delegation: it carries the *current* agent roster with
// delegation-ready descriptions, so the main model knows exactly which
// agents exist and when to spawn each one. Regenerated on every agent
// upsert/delete; removed when no custom agents remain.
// ---------------------------------------------------------------------------

fn orchestrator_skill_dir_in(codex_home: &std::path::Path) -> PathBuf {
    codex_home.join("skills").join("loom-orchestrator")
}

/// Rewrite the orchestrator skill from the current agent roster. With no
/// custom agents the skill is removed entirely — built-in agents need no
/// routing help.
pub(super) fn sync_orchestrator_skill_in(codex_home: &std::path::Path) -> anyhow::Result<()> {
    let dir = orchestrator_skill_dir_in(codex_home);
    let agents = agents_list_in(&codex_home.join("agents"))?;
    if agents.is_empty() {
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        return Ok(());
    }

    let mut roster = String::new();
    for a in &agents {
        let model = a
            .model
            .as_deref()
            .unwrap_or("inherits the current LoomRouter model");
        roster.push_str(&format!(
            "- **{}** (model: `{}`): {}\n",
            a.name,
            model,
            a.description.trim()
        ));
    }

    let skill = format!(
        "---\n\
         name: loom-orchestrator\n\
         description: \"Use when the user asks to run tasks with multiple agents, subagents or specialists, delegate or fan out work, or get parallel reviews — for example 'use multi agents to review this', 'spawn agents to check this', 'have specialists look at this'.\"\n\
         ---\n\
         \n\
         # LoomRouter Agent Orchestration\n\
         \n\
         The user has custom Codex subagents installed (managed by LoomRouter). When a request involves delegating, fanning out, or using multiple agents or specialists, act automatically. Always prefer saved LoomRouter agents when their descriptions fit the work. Create an ad hoc worker specification only for uncovered roles or when the user explicitly overrides the saved agent's model or rules. Do not ask the user to pre-create an agent.\n\
         \n\
         ## Available agents\n\
         \n\
         {roster}\n\
         ## Model routing\n\
         \n\
         The `model:` values above are LoomRouter slugs. Use them as the exact model for spawned agents. Do not replace them with Claude Code's built-in models or Codex native models. If an agent has `inherits the current LoomRouter model`, keep the current session's LoomRouter-routed model; do not switch to another model.\n\
         \n\
         A user-requested model is not limited to the saved roster. If the spawn tool accepts a free-form model, pass the requested LoomRouter slug exactly. If its schema exposes a closed model list, first use a saved agent whose configured model matches the request. If neither route can represent that model, report that the host tool rejected the model; never claim that LoomRouter itself lacks the model merely because it is absent from the roster.\n\
         \n\
         ## Operating rules (single injection)\n\
         \n\
         Keep this block as the single source of truth. Do not duplicate it in prompts.\n\
         \n\
         - Parallel budget: `P = min(task_width, hardware_budget, token_budget)`. Raise P only when the task graph has real parallel width.\n\
         - No two agents edit the same file in the same wave.\n\
         - Token budget: 60% workers, 25% orchestrator/synthesis, 15% retries/review.\n\
         \n\
         ## How to delegate\n\
         \n\
         Spawning is a tool call, never a shell command. Do NOT look for a CLI, script, or executable named after an agent tool — none exists, and running one only wastes a turn.\n\
         \n\
         The tool is normally `spawn_agent`. Depending on which multi-agent surface the session negotiated it can instead appear namespaced, such as `collaboration.spawn_agent`; use whichever one is actually in your tool list.\n\
         \n\
         If neither is there, stop and say so, and point the user at `[features] multi_agent_v2 = true` in `~/.codex/config.toml`. Do not substitute thread tools such as `create_thread`, and do not quietly do the whole task yourself — the user asked for delegation, so a single-agent answer that does not mention the tool was missing is a wrong answer.\n\
         \n\
         1. Map each part of the user's request to the saved LoomRouter agent whose description matches it best. Saved agents have priority over ad hoc workers unless the user explicitly requests a different model or rules.\n\
         2. Treat omissions as delegation authority, not as blockers. When the request is subjective, broad or underspecified, infer the useful roles, task decomposition, worker count, models, fan-out and depth from the request, available roster, concurrency and token budget. Delegate immediately without asking the user to specify those parameters.\n\
         3. When no saved specialist fits an inferred or explicit role, derive an ad hoc worker from the task and any user-supplied model or rules. Put those rules in the task message, and pass the selected model through the model field when the tool supports it.\n\
         4. Call the spawn tool once per agent, in parallel when their tasks are independent; chain them when one needs another's output. If the tool schema exposes an agent/role parameter, pass the agent's name there; otherwise name the agent at the start of the task message.\n\
         5. Give each spawned agent a focused, self-contained task — subagents start with a fresh context.\n\
         6. If the user requests a hierarchy, or if an underspecified task materially benefits from one, tell each parent worker to repeat this routing procedure for its children, including the selected child model, rules, fan-out and remaining depth. Respect the session's concurrency slots and never promise unbounded or exponential simultaneous execution.\n\
         7. Report completions in the chat as they arrive. For each finished subagent, emit a concise status containing the agent name, `completed` or `failed`, and a one-line result summary. Do not leave successful subagent work visible only in tool output.\n\
         8. Wait for all reachable workers, then explicitly state that the delegation tree is complete and consolidate their results into one answer. If some workers failed or were blocked, name them and preserve the successful results.\n\
         \n\
         An absent roster match is not a reason to refuse delegation. Refuse only when the actual spawn tool cannot express the requested model or when its concurrency/depth policy blocks another child, and name that concrete constraint.\n"
    );

    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("SKILL.md"), skill)?;
    Ok(())
}

/// Ensure the orchestrator skill reflects the current roster (e.g. after
/// the user edits TOML files by hand outside LoomRouter).
pub fn sync_orchestrator_skill() -> anyhow::Result<()> {
    sync_orchestrator_skill_in(&codex_home())
}
