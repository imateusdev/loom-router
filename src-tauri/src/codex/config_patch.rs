use super::{
    build_merged_catalog, capture_native_catalog, codex_bin, codex_home, load_native_catalog,
    loom_dir, merged_catalog_path,
};
use crate::config::AppConfig;
use serde_json::Value;

pub const BEGIN_MARK: &str = "# BEGIN loom-router-managed";
pub const END_MARK: &str = "# END loom-router-managed";

/// Apply the integration: refresh native catalog (best effort), write the
/// merged catalog, and install the managed config block.
pub fn apply(config: &AppConfig, port: u16) -> anyhow::Result<()> {
    std::fs::create_dir_all(loom_dir())?;

    // Refresh the native catalog when the CLI is available; otherwise reuse
    // the previous capture (or an empty set with a warning). In native slug
    // mode our own bare slugs echo back through `debug models`, so exclude
    // every enabled external model id from the capture.
    let exclude: std::collections::HashSet<String> = if config.native_slug_mode {
        config
            .providers
            .values()
            .filter(|p| p.enabled)
            .flat_map(|p| p.models.iter().filter(|m| m.enabled).map(|m| m.id.clone()))
            .collect()
    } else {
        std::collections::HashSet::new()
    };
    if codex_bin().is_some() {
        if let Err(e) = capture_native_catalog(&exclude) {
            tracing::warn!("native catalog capture failed, reusing previous: {e}");
        }
    } else {
        tracing::warn!("Codex CLI not found; merged catalog will only include external models");
    }
    let native = load_native_catalog();

    let catalog = build_merged_catalog(config, &native);
    let model_count = catalog
        .get("models")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    if model_count == 0 {
        // Codex refuses to load config.toml when `model_catalog_json` has no
        // models, which breaks the whole app (threads cannot even be
        // resumed). Never leave Codex in that state: roll back any managed
        // block we previously wrote and fail loudly instead.
        // `Some(config)`: apply() is re-entrant, so an earlier successful
        // run may well have published the root `model` key. Rolling back
        // with `None` left it pointing at a slug that no catalog contains.
        let _ = remove(Some(config));
        anyhow::bail!(
            "no models to publish: enable at least one provider model before \
             applying the Codex integration"
        );
    }
    let catalog_path = merged_catalog_path();
    std::fs::write(&catalog_path, serde_json::to_string_pretty(&catalog)?)?;

    let block = managed_block(
        port,
        &catalog_path.display().to_string().replace('\\', "/"),
        config.native_slug_mode,
    );

    let cfg_path = codex_home().join("config.toml");
    let raw = std::fs::read_to_string(&cfg_path).unwrap_or_default();
    let stripped = strip_managed_block(&raw)?;
    // Pre-marker installs left the provider block unmarked; re-applying on
    // top of one duplicates the owned root keys, so migrate it away first.
    let stripped = strip_legacy_install(&stripped);
    // The active model is a plain root key, so it has to be reconciled on
    // the *stripped* text: writing it inside the managed block would collide
    // with a `model` the user already has at the root, and a duplicated key
    // makes Codex reject the whole config.toml.
    let stripped = reconcile_root_model(&stripped, config);
    let out = insert_root_block(&stripped, &block);
    // Last line of defence: a config.toml Codex cannot parse breaks the app
    // completely (threads cannot even be resumed), so never write one — a
    // shape we failed to anticipate stops here instead of at Codex startup.
    if let Err(e) = toml::from_str::<toml::Value>(&out) {
        anyhow::bail!("refusing to write an invalid Codex config.toml: {e}");
    }
    write_config_atomic(&cfg_path, &out)?;
    Ok(())
}

