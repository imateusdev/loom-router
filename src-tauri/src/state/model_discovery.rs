//! Live provider catalog discovery, models.dev enrichment, and dialect probes.

use super::{http_client, AppState};
use crate::codex;
use std::collections::{HashMap, HashSet};

/// Public model-limit catalog (the same source OpenCode uses for its model
/// limits), used when a provider's own `/models` publishes no context window.
const MODELS_DEV_URL: &str = "https://models.dev/api.json";
const MODELS_DEV_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

impl AppState {
    /// Fetch the native Codex model slugs for the model pickers. Returns a
    /// cached value when the proxy is already running; otherwise runs a
    /// blocking CLI probe and caches the result for the next active read.
    pub async fn codex_native_models(&self) -> Vec<String> {
        let server_running = self.server.read().await.is_some();
        if server_running {
            let cached = self.native_slugs_cache.read().await.clone();
            if let Some(cached) = cached {
                return cached;
            }
        }

        let cfg = self.config.read().await.clone();
        let slugs = tokio::task::spawn_blocking(move || codex::native_model_slugs(&cfg))
            .await
            .unwrap_or_default();
        *self.native_slugs_cache.write().await = Some(slugs.clone());
        slugs
    }

    /// Discard native slugs after a Codex integration rewrite.
    pub(super) async fn invalidate_native_slugs_cache(&self) {
        *self.native_slugs_cache.write().await = None;
    }

    /// Return a fresh models.dev catalog, caching successful responses. The
    /// read guard ends before the HTTP request so a slow catalog never blocks
    /// unrelated readers or the cache write that follows.
    async fn models_dev_catalog(&self) -> Option<serde_json::Value> {
        let cached = {
            let guard = self.models_dev.read().await;
            guard
                .as_ref()
                .filter(|(fetched_at, _)| fetched_at.elapsed() < MODELS_DEV_TTL)
                .map(|(_, json)| json.clone())
        };
        if cached.is_some() {
            return cached;
        }

        let response = http_client().get(MODELS_DEV_URL).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        let json = response.json::<serde_json::Value>().await.ok()?;
        *self.models_dev.write().await = Some((std::time::Instant::now(), json.clone()));
        Some(json)
    }

