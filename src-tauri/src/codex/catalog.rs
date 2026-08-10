use super::{codex_bin, codex_home};
use crate::config::AppConfig;
use serde_json::{json, Map, Value};
use std::path::PathBuf;

pub(super) fn loom_dir() -> PathBuf {
    codex_home().join("loom-router")
}

pub(super) fn merged_catalog_path() -> PathBuf {
    loom_dir().join("merged-models.json")
}

pub(super) fn native_catalog_path() -> PathBuf {
    loom_dir().join("native-models.json")
}

/// Capture the native catalog from the Codex CLI (`codex debug models`,
/// falling back to `--bundled`). Returns the parsed `{models: [...]}`.
/// `exclude_slugs` lists additional slugs to drop (besides the built-in
/// `provider/model` filter): in native slug mode our republished bare slugs
/// echo back through `debug models` and must not pollute the next capture.
pub fn capture_native_catalog(
    exclude_slugs: &std::collections::HashSet<String>,
) -> anyhow::Result<Value> {
    let bin = codex_bin().ok_or_else(|| {
        anyhow::anyhow!("Codex CLI not found on PATH (set CODEX_BIN to its location)")
    })?;
    let run = |extra: &str| -> anyhow::Result<String> {
        let mut command = std::process::Command::new(&bin);
        crate::cli_locator::hide_console_window(&mut command);
        let out = command
            .args(["debug", "models"])
            .args(if extra.is_empty() {
                vec![]
            } else {
                vec![extra]
            })
            .output()?;
        if !out.status.success() {
            anyhow::bail!(
                "codex debug models failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(String::from_utf8(out.stdout)?)
    };
    let raw = run("").or_else(|_| run("--bundled"))?;
    let parsed: Value = serde_json::from_str(&raw)?;
    let models: Vec<Value> = parsed
        .get("models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // When the managed block is active, `debug models` echoes our own merged
    // catalog back. Routed slugs always look like `provider/model`; native
    // OpenAI slugs never contain '/'. Drop them so stale routed entries can
    // never pile up as duplicates in the next merge. Native slug mode
    // publishes bare slugs instead, so the caller passes those explicitly.
    let models: Vec<Value> = models
        .into_iter()
        .filter(|m| {
            m.get("slug")
                .and_then(Value::as_str)
                .map(|s| !s.contains('/') && !exclude_slugs.contains(s))
                .unwrap_or(true)
        })
        .collect();
    if models.is_empty() {
        anyhow::bail!("Codex returned an empty or invalid model catalog");
    }
    let mut catalog = json!({ "models": models });
    ensure_native_catalog_backfills(&mut catalog);
    std::fs::create_dir_all(loom_dir())?;
    std::fs::write(
        native_catalog_path(),
        serde_json::to_string_pretty(&catalog)?,
    )?;
    Ok(catalog)
}

pub(super) fn load_native_catalog() -> Value {
    let mut catalog = std::fs::read_to_string(native_catalog_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({ "models": [] }));
    ensure_native_catalog_backfills(&mut catalog);
    catalog
}

/// Keep a release-known native entry available when an older or sandboxed
/// Codex CLI omits it from `debug models`. Clone Terra's real schema instead
/// of inventing one, so the picker gets the same contract Codex expects.
pub(super) fn ensure_native_catalog_backfills(catalog: &mut Value) {
    let Some(models) = catalog.get_mut("models").and_then(Value::as_array_mut) else {
        return;
    };
    if models
        .iter()
        .any(|model| model.get("slug").and_then(Value::as_str) == Some("gpt-5.6-sol"))
    {
        return;
    }
    let Some(mut sol) = models
        .iter()
        .find(|model| model.get("slug").and_then(Value::as_str) == Some("gpt-5.6-terra"))
        .cloned()
    else {
        return;
    };
    sol["slug"] = json!("gpt-5.6-sol");
    sol["display_name"] = json!("GPT-5.6-Sol");
    sol["priority"] = json!(4);
    models.push(sol);
}

/// Conservative fallback context window (tokens) for providers without an
/// explicit `context_window` override. Under-estimating is safe — the agent
/// just compacts earlier — while over-estimating makes Codex plan turns
/// against a window the model does not have.
pub(super) const DEFAULT_CONTEXT_WINDOW: i64 = 131_072;

/// The context window LoomRouter publishes for one model, and where the
/// number came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct ContextWindow {
    /// Tokens.
    pub window: i64,
    /// False when `window` is only the conservative fallback, i.e. nothing
    /// is actually known about this model. The UI must not present a guess
    /// as if it were the model's published limit.
    pub known: bool,
}

/// Context window (tokens) for a model, and whether it is a real value.
///
/// Single source of truth. This is the number written into Codex's catalog,
/// so anything that displays a limit has to read it from here — a second
/// copy of the heuristic would drift and show the user a window Codex was
/// never told about.
///
/// Precedence: a per-model value learned during discovery (or hand-set in
/// the config) wins over everything; then the Kimi name heuristic (K3 = 1M
/// tokens; 256k-class = 256k), which applies only to Kimi-family providers:
/// applying it to e.g. claude-sonnet-5 or grok-4.5 would publish a window
/// those models do not have. Everything else uses the provider's explicit
/// override when configured, and otherwise falls back — under-estimating is
/// safe, since the agent just compacts earlier, while over-estimating makes
/// Codex plan turns against a window it does not have.
pub fn context_window_for(provider: &crate::config::Provider, model_id: &str) -> ContextWindow {
    if let Some(w) = provider
        .models
        .iter()
        .find(|m| m.id == model_id)
        .and_then(|m| m.context_window)
    {
        return ContextWindow {
            window: i64::from(w),
            known: true,
        };
    }
    match crate::proxy::family_of(provider) {
        crate::proxy::ProviderFamily::Kimi => {
            let window = if model_id.contains("256k") {
                262_144
            } else if model_id.contains("k3") {
                1_000_000
            } else {
                262_144
            };
            ContextWindow {
                window,
                known: true,
            }
        }
        _ => match provider.context_window {
            Some(w) => ContextWindow {
                window: i64::from(w),
                known: true,
            },
            None => ContextWindow {
                window: DEFAULT_CONTEXT_WINDOW,
                known: false,
            },
        },
    }
}

/// Field overrides applied to a cloned native template for each external
/// model. Mirrors the schema Codex emits for its own models.
///
/// `native_slug_mode` selects the published slug: routed mode uses
/// `provider/model` (unambiguous next to native GPT models); native slug
/// mode uses the bare model id so entries look and resolve like native
/// ones (see module docs).
fn routed_model(
    template: &Value,
    provider: &crate::config::Provider,
    model_id: &str,
    label: Option<&str>,
    priority: i64,
    native_slug_mode: bool,
    supports_image_input: bool,
) -> Value {
    let mut m: Map<String, Value> = template.as_object().cloned().unwrap_or_default();
    let slug = if native_slug_mode {
        model_id.to_string()
    } else {
        format!("{}/{}", provider.id, model_id)
    };
    m.insert("slug".into(), json!(slug));
    // The cloned template's system prompt says "based on GPT-5", which
    // makes external models introduce themselves as GPT-5. Rewrite the
    // identity line to be model-neutral.
    if let Some(instructions) = m
        .get("base_instructions")
        .and_then(Value::as_str)
        .map(str::to_string)
    {
        let patched = instructions.replace("an agent based on GPT-5", "a coding agent");
        m.insert("base_instructions".into(), json!(patched));
    }
    m.insert(
        "display_name".into(),
        json!(label.unwrap_or(model_id).to_string()),
    );
    m.insert(
        "description".into(),
        json!(format!(
            "{} via LoomRouter ({}){}",
            model_id,
            provider.id,
            if provider.id == crate::providers::CLAUDE_CODE_PROVIDER_ID
                && crate::providers::claude_code_fast_mode(model_id)
            {
                " · fast mode"
            } else {
                ""
            }
        )),
    );
    m.insert("priority".into(), json!(priority));
    m.insert("visibility".into(), json!("list"));
    m.insert("supported_in_api".into(), json!(true));
    // Reasoning levels shown in Codex's effort picker. Codex only renders
    // the canonical set (low/medium/high/xhigh); other values are hidden.
    m.insert(
        "supported_reasoning_levels".into(),
        json!([
            {"effort": "low", "description": "Fast responses with lighter reasoning"},
            {"effort": "medium", "description": "Balances speed and reasoning depth for everyday tasks"},
            {"effort": "high", "description": "Greater reasoning depth for complex problems"},
            {"effort": "xhigh", "description": "Maximum reasoning depth"}
        ]),
    );
    m.insert("default_reasoning_level".into(), json!("high"));
    let window = context_window_for(provider, model_id).window;
    m.insert("context_window".into(), json!(window));
    m.insert("max_context_window".into(), json!(window));
    m.insert("effective_context_window_percent".into(), json!(95));
    m.insert(
        "input_modalities".into(),
        if supports_image_input {
            json!(["text", "image"])
        } else {
            json!(["text"])
        },
    );
    m.insert("additional_speed_tiers".into(), json!([]));
    m.insert("service_tiers".into(), json!([]));
    m.insert("availability_nux".into(), Value::Null);
    m.insert("upgrade".into(), Value::Null);
    m.insert("supports_reasoning_summaries".into(), json!(true));
    m.insert("default_reasoning_summary".into(), json!("auto"));
    m.insert("support_verbosity".into(), json!(false));
    m.insert("default_verbosity".into(), Value::Null);
    // Deferred tool loading. With this on, Codex stops inlining every tool
    // definition in every request and advertises only `tool_search`; the
    // model searches (BM25 runs client-side in Codex) and the discovered
    // specs arrive in a `tool_search_output` item on the next request, where
    // translate.rs activates them into the Chat tool list. Requires
    // namespace_tools, which custom providers already get.
    m.insert("supports_search_tool".into(), json!(true));
    m.insert("supports_image_detail_original".into(), json!(false));
    m.insert("use_responses_lite".into(), json!(false));
    // This field decides which multi-agent tool surface Codex builds for the
    // model, and "v1" was the wrong side of that fork.
    //
    // Codex resolves the version as
    // `multi_agent_version_override().or(model_multi_agent_version)`, so the
    // value written here is what a routed model gets unless the user sets
    // `[features] multi_agent_v2`. The two versions then register the spawn
    // tool under different names: v1 as `ToolName::namespaced(
    // MULTI_AGENT_V1_NAMESPACE, "spawn_agent")` — the "collaboration"
    // namespace — and v2 as `ToolName::plain("spawn_agent")`.
    //
    // The orchestrator skill below tells the model to call `spawn_agent`.
    // Under v1 no tool by that name exists, only `collaboration.spawn_agent`,
    // so the model reported having no such tool, tried `spawn_agent --help`
    // as a shell command, and fell back to doing the whole task itself.
    //
    // Native entries in the catalog already ship "v2" (gpt-5.6-terra), so
    // matching it here puts routed models on the same surface the skill and
    // the rest of the ecosystem assume.
    m.insert("multi_agent_version".into(), json!("v2"));
    Value::Object(m)
}

/// Build the merged catalog. Routed mode: every native model (so GPT stays
/// in the picker) plus one entry per enabled external model cloned from a
/// native template. Native slug mode: external entries only, published
/// under bare slugs — native GPT models require the ChatGPT login this mode
/// exists to avoid (see module docs).
pub fn build_merged_catalog(config: &AppConfig, native: &Value) -> Value {
    let native_slug_mode = config.native_slug_mode;
    let native_models: Vec<Value> = native
        .get("models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let template = native_models
        .iter()
        .find(|m| m.get("slug").and_then(Value::as_str) == Some("gpt-5.5"))
        .or_else(|| {
            native_models
                .iter()
                .find(|m| m.get("visibility").and_then(Value::as_str) == Some("list"))
        })
        .or_else(|| native_models.first())
        .cloned()
        .unwrap_or_else(|| json!({}));

    let mut models = if native_slug_mode {
        Vec::new()
    } else {
        native_models
    };
    // External entries start after native priorities.
    let mut priority = 100_i64;
    let bridge_supports_images = crate::visual::has_valid_configuration(config);
    for p in config.providers.values().filter(|p| p.enabled) {
        for m in p.models.iter().filter(|m| m.enabled) {
            models.push(routed_model(
                &template,
                p,
                &m.id,
                m.label.as_deref(),
                priority,
                native_slug_mode,
                m.supports_vision || bridge_supports_images,
            ));
            priority += 1;
        }
    }

    // Dedupe by slug. In routed mode, routed entries win over any stale
    // native-copy entry (reverse iteration keeps the last of each slug).
    // In native slug mode, two providers serving the same bare model id
    // collide; the first provider in config order (BTreeMap) wins.
    let mut seen = std::collections::HashSet::new();
    let mut deduped: Vec<Value> = Vec::with_capacity(models.len());
    if native_slug_mode {
        for m in models.into_iter() {
            let slug = m
                .get("slug")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if seen.insert(slug) {
                deduped.push(m);
            }
        }
    } else {
        for m in models.into_iter().rev() {
            let slug = m
                .get("slug")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if seen.insert(slug) {
                deduped.push(m);
            }
        }
        deduped.reverse();
    }
    let mut models = deduped;

    models.sort_by_key(|m| m.get("priority").and_then(Value::as_i64).unwrap_or(999));
    json!({ "models": models })
}

#[cfg(test)]
#[path = "catalog/tests.rs"]
mod tests;