/// Decide what the root `model` key should say, given what LoomRouter has
/// selected and what was there before it.
///
/// The rule is ownership: LoomRouter only ever rewrites a value it put
/// there itself. A `model` the user wrote by hand is restored, not deleted
/// — the previous version of this deleted it on the first auto-apply.
fn reconcile_root_model(stripped: &str, config: &AppConfig) -> String {
    if let Some(slug) = active_slug(config) {
        return set_root_model_key(stripped, Some(&slug));
    }
    match root_model_key(stripped) {
        // Nothing selected and the key is ours: hand it back to whatever
        // Codex used before us, or to Codex's own default.
        Some(current) if owns_slug(config, &current) => {
            set_root_model_key(stripped, config.codex_model_backup.as_deref())
        }
        // Someone else's model (or none at all): leave it alone.
        _ => stripped.to_string(),
    }
}

/// Whether a root `model` value is one LoomRouter published. Both slug
/// shapes are accepted regardless of the current mode, because
/// `native_slug_mode` may have been flipped since the key was written.
pub fn owns_slug(config: &AppConfig, value: &str) -> bool {
    config.providers.iter().any(|(provider_id, provider)| {
        provider
            .models
            .iter()
            .any(|m| value == format!("{provider_id}/{}", m.id) || value == m.id)
    })
}

/// The root `model` currently in Codex's config, ignoring anything inside
/// our managed block. Used to remember a user's own model before replacing
/// it for the first time.
pub fn current_root_model() -> Option<String> {
    let raw = std::fs::read_to_string(codex_home().join("config.toml")).ok()?;
    let stripped = strip_managed_block(&raw).ok()?;
    root_model_key(&stripped)
}

/// The slug a model is published under: `provider/model` in routed mode,
/// the bare model id in native slug mode. Single source of truth for the
/// merged catalog, the tray menu and the root `model` key.
pub fn published_slug(provider_id: &str, model_id: &str, native_slug_mode: bool) -> String {
    if native_slug_mode {
        model_id.to_string()
    } else {
        format!("{provider_id}/{model_id}")
    }
}

/// Resolve `config.active_model` ("provider/model") to the slug that is
/// actually in the published catalog. Returns `None` when nothing is
/// selected or the selection no longer exists / is disabled — a stale
/// pointer must not be written to Codex, which would show a model it
/// cannot route.
pub fn active_slug(config: &AppConfig) -> Option<String> {
    let active = config.active_model.as_deref()?;
    let (provider_id, model_id) = active.split_once('/')?;
    let provider = config.providers.get(provider_id)?;
    if !provider.enabled {
        return None;
    }
    if !provider
        .models
        .iter()
        .any(|m| m.enabled && m.id == model_id)
    {
        return None;
    }
    Some(published_slug(
        provider_id,
        model_id,
        config.native_slug_mode,
    ))
}

/// Set (or clear) the root `model` key of a `config.toml` whose managed
/// block has already been stripped.
///
/// Textual patch rather than a TOML round-trip, for the same reason
/// `set_multi_agent_in` uses one: re-serializing would drop the user's
/// comments and our own BEGIN/END markers. Root keys must precede the first
/// `[table]`, so the search stops there.
/// Does this line assign the root `model` key?
///
/// `model_provider`, `model_catalog_json` and `model_reasoning_effort` all
/// share the prefix, so the match is on the key boundary.
fn is_root_model_line(line: &str) -> bool {
    is_root_assignment(line, "model")
}

/// Does this line assign the given key? TOML also allows the key to be
/// quoted (`"key" = …`, `'key' = …`), and missing that form would leave the
/// old assignment in place next to a new one — a duplicate key, which makes
/// Codex reject the whole file.
fn is_root_assignment(line: &str, key: &str) -> bool {
    let t = line.trim_start();
    for form in [key.to_string(), format!("\"{key}\""), format!("'{key}'")] {
        if let Some(rest) = t.strip_prefix(&form) {
            if rest.trim_start().starts_with('=') {
                return true;
            }
        }
    }
    false
}