    /// Fill context windows the provider's own catalog did not publish from
    /// the public models.dev catalog (the same source OpenCode itself uses
    /// for its model limits). Best effort: any failure leaves the entries
    /// untouched.
    async fn enrich_from_models_dev(
        &self,
        provider_id: &str,
        models: &mut [(String, Option<u32>)],
    ) {
        if models.iter().all(|(_, ctx)| ctx.is_some()) {
            return;
        }
        let Some(catalog) = self.models_dev_catalog().await else {
            return;
        };
        let entries = catalog
            .get(models_dev_key(provider_id))
            .and_then(|p| p.get("models"))
            .and_then(serde_json::Value::as_object);
        let Some(entries) = entries else { return };
        for (id, ctx) in models.iter_mut() {
            if ctx.is_none() {
                let found = entries
                    .get(id.as_str())
                    .and_then(|m| m.pointer("/limit/context"))
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|v| u32::try_from(v).ok());
                if let Some(c) = found {
                    *ctx = Some(c);
                }
            }
        }
    }

    async fn vision_models_from_models_dev(&self, provider_id: &str) -> Option<HashSet<String>> {
        let catalog = self.models_dev_catalog().await?;
        let entries = catalog
            .get(models_dev_key(provider_id))
            .and_then(|p| p.get("models"))
            .and_then(serde_json::Value::as_object)?;
        Some(
            entries
                .iter()
                .filter_map(|(id, model)| {
                    let inputs = model.pointer("/modalities/input")?.as_array()?;
                    inputs
                        .iter()
                        .any(|v| v.as_str() == Some("image"))
                        .then(|| id.clone())
                })
                .collect(),
        )
    }

    /// Live model discovery: GET {base_url}/models (OpenAI-compatible).
    ///
    /// Beyond returning ids to the UI, this persists whatever context
    /// windows the catalog (or the models.dev fallback) publishes: existing
    /// `ProviderModel` entries are filled in, and models the upstream
    /// catalog has started publishing are added disabled, ready for the user
    /// to enable. Existing user choices are never overwritten.
    pub async fn discover_models(&self, provider_id: &str) -> anyhow::Result<Vec<String>> {
        let (models, changed) = self.discover_models_without_persist(provider_id).await?;
        if changed {
            self.persist().await?;
            self.maybe_auto_apply().await;
        }
        Ok(models)
    }

    /// Refresh every enabled provider once, committing the combined model
    /// changes in one write and one Codex re-apply instead of once per API.
    pub async fn refresh_enabled_provider_model_catalogs(&self) -> bool {
        let provider_ids: Vec<String> = self
            .config
            .read()
            .await
            .providers
            .values()
            .filter(|provider| provider.enabled)
            .map(|provider| provider.id.clone())
            .collect();
        let mut changed = false;
        for provider_id in provider_ids {
            match self.discover_models_without_persist(&provider_id).await {
                Ok((_, updated)) => changed |= updated,
                Err(error) => tracing::warn!(
                    provider = %provider_id,
                    "periodic provider model catalog refresh failed: {error}"
                ),
            }
        }
        if changed {
            if let Err(error) = self.persist().await {
                tracing::warn!("persisting periodic provider catalog refresh failed: {error}");
                return false;
            }
            self.maybe_auto_apply().await;
        }
        changed
    }

    async fn discover_models_without_persist(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<(Vec<String>, bool)> {
        let provider = {
            let cfg = self.config.read().await;
            cfg.providers
                .get(provider_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unknown provider '{provider_id}'"))?
        };
        // claude-code has no remote catalog to fetch, but it does have a
        // public one: models.dev is already downloaded here for context
        // windows and vision flags, and it carries new Anthropic models the
        // day they ship.
        let mut detailed = if provider.id == crate::providers::CLAUDE_CODE_PROVIDER_ID {
            claude_code_catalog(self.models_dev_catalog().await.as_ref())
        } else {
            list_models_detailed(&provider).await?
        };
        let enabled_ids: Vec<String> = provider
            .models
            .iter()
            .filter(|model| model.enabled)
            .map(|model| model.id.clone())
            .collect();
        // The public capability catalog and upstream dialect checks are
        // independent. Starting them together avoids making Fetch models wait
        // for a multi-megabyte catalog before it validates enabled models.
        let (_, detected_protocols) = futures::join!(
            self.enrich_from_models_dev(provider_id, &mut detailed),
            probe_enabled_model_dialects(&provider, provider_id, enabled_ids),
        );
        let vision_models = self.vision_models_from_models_dev(provider_id).await;
        match &vision_models {
            Some(models) => tracing::info!(
                provider = %provider_id,
                visual_models = ?models,
                "model capability detection completed from models.dev"
            ),
            None => tracing::warn!(
                provider = %provider_id,
                "model capability detection unavailable; models.dev did not return a catalog"
            ),
        }
        for (id, _) in &detailed {
            tracing::info!(
                provider = %provider_id,
                model = %id,
                supports_vision = vision_models
                    .as_ref()
                    .map(|models| models.contains(id))
                    .unwrap_or(false),
                "model capability"
            );
        }

        let known: Vec<(String, u32)> = detailed
            .iter()
            .filter_map(|(id, ctx)| ctx.map(|c| (id.clone(), c)))
            .collect();
        if !known.is_empty() {
            let mut cache = self.model_contexts.write().await;
            let per_provider = cache.entry(provider_id.to_string()).or_default();
            for (id, ctx) in &known {
                per_provider.insert(id.clone(), *ctx);
            }
        }
        let updated = {
            let mut cfg = self.config.write().await;
            cfg.providers.get_mut(provider_id).is_some_and(|current| {
                merge_discovered_models(
                    current,
                    &detailed,
                    &detected_protocols,
                    vision_models.as_ref(),
                )
            })
        };
        Ok((detailed.into_iter().map(|(id, _)| id).collect(), updated))
    }
}

fn merge_discovered_models(
    provider: &mut crate::config::Provider,
    detailed: &[(String, Option<u32>)],
    detected_protocols: &HashMap<String, crate::config::ProviderProtocol>,
    vision_models: Option<&HashSet<String>>,
) -> bool {
    let is_claude_code = provider.id == crate::providers::CLAUDE_CODE_PROVIDER_ID;
    let mut changed = false;
    for model in &mut provider.models {
        let protocol =
            persisted_model_protocol(model.protocol.as_ref(), detected_protocols.get(&model.id));
        if model.protocol != protocol {
            model.protocol = protocol;
            changed = true;
        }
        if let Some(vision_models) = vision_models {
            let next = vision_models.contains(&model.id);
            if model.supports_vision != next {
                model.supports_vision = next;
                changed = true;
            }
        }
        if model.context_window.is_none() {
            // Only a published window is an update. Writing `None` over `None`
            // reported a change on every pass, and the periodic refresh turned
            // that into a persist plus a full `codex::apply` every tick.
            if let Some((_, Some(context))) = detailed.iter().find(|(id, _)| id == &model.id) {
                model.context_window = Some(*context);
                changed = true;
            }
        }
    }
    for (id, context_window) in detailed {
        if provider.models.iter().any(|model| model.id == *id) {
            continue;
        }
        provider.models.push(crate::config::ProviderModel {
            id: id.clone(),
            label: if is_claude_code {
                crate::providers::claude_code_label(id)
            } else {
                None
            },
            context_window: *context_window,
            protocol: None,
            fast_mode: is_claude_code && crate::providers::claude_code_fast_mode(id),
            // Discovered, not adopted. `toggle_model` probes the wire dialect
            // before exposing a model, so enabling here would publish it to
            // Codex with `protocol: None` and route its first turn through a
            // guessed dialect. It would also enlarge the enabled set that the
            // periodic refresh re-probes, turning a large upstream catalog
            // into a recurring burst of billed completion requests.
            enabled: false,
            supports_vision: vision_models
                .map(|models| models.contains(id))
                .unwrap_or(is_claude_code),
        });
        changed = true;
    }
    changed
}

/// The models.dev catalog key for one of our provider ids, where the two
/// catalogs use different slugs. The gateway publishes two entries —
/// `opencode` (Zen) and `opencode-go` (Go) — and Zen's does not match our id
/// either way. Prefix rather than exact match so both the merged provider
/// (`opencode-zen`) and the per-dialect ones it replaced (`opencode-zen-chat`
/// and friends, still on disk until the first launch migrates them) resolve:
/// a key that does not exist makes the enrichment silently no-op, leaving
/// every model on the provider at the conservative 128k.
fn models_dev_key(provider_id: &str) -> &str {
    if provider_id.starts_with("opencode-zen") {
        "opencode"
    } else if provider_id.starts_with("opencode-go") {
        "opencode-go"
    } else if provider_id == "kimi-coding" {
        // The preset id follows the provider used by the coding CLI, while
        // models.dev publishes this catalog as `kimi-for-coding`.
        "kimi-for-coding"
    } else if provider_id == "zai-coding" {
        // The coding endpoint advertises the full Z.AI catalog, which
        // models.dev publishes as `zai` rather than the provider slug.
        "zai"
    } else if provider_id == crate::providers::CLAUDE_CODE_PROVIDER_ID {
        "anthropic"
    } else {
        provider_id
    }
}

/// Context window (tokens) one catalog entry publishes, if any. Dialects
/// seen in the wild: OpenRouter's flat `context_length` (with
/// `top_provider.context_length` as backup) and the models.dev-shaped
/// `limit.context`.
fn entry_context_window(m: &serde_json::Value) -> Option<u32> {
    m.get("context_length")
        .and_then(serde_json::Value::as_u64)
        .or_else(|| {
            m.pointer("/top_provider/context_length")
                .and_then(serde_json::Value::as_u64)
        })
        .or_else(|| {
            m.pointer("/limit/context")
                .and_then(serde_json::Value::as_u64)
        })
        .and_then(|v| u32::try_from(v).ok())
}

/// Build a deliberately tiny non-streaming request for one upstream wire.
/// A successful status proves the gateway accepted both this model and this
/// endpoint shape; model discovery itself only returns ids, never this fact.
fn dialect_probe_request(
    protocol: &crate::config::ProviderProtocol,
    model: &str,
) -> (&'static str, serde_json::Value) {
    use crate::config::ProviderProtocol;
    match protocol {
        ProviderProtocol::OpenAI => (
            "chat/completions",
            serde_json::json!({
                "model": model,
                "messages": [{"role": "user", "content": "Reply with OK."}],
                "max_tokens": 16,
                "stream": false,
            }),
        ),
        ProviderProtocol::Anthropic => (
            "messages",
            serde_json::json!({
                "model": model,
                "messages": [{"role": "user", "content": "Reply with OK."}],
                "max_tokens": 16,
                "stream": false,
            }),
        ),
        ProviderProtocol::Responses => (
            "responses",
            serde_json::json!({
                "model": model,
                "input": "Reply with OK.",
                "max_output_tokens": 16,
                "stream": false,
            }),
        ),
    }
}

const MAX_PARALLEL_DIALECT_PROBES: usize = 3;

async fn probe_enabled_model_dialects(
    provider: &crate::config::Provider,
    provider_id: &str,
    model_ids: Vec<String>,
) -> HashMap<String, crate::config::ProviderProtocol> {
    use futures::stream::{self, StreamExt};

    stream::iter(model_ids.into_iter().map(|id| {
        let provider = provider.clone();
        let provider_id = provider_id.to_string();
        async move {
            let protocol = probe_model_dialect(&provider, &id).await;
            (provider_id, id, protocol)
        }
    }))
    // Gateways can rate-limit bursts, so lower the wall-clock wait without
    // turning a long enabled-model list into an unbounded request fan-out.
    .buffer_unordered(MAX_PARALLEL_DIALECT_PROBES)
    .filter_map(|(provider_id, id, result)| async move {
        match result {
            Ok(protocol) => Some((id, protocol)),
            Err(error) => {
                tracing::warn!(
                    provider = %provider_id,
                    model = %id,
                    "model dialect validation failed: {error}"
                );
                None
            }
        }
    })
    .collect()
    .await
}

fn select_detected_dialect(
    provider_default: crate::config::ProviderProtocol,
    supported: &[crate::config::ProviderProtocol],
) -> Option<crate::config::ProviderProtocol> {
    supported
        .iter()
        .find(|protocol| **protocol == provider_default)
        .cloned()
        .or_else(|| supported.first().cloned())
}

fn persisted_model_protocol(
    current: Option<&crate::config::ProviderProtocol>,
    detected: Option<&crate::config::ProviderProtocol>,
) -> Option<crate::config::ProviderProtocol> {
    current.cloned().or_else(|| detected.cloned())
}

fn provider_with_primary_key(p: &crate::config::Provider) -> crate::config::Provider {
    let mut provider = p.clone();
    if let Some(key) = p.keys.iter().find(|key| {
        key.enabled
            && key
                .api_key
                .as_deref()
                .is_some_and(|value| !value.is_empty())
    }) {
        provider.api_key = key.api_key.clone();
        provider.has_key = key.has_key;
    }
    provider
}

pub(super) async fn probe_model_dialect(
    provider: &crate::config::Provider,
    model: &str,
) -> anyhow::Result<crate::config::ProviderProtocol> {
    use crate::config::ProviderProtocol;

    // This provider executes through the local CLI, not an HTTP endpoint.
    if provider.id == crate::providers::CLAUDE_CODE_PROVIDER_ID {
        return Ok(ProviderProtocol::Anthropic);
    }

    let client = http_client();
    let provider = provider_with_primary_key(provider);
    let candidates = [
        ProviderProtocol::OpenAI,
        ProviderProtocol::Anthropic,
        ProviderProtocol::Responses,
    ];
    let mut supported = Vec::new();
    for protocol in candidates {
        let (path, body) = dialect_probe_request(&protocol, model);
        let mut probe_provider = provider.clone();
        probe_provider.protocol = protocol.clone();
        let url = format!("{}/{path}", provider.base_url.trim_end_matches('/'));
        let mut request =
            crate::proxy::apply_provider_auth(client.post(url).json(&body), &probe_provider, None);
        if let Some(user_agent) = &provider.user_agent {
            request = request.header("user-agent", user_agent);
        }
        match request.send().await {
            Ok(response) if response.status().is_success() => supported.push(protocol),
            Ok(response) => tracing::debug!(
                provider = %provider.id,
                model,
                protocol = ?protocol,
                status = %response.status(),
                "model dialect probe rejected"
            ),
            Err(error) => tracing::debug!(
                provider = %provider.id,
                model,
                protocol = ?protocol,
                "model dialect probe failed: {error}"
            ),
        }
    }
    select_detected_dialect(provider.protocol.clone(), &supported).ok_or_else(|| {
        anyhow::anyhow!("no supported upstream wire dialect detected for model '{model}'")
    })
}

/// True for the dated snapshot models.dev publishes next to a rolling alias
/// (`claude-opus-4-5-20251101` beside `claude-opus-4-5`). Codex and Claude
/// Code both address the alias, so listing the twin only doubles every row.
fn is_dated_snapshot(model_id: &str) -> bool {
    model_id
        .rsplit_once('-')
        .is_some_and(|(_, tail)| tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_digit()))
}

