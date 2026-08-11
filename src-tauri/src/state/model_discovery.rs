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
    /// `ProviderModel` entries are filled in, and the rest is cached so
    /// `toggle_model` can persist it when the model is enabled. Existing
    /// values are kept — a hand-set override beats a later discovery.
    pub async fn discover_models(&self, provider_id: &str) -> anyhow::Result<Vec<String>> {
        let provider = {
            let cfg = self.config.read().await;
            cfg.providers
                .get(provider_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unknown provider '{provider_id}'"))?
        };
        let mut detailed = list_models_detailed(&provider).await?;
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
        let mut updated = false;
        {
            let mut cfg = self.config.write().await;
            if let Some(p) = cfg.providers.get_mut(provider_id) {
                for m in p.models.iter_mut() {
                    let protocol = persisted_model_protocol(
                        m.protocol.as_ref(),
                        detected_protocols.get(&m.id),
                    );
                    if m.protocol != protocol {
                        m.protocol = protocol;
                        updated = true;
                    }
                    if let Some(vision_models) = &vision_models {
                        let next = vision_models.contains(&m.id);
                        if m.supports_vision != next {
                            m.supports_vision = next;
                            updated = true;
                        }
                    }
                    if m.context_window.is_none() {
                        if let Some((_, ctx)) = known.iter().find(|(id, _)| id == &m.id) {
                            m.context_window = Some(*ctx);
                            updated = true;
                        }
                    }
                }
            }
        }
        if updated {
            self.persist().await?;
            self.maybe_auto_apply().await;
        }
        Ok(detailed.into_iter().map(|(id, _)| id).collect())
    }
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

/// Fetch a provider's live model catalog, keeping whatever context window
/// each entry publishes. Most providers publish none — OpenCode Go returns
/// only id/created/object/owned_by, which the models.dev enrichment in
/// `AppState::discover_models` covers.
pub async fn list_models_detailed(
    p: &crate::config::Provider,
) -> anyhow::Result<Vec<(String, Option<u32>)>> {
    // The claude-code provider has no remote catalog: the models are the
    // curated set served by the local `claude` CLI on the subscription.
    if p.id == crate::providers::CLAUDE_CODE_PROVIDER_ID {
        return Ok(crate::providers::CLAUDE_CODE_MODELS
            .iter()
            .map(|(id, ctx, _)| (id.to_string(), Some(*ctx)))
            .collect());
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
    use crate::config::{AppConfig, ProviderProtocol};
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
}