fn set_root_model_key(stripped: &str, slug: Option<&str>) -> String {
    let nl = detect_newline(stripped);
    let mut lines: Vec<String> = stripped.lines().map(str::to_string).collect();
    let first_table = lines
        .iter()
        .position(|l| {
            let t = l.trim_start();
            t.starts_with('[') && !t.starts_with('#')
        })
        .unwrap_or(lines.len());
    let existing = lines[..first_table]
        .iter()
        .position(|l| is_root_model_line(l));

    match (slug, existing) {
        (Some(slug), Some(idx)) => lines[idx] = format!("model = \"{}\"", escape_toml(slug)),
        (Some(slug), None) => lines.insert(0, format!("model = \"{}\"", escape_toml(slug))),
        (None, Some(idx)) => {
            lines.remove(idx);
        }
        (None, None) => {}
    }

    let mut out = lines.join(nl);
    if !out.is_empty() {
        out.push_str(nl);
    }
    out
}

/// Read the current root `model` value of a stripped `config.toml`.
fn root_model_key(stripped: &str) -> Option<String> {
    let first_table = stripped
        .lines()
        .position(|l| {
            let t = l.trim_start();
            t.starts_with('[') && !t.starts_with('#')
        })
        .unwrap_or(usize::MAX);
    stripped
        .lines()
        .take(first_table)
        .filter(|l| is_root_model_line(l))
        .find_map(|l| {
            let value = l.split_once('=')?.1;
            // Strip a trailing comment before unquoting: `model = "x" # note`
            // must read as `x`, not as `x" # note`.
            let value = value.trim();
            let quoted = value.strip_prefix('"')?;
            let (inner, _) = quoted.split_once('"')?;
            Some(inner.to_string())
        })
}