/// The claude-code catalog: models.dev's Anthropic entries, minus dated
/// snapshots, unioned with the curated const.
///
/// The union direction matters. models.dev is the live half — it is where
/// `claude-sonnet-5` and `claude-fable-5-1` appear without a LoomRouter
/// release — while the const is a floor, so an unreachable or reshaped
/// models.dev can only ever fail to add a model, never take one away from a
/// working install. Passing `None` yields exactly the const, which is the
/// offline behaviour.
pub(super) fn claude_code_catalog(
    models_dev: Option<&serde_json::Value>,
) -> Vec<(String, Option<u32>)> {
    let mut catalog: Vec<(String, Option<u32>)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let entries = models_dev
        .and_then(|catalog| catalog.get(models_dev_key(crate::providers::CLAUDE_CODE_PROVIDER_ID)))
        .and_then(|provider| provider.get("models"))
        .and_then(serde_json::Value::as_object);
    for (id, entry) in entries.into_iter().flatten() {
        if is_dated_snapshot(id) || !seen.insert(id.clone()) {
            continue;
        }
        let context = entry
            .pointer("/limit/context")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok());
        catalog.push((
            id.clone(),
            context.or_else(|| crate::providers::claude_code_context(id)),
        ));
    }

    for (id, context, _) in crate::providers::CLAUDE_CODE_MODELS {
        if seen.insert((*id).to_string()) {
            catalog.push(((*id).to_string(), Some(*context)));
        }
    }

    // models.dev hands back a map; without this the picker order would depend
    // on serde_json's map implementation.
    catalog.sort_by(|a, b| a.0.cmp(&b.0));
    catalog
}