fn escape_toml(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Write the Codex `config.toml` atomically, keeping a `.bak` of the
/// previous contents.
///
/// This file carries the local proxy token inside the managed block, so it
/// is written owner-only. It used to be created with a plain `fs::write`
/// (0644 on Unix), which handed any local process the token — and with it
/// the ability to spend the stored API keys through the proxy. The write
/// now shares one implementation with the credential config; see
/// `secure_fs`.
pub(super) fn write_config_atomic(path: &std::path::Path, contents: &str) -> anyhow::Result<()> {
    crate::secure_fs::write_private_with_backup(path, contents.as_bytes())?;
    Ok(())
}

/// The managed config block. We pin an explicit provider and point
/// `model_provider` at it; `supports_websockets = true` advertises the
/// Responses-over-WS transport the proxy implements (Codex v2 protocol),
/// with plain HTTP/SSE as the fallback.
///
/// `requires_openai_auth` is the login gate (Codex source:
/// `model-provider-info`). Routed mode keeps it `true` so ChatGPT login
/// stays available and native GPT models keep working through the
/// passthrough. Native slug mode sets it `false`: Codex then authenticates
/// only with the static `http_headers` below — the local proxy bearer
/// token — and never asks for an OpenAI login.
///
/// The proxy requires a local bearer token (generated at startup); Codex
/// authenticates with it through the provider's `http_headers`.
fn managed_block(port: u16, catalog_path: &str, native_slug_mode: bool) -> String {
    // The token is generated by us (hex), but escape defensively anyway so
    // the block can never become invalid TOML.
    let token = crate::proxy::local_token()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!(
        "{BEGIN_MARK}\n\
         model_provider = \"loomrouter\"\n\
         openai_base_url = \"http://127.0.0.1:{port}/v1\"\n\
         model_catalog_json = \"{catalog_path}\"\n\
         \n\
         [model_providers.loomrouter]\n\
         name = \"OpenAI\"\n\
         base_url = \"http://127.0.0.1:{port}/v1\"\n\
         wire_api = \"responses\"\n\
         requires_openai_auth = {requires_openai_auth}\n\
         supports_websockets = true\n\
         http_headers = {{ \"x-loomrouter-token\" = \"{token}\", \"Authorization\" = \"Bearer {token}\" }}\n\
         {END_MARK}",
        requires_openai_auth = !native_slug_mode,
    )
}

/// TOML root keys must appear before the first `[table]` header; appending
/// at EOF would nest them under the last table. Insert the managed block
/// right before the first table header, or at the end if there are none.
/// Preserves the file's original line endings (CRLF on Windows).
fn insert_root_block(raw: &str, block: &str) -> String {
    let nl = detect_newline(raw);
    let block = if nl == "\r\n" {
        block.replace('\n', "\r\n")
    } else {
        block.to_string()
    };
    let first_table = raw
        .lines()
        .enumerate()
        .find(|(_, l)| {
            let t = l.trim_start();
            t.starts_with('[') && !t.starts_with("#")
        })
        .map(|(i, _)| i);

    let mut out = String::new();
    match first_table {
        Some(idx) => {
            let mut lines: Vec<&str> = raw.lines().collect();
            // Trim trailing blank lines of the root section.
            let mut insert_at = idx;
            while insert_at > 0 && lines[insert_at - 1].trim().is_empty() {
                insert_at -= 1;
            }
            lines.insert(insert_at, &block);
            out.push_str(&lines.join(nl));
            out.push_str(nl);
        }
        None => {
            out.push_str(raw.trim_end());
            if !out.is_empty() {
                out.push_str(nl);
                out.push_str(nl);
            }
            out.push_str(&block);
            out.push_str(nl);
        }
    }
    out
}

/// Remove the integration: delete managed block and merged catalog.
///
/// `config` is what lets this tell our own root `model` from the user's:
/// a slug we published is replaced by whatever Codex used before us
/// (`codex_model_backup`), and anything else is left untouched. Passing
/// `None` skips the key entirely — used by the rollback inside `apply`,
/// where nothing of ours has been published yet.
pub fn remove(config: Option<&AppConfig>) -> anyhow::Result<()> {
    let cfg_path = codex_home().join("config.toml");
    if let Ok(raw) = std::fs::read_to_string(&cfg_path) {
        let stripped = strip_managed_block(&raw)?;
        // A legacy unmarked install is ours too: it leaves with us.
        let stripped = strip_legacy_install(&stripped);
        let restored = match (config, root_model_key(&stripped)) {
            (Some(cfg), Some(current)) if owns_slug(cfg, &current) => {
                set_root_model_key(&stripped, cfg.codex_model_backup.as_deref())
            }
            _ => stripped,
        };
        write_config_atomic(&cfg_path, &restored)?;
    }
    for path in [merged_catalog_path()] {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Multi-agent feature flag ([features] multi_agent in config.toml)
//
// Subagent spawning requires the Codex multi-agent feature. The flag lives
// in the user's own config, outside the LoomRouter managed block, so we
// patch it textually: rewriting the file through a TOML serializer would
// drop every comment (including our BEGIN/END markers).
// ---------------------------------------------------------------------------

/// The two flags this toggle owns, and only one of them does what the app
/// needs. `multi_agent` is Codex's `Feature::Collab`: it selects the v1
/// surface, where the spawn tool is registered as
/// `collaboration.spawn_agent`. `multi_agent_v2` is `Feature::MultiAgentV2`
/// and is the only thing that produces the plain `spawn_agent` name the
/// orchestrator skill tells the model to call.
///
/// Codex resolves the version as
/// `multi_agent_version_override().or(model_multi_agent_version)`, and the
/// override reads `multi_agent_v2` — so it wins over whatever the merged
/// catalog declares. Writing only `multi_agent` gave users a toggle that
/// reported success while the tool never appeared in any session.
const MULTI_AGENT_KEYS: [&str; 2] = ["multi_agent", "multi_agent_v2"];

/// Whether Codex will expose the spawn tool under the name the orchestrator
/// skill uses. Keyed on `multi_agent_v2`: `multi_agent` alone leaves the
/// model with a namespaced tool the skill never asks for.
pub fn multi_agent_enabled() -> bool {
    multi_agent_enabled_in(&codex_home().join("config.toml"))
}

fn multi_agent_enabled_in(cfg_path: &std::path::Path) -> bool {
    std::fs::read_to_string(cfg_path)
        .ok()
        .and_then(|raw| toml::from_str::<toml::Value>(&raw).ok())
        .and_then(|v| v.get("features")?.get("multi_agent_v2")?.as_bool())
        .unwrap_or(false)
}

/// Set both multi-agent flags without disturbing anything else in the
/// file. Returns the new state.
pub fn set_multi_agent(enabled: bool) -> anyhow::Result<bool> {
    set_multi_agent_in(&codex_home().join("config.toml"), enabled)
}

/// True when `line` assigns exactly `key`. Prefix matching is wrong here:
/// `multi_agent` is a prefix of `multi_agent_v2`, so `starts_with` rewrites
/// whichever of the two appears first and silently destroys the other.
fn assigns_key(line: &str, key: &str) -> bool {
    line.trim_start()
        .strip_prefix(key)
        .map(|rest| rest.trim_start().starts_with('='))
        .unwrap_or(false)
}

fn set_multi_agent_in(cfg_path: &std::path::Path, enabled: bool) -> anyhow::Result<bool> {
    let raw = std::fs::read_to_string(cfg_path).unwrap_or_default();
    let nl = detect_newline(&raw);
    let value = if enabled { "true" } else { "false" };

    let mut lines: Vec<String> = raw.lines().map(str::to_string).collect();
    // Find the [features] table header and its body span (up to the next
    // table header or EOF).
    let header_idx = lines.iter().position(|l| l.trim() == "[features]");
    match header_idx {
        Some(h) => {
            for key in MULTI_AGENT_KEYS {
                // Recomputed per key: inserting a missing one shifts every
                // index after the header.
                let body_end = lines[h + 1..]
                    .iter()
                    .position(|l| {
                        let t = l.trim_start();
                        t.starts_with('[') && !t.starts_with('#')
                    })
                    .map(|p| h + 1 + p)
                    .unwrap_or(lines.len());
                let key_idx = lines[h + 1..body_end]
                    .iter()
                    .position(|l| assigns_key(l, key))
                    .map(|p| h + 1 + p);
                match key_idx {
                    Some(k) => lines[k] = format!("{key} = {value}"),
                    None => lines.insert(h + 1, format!("{key} = {value}")),
                }
            }
        }
        None => {
            if !lines.last().map(|l| l.trim().is_empty()).unwrap_or(true) {
                lines.push(String::new());
            }
            lines.push("[features]".into());
            for key in MULTI_AGENT_KEYS {
                lines.push(format!("{key} = {value}"));
            }
        }
    }
    let mut out = lines.join(nl);
    out.push_str(nl);
    write_config_atomic(cfg_path, &out)?;
    Ok(enabled)
}

/// Dominant line ending of the original file, so rewrites never flip the
/// user's config from CRLF to LF (Windows).
fn detect_newline(raw: &str) -> &'static str {
    if raw.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

/// Remove the managed block. Line endings of the surviving content are
/// preserved.
///
/// A BEGIN marker without a matching END is what an external rewrite of
/// config.toml leaves behind (the Codex desktop app re-serializes the file
/// and drops the trailing comment). Bricking apply/remove over that would
/// strand the user, so we recover by ownership: drop the orphan marker and
/// strip exactly what we can prove is ours — the owned root keys and the
/// `[model_providers.loomrouter]` table (see `recover_orphan_managed_block`).
/// Content that is not ours is never touched.
fn strip_managed_block(raw: &str) -> anyhow::Result<String> {
    let nl = detect_newline(raw);
    let mut out = String::new();
    let mut inside = false;
    let mut saw_begin = false;
    let mut saw_end = false;
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed == BEGIN_MARK {
            inside = true;
            saw_begin = true;
            continue;
        }
        if trimmed == END_MARK {
            inside = false;
            saw_end = true;
            continue;
        }
        if !inside {
            out.push_str(line);
            out.push_str(nl);
        }
    }
    if saw_begin && !saw_end {
        return recover_orphan_managed_block(raw, nl);
    }
    Ok(out)
}

/// Whether a `config.toml` line is the `[model_providers.loomrouter]` table
/// header (or one of its sub-tables).
fn is_loomrouter_table(line: &str) -> bool {
    let t = line.trim();
    let Some(inner) = t.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
        return false;
    };
    let inner = inner.trim();
    inner == "model_providers.loomrouter" || inner.starts_with("model_providers.loomrouter.")
}

/// Whether a `config.toml` carries content LoomRouter demonstrably owns: a
/// `loomrouter` provider table, or a root `model_provider` pointing at it.
fn has_loomrouter_content(raw: &str) -> bool {
    raw.lines().any(|l| {
        is_loomrouter_table(l)
            || (is_root_assignment(l, "model_provider")
                && l.split_once('=')
                    .is_some_and(|(_, v)| v.contains("loomrouter")))
    })
}

/// Recover from a `# BEGIN` marker with no matching `# END`.
///
/// The orphan is the signature of an external rewrite (e.g. the Codex
/// desktop app round-tripping config.toml and dropping the trailing
/// comment), not necessarily an interrupted write. Removing the marker and
/// then stripping by ownership is safe: only the owned root keys and the
/// `[model_providers.loomrouter]` table are deleted, and anything that is
/// not ours — marketplaces, plugins, hooks, mcp_servers — survives intact.
/// When nothing of ours is present the marker is genuinely ambiguous, and
/// we keep the defensive refusal instead of guessing.
fn recover_orphan_managed_block(raw: &str, nl: &str) -> anyhow::Result<String> {
    let without_marker: String = raw
        .lines()
        .filter(|l| l.trim() != BEGIN_MARK)
        .collect::<Vec<_>>()
        .join(nl);
    if !has_loomrouter_content(&without_marker) {
        anyhow::bail!(
            "config.toml has a loom-router BEGIN marker without a matching END \
             and no loom-router-managed content to remove; refusing to modify \
             the file. Restore it from config.toml.bak or remove the marker \
             manually."
        );
    }
    Ok(strip_legacy_install(&without_marker))
}

/// Root keys the integration owns. The managed block re-declares all of
/// them, so any copy left outside the markers is a duplicate key in the
/// making.
const OWNED_ROOT_KEYS: [&str; 3] = ["model_provider", "openai_base_url", "model_catalog_json"];

/// Strip a legacy, unmarked LoomRouter install.
///
/// Versions before the BEGIN/END markers wrote the provider block straight
/// into `config.toml`: the owned root keys plus a
/// `[model_providers.loomrouter]` table (and its `.http_headers` sub-table).
/// `strip_managed_block` cannot see that shape, so applying on top of it
/// duplicated `model_provider` at the root and the parse check refused
/// every write — the integration could never apply again.
///
/// Detection is by ownership: a `loomrouter` provider table, or a root
/// `model_provider` pointing at it. A user's own `model_provider` naming
/// any other provider, and profile-level keys, are left untouched.
fn strip_legacy_install(stripped: &str) -> String {
    if !has_loomrouter_content(stripped) {
        return stripped.to_string();
    }

    let nl = detect_newline(stripped);
    let mut out: Vec<&str> = Vec::new();
    let mut seen_table = false;
    let mut in_legacy_table = false;
    for line in stripped.lines() {
        let t = line.trim_start();
        if t.starts_with('[') {
            seen_table = true;
            in_legacy_table = is_loomrouter_table(line);
            if in_legacy_table {
                continue;
            }
        // One condition, because both drop the line: inside the legacy
        // table, or a root key we own from before there was a table.
        } else if in_legacy_table
            || (!seen_table && OWNED_ROOT_KEYS.iter().any(|k| is_root_assignment(line, k)))
        {
            continue;
        }
        out.push(line);
    }
    let mut joined = out.join(nl);
    if !joined.is_empty() {
        joined.push_str(nl);
    }
    joined
}

#[cfg(test)]
#[path = "config_patch/tests.rs"]
mod tests;