/// Fetch a provider's live model catalog, keeping whatever context window
/// each entry publishes. Most providers publish none — OpenCode Go returns
/// only id/created/object/owned_by, which the models.dev enrichment in
/// `AppState::discover_models` covers.
pub async fn list_models_detailed(
    p: &crate::config::Provider,
) -> anyhow::Result<Vec<(String, Option<u32>)>> {
    // The claude-code provider has no remote catalog: requests are served by
    // the local `claude` CLI on the subscription. Callers on this path are
    // credential/health probes, so they get the offline catalog rather than
    // paying for a multi-megabyte models.dev fetch; `discover_models` is the
    // one that asks for the live list.
    if p.id == crate::providers::CLAUDE_CODE_PROVIDER_ID {
        return Ok(claude_code_catalog(None));
    }
    let provider = provider_with_primary_key(p);
    let url = format!("{}/models", provider.base_url.trim_end_matches('/'));
    let client = http_client();
    // Protocol-correct auth shared with the proxy (Anthropic gets
    // x-api-key + anthropic-version; everything else a bearer token). The
    // catalog is the whole provider, not one model: provider dialect.
    let mut req = crate::proxy::apply_provider_auth(client.get(&url), &provider, None);
    if let Some(ua) = &provider.user_agent {
        req = req.header("user-agent", ua);
    }
    let res = req
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("request failed: {e}"))?;
    let status = res.status();
    let body: serde_json::Value = res
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("invalid JSON from provider: {e}"))?;
    if !status.is_success() {
        let msg = body
            .pointer("/error/message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown error");
        anyhow::bail!("provider returned {status}: {msg}");
    }
    let models: Vec<_> = body
        .get("data")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    m.get("id")
                        .and_then(serde_json::Value::as_str)
                        .map(|id| (id.to_string(), entry_context_window(m)))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(models
        .into_iter()
        .filter(|(id, _)| !crate::config::is_gpt_model_id(id))
        .collect())
}

/// Fetch a provider's live model catalog (also validates the API key).
pub async fn list_models(p: &crate::config::Provider) -> anyhow::Result<Vec<String>> {
    Ok(list_models_detailed(p)
        .await?
        .into_iter()
        .map(|(id, _)| id)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, ProviderModel, ProviderProtocol};
    use serde_json::json;

    #[tokio::test]
    async fn models_dev_catalog_reuses_a_fresh_cache() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::for_test(AppConfig::default(), dir.path().join("config.json"));
        let expected = json!({"openrouter": {"models": {}}});
        *state.models_dev.write().await = Some((std::time::Instant::now(), expected.clone()));

        assert_eq!(state.models_dev_catalog().await, Some(expected));
    }

    #[test]
    fn catalog_sync_adds_new_models_disabled_without_changing_existing_choices() {
        // This catches a regression where the periodic catalog refresh either
        // drops a newly released model or overrides an explicit user choice
        // for a model already in the provider. New models arrive disabled:
        // `toggle_model` owns enabling, because it probes the wire dialect
        // first.
        let mut provider = crate::config::Provider {
            id: "test".into(),
            name: "Test".into(),
            protocol: ProviderProtocol::OpenAI,
            base_url: "https://example.test/v1".into(),
            api_key: None,
            keys: vec![],
            rotation_enabled: false,
            has_key: false,
            context_window: None,
            user_agent: None,
            prompt_cache: None,
            models: vec![ProviderModel {
                id: "existing".into(),
                label: None,
                context_window: None,
                protocol: None,
                fast_mode: false,
                enabled: false,
                supports_vision: false,
            }],
            enabled: true,
        };

        let changed = merge_discovered_models(
            &mut provider,
            &[
                ("existing".into(), Some(128_000)),
                ("new-model".into(), Some(256_000)),
            ],
            &HashMap::new(),
            None,
        );

        assert!(changed);
        assert!(!provider.models[0].enabled);
        assert_eq!(provider.models[0].context_window, Some(128_000));
        assert_eq!(provider.models[1].id, "new-model");
        assert!(!provider.models[1].enabled);
        assert!(provider.models[1].protocol.is_none());
        assert_eq!(provider.models[1].context_window, Some(256_000));
    }

    #[test]
    fn catalog_sync_without_a_published_context_window_reports_no_change() {
        // The periodic refresh persists and re-applies the whole Codex
        // integration whenever this returns true. A catalog that publishes no
        // context window used to write `None` over `None` and still report a
        // change, so an idle install rewrote ~/.codex/config.toml every tick.
        let mut provider = crate::config::Provider {
            id: "test".into(),
            name: "Test".into(),
            protocol: ProviderProtocol::OpenAI,
            base_url: "https://example.test/v1".into(),
            api_key: None,
            keys: vec![],
            rotation_enabled: false,
            has_key: false,
            context_window: None,
            user_agent: None,
            prompt_cache: None,
            models: vec![ProviderModel {
                id: "no-window".into(),
                label: None,
                context_window: None,
                protocol: None,
                fast_mode: false,
                enabled: true,
                supports_vision: false,
            }],
            enabled: true,
        };

        let changed = merge_discovered_models(
            &mut provider,
            &[("no-window".into(), None)],
            &HashMap::new(),
            None,
        );

        assert!(!changed, "an unchanged catalog must not trigger a rewrite");
        assert_eq!(provider.models[0].context_window, None);
    }

    #[test]
    fn entry_context_window_reads_each_catalog_dialect() {
        let flat = json!({"id":"a","context_length":1_048_576});
        assert_eq!(entry_context_window(&flat), Some(1_048_576));
        let nested = json!({"id":"b","top_provider":{"context_length":200_000}});
        assert_eq!(entry_context_window(&nested), Some(200_000));
        let models_dev = json!({"id":"c","limit":{"context":256_000,"output":64_000}});
        assert_eq!(entry_context_window(&models_dev), Some(256_000));
        let bare = json!({"id":"d","object":"model","created":1,"owned_by":"x"});
        assert_eq!(entry_context_window(&bare), None);
    }

    #[test]
    fn entry_context_window_prefers_the_flat_field() {
        let both = json!({
            "id":"a",
            "context_length":1_048_576,
            "top_provider":{"context_length":128_000}
        });
        assert_eq!(entry_context_window(&both), Some(1_048_576));
    }

    #[test]
    fn models_dev_key_maps_all_opencode_presets() {
        for zen in [
            "opencode-zen",
            "opencode-zen-chat",
            "opencode-zen-claude",
            "opencode-zen-responses",
        ] {
            assert_eq!(models_dev_key(zen), "opencode");
        }
        for go in [
            "opencode-go",
            "opencode-go-chat",
            "opencode-go-claude",
            "opencode-go-responses",
        ] {
            assert_eq!(models_dev_key(go), "opencode-go");
        }
        assert_eq!(models_dev_key("openrouter"), "openrouter");
        assert_eq!(models_dev_key("kimi-coding"), "kimi-for-coding");
        assert_eq!(models_dev_key("zai-coding"), "zai");
    }

    #[test]
    fn dialect_probe_requests_use_each_wire_format() {
        let (path, chat) = dialect_probe_request(&ProviderProtocol::OpenAI, "kimi-k3");
        assert_eq!(path, "chat/completions");
        assert_eq!(chat["model"], "kimi-k3");
        assert_eq!(chat["messages"][0]["content"], "Reply with OK.");

        let (path, anthropic) = dialect_probe_request(&ProviderProtocol::Anthropic, "qwen3.8-max");
        assert_eq!(path, "messages");
        assert_eq!(anthropic["model"], "qwen3.8-max");
        assert_eq!(anthropic["max_tokens"], 16);

        let (path, responses) = dialect_probe_request(&ProviderProtocol::Responses, "gpt-5.6-sol");
        assert_eq!(path, "responses");
        assert_eq!(responses["model"], "gpt-5.6-sol");
        assert_eq!(responses["input"], "Reply with OK.");
    }

    #[test]
    fn detected_dialect_prefers_the_provider_default_when_several_work() {
        let detected = select_detected_dialect(
            ProviderProtocol::Responses,
            &[ProviderProtocol::OpenAI, ProviderProtocol::Responses],
        );
        assert_eq!(detected, Some(ProviderProtocol::Responses));
    }

    #[test]
    fn discovered_dialect_does_not_replace_a_known_model_protocol() {
        assert_eq!(
            persisted_model_protocol(
                Some(&ProviderProtocol::Responses),
                Some(&ProviderProtocol::Anthropic),
            ),
            Some(ProviderProtocol::Responses),
        );
    }

    fn models_dev_anthropic(ids: &[(&str, u64)]) -> serde_json::Value {
        let entries: serde_json::Map<String, serde_json::Value> = ids
            .iter()
            .map(|(id, ctx)| {
                (
                    (*id).to_string(),
                    serde_json::json!({"limit": {"context": ctx}}),
                )
            })
            .collect();
        serde_json::json!({ "anthropic": { "models": entries } })
    }

    #[test]
    fn claude_code_catalog_without_models_dev_is_the_offline_const() {
        let catalog = claude_code_catalog(None);
        let ids: Vec<&str> = catalog.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids.len(), crate::providers::CLAUDE_CODE_MODELS.len());
        for (id, _, _) in crate::providers::CLAUDE_CODE_MODELS {
            assert!(ids.contains(id), "offline catalog dropped {id}");
        }
    }

    #[test]
    fn claude_code_catalog_picks_up_models_shipped_after_this_build() {
        // The regression this fixes: the catalog was a const, so a model
        // released after the binary (Sonnet 5, Fable 5.1) could never appear
        // however many times the user pressed Fetch.
        let catalog = claude_code_catalog(Some(&models_dev_anthropic(&[
            ("claude-sonnet-5", 1_000_000),
            ("claude-fable-5-1", 1_000_000),
            ("claude-opus-5", 1_000_000),
        ])));
        let ids: Vec<&str> = catalog.iter().map(|(id, _)| id.as_str()).collect();
        assert!(ids.contains(&"claude-sonnet-5"));
        assert!(ids.contains(&"claude-fable-5-1"));
        // And the curated entries models.dev did not mention survive.
        assert!(ids.contains(&"claude-haiku-4-5"));
        assert_eq!(
            ids.iter().filter(|id| **id == "claude-opus-5").count(),
            1,
            "a model both sources list must not be duplicated"
        );
    }

    #[test]
    fn claude_code_catalog_drops_dated_snapshots_and_keeps_the_alias() {
        let catalog = claude_code_catalog(Some(&models_dev_anthropic(&[
            ("claude-sonnet-4-5", 1_000_000),
            ("claude-sonnet-4-5-20250929", 1_000_000),
            ("claude-haiku-4-5-20251001", 200_000),
        ])));
        let ids: Vec<&str> = catalog.iter().map(|(id, _)| id.as_str()).collect();
        assert!(ids.contains(&"claude-sonnet-4-5"));
        assert!(!ids.contains(&"claude-sonnet-4-5-20250929"));
        assert!(!ids.contains(&"claude-haiku-4-5-20251001"));
        // The undated twin of a dropped snapshot still comes from the const.
        assert!(ids.contains(&"claude-haiku-4-5"));
    }

    #[test]
    fn claude_code_catalog_prefers_models_dev_context_and_falls_back_to_the_const() {
        let catalog = claude_code_catalog(Some(&models_dev_anthropic(&[
            ("claude-opus-5", 2_000_000),
            ("claude-sonnet-5", 0),
        ])));
        let ctx = |wanted: &str| {
            catalog
                .iter()
                .find(|(id, _)| id == wanted)
                .and_then(|(_, ctx)| *ctx)
        };
        assert_eq!(ctx("claude-opus-5"), Some(2_000_000));
        // Curated entries models.dev omitted keep the const window.
        assert_eq!(ctx("claude-haiku-4-5"), Some(200_000));
    }

    #[test]
    fn claude_code_catalog_order_is_stable() {
        let source = models_dev_anthropic(&[
            ("claude-sonnet-5", 1_000_000),
            ("claude-fable-5-1", 1_000_000),
        ]);
        let first = claude_code_catalog(Some(&source));
        let second = claude_code_catalog(Some(&source));
        assert_eq!(first, second);
        let ids: Vec<&str> = first.iter().map(|(id, _)| id.as_str()).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted);
    }

    #[test]
    fn dated_snapshot_detection_only_matches_a_trailing_eight_digit_date() {
        assert!(is_dated_snapshot("claude-opus-4-5-20251101"));
        assert!(!is_dated_snapshot("claude-opus-4-5"));
        assert!(!is_dated_snapshot("claude-fable-5-1"));
        assert!(!is_dated_snapshot("claude-sonnet-5"));
        assert!(!is_dated_snapshot("gpt-5.4-mini"));
    }
}
