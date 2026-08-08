//! Shared application state: config, proxy server lifecycle, integrations.

use crate::codex;
use crate::config::{AppConfig, VisualAssistanceConfig};
use crate::stats::{SharedStats, Stats};
use serde::Serialize;
use std::{collections::HashSet, sync::Arc};
use tokio::sync::{oneshot, RwLock};

pub type SharedConfig = Arc<RwLock<AppConfig>>;

#[derive(Debug, Clone, Serialize)]
pub struct ServerStatus {
    pub running: bool,
    pub port: u16,
    pub url: Option<String>,
}

pub struct AppState {
    pub config: SharedConfig,
    pub stats: SharedStats,
    server: RwLock<Option<ServerHandle>>,
    /// Serializes the combined power toggle. Without it, two fast clicks on
    /// the tray item interleave start/stop with apply/remove and settle on a
    /// state neither click asked for.
    power: tokio::sync::Mutex<()>,
    /// Context windows learned during model discovery, per provider, for
    /// models not yet in the config (they only get a `ProviderModel` entry
    /// when the user enables one — see toggle_model).
    model_contexts:
        RwLock<std::collections::HashMap<String, std::collections::HashMap<String, u32>>>,
    /// Cached models.dev catalog and when it was fetched. The file is
    /// several MB and changes rarely, so one fetch per session window is
    /// plenty.
    models_dev: RwLock<Option<(std::time::Instant, serde_json::Value)>>,
    /// Test-only destination for configuration writes. Command integration
    /// tests must prove persistence without touching the user's real config.
    #[cfg(test)]
    test_config_path: Option<std::path::PathBuf>,
}

struct ServerHandle {
    shutdown: oneshot::Sender<()>,
}

impl AppState {
    pub fn load() -> Self {
        Self {
            config: Arc::new(RwLock::new(AppConfig::load())),
            stats: Arc::new(RwLock::new(Stats::load())),
            server: RwLock::new(None),
            power: tokio::sync::Mutex::new(()),
            model_contexts: RwLock::new(std::collections::HashMap::new()),
            models_dev: RwLock::new(None),
            #[cfg(test)]
            test_config_path: None,
        }
    }

    async fn persist(&self) -> anyhow::Result<()> {
        #[cfg(test)]
        if let Some(path) = &self.test_config_path {
            let config = self.config.read().await;
            let json = serde_json::to_string_pretty(&*config)?;
            crate::secure_fs::write_private(path, json.as_bytes())?;
            return Ok(());
        }
        self.config.read().await.save()
    }

    #[cfg(test)]
    pub(crate) fn for_test(config: AppConfig, config_path: std::path::PathBuf) -> Self {
        Self {
            config: Arc::new(RwLock::new(config)),
            stats: Arc::new(RwLock::new(Stats::load())),
            server: RwLock::new(None),
            power: tokio::sync::Mutex::new(()),
            model_contexts: RwLock::new(std::collections::HashMap::new()),
            models_dev: RwLock::new(None),
            test_config_path: Some(config_path),
        }
    }

    /// Write out a config that `AppConfig::load()` rewrote on the way in and
    /// push the result to Codex. Nothing else does it: the auto-apply hangs
    /// off config *changes*, and a migration happens before the user makes
    /// one — leaving Codex pointed at a provider id that no longer exists.
    pub async fn persist_migration(&self) -> anyhow::Result<()> {
        if !self.config.read().await.migrated {
            return Ok(());
        }
        self.persist().await?;
        self.config.write().await.migrated = false;
        self.maybe_auto_apply().await;
        Ok(())
    }

    /// Re-apply the Codex integration after a config change, but only when
    /// the user enabled it. Failures are logged, never fatal.
    async fn maybe_auto_apply(&self) {
        let cfg = self.config.read().await.clone();
        if !cfg.codex_integration {
            return;
        }
        // `codex::apply` shells out and rewrites two files — off the async
        // executor, same as every other call site.
        let port = cfg.port;
        match tokio::task::spawn_blocking(move || codex::apply(&cfg, port)).await {
            Ok(Ok(())) => tracing::info!("Codex integration auto-applied after config change"),
            Ok(Err(e)) => tracing::warn!("auto-apply of Codex integration failed: {e}"),
            Err(e) => tracing::warn!("auto-apply of Codex integration panicked: {e}"),
        }
    }

    /// Rebuild generated Codex files once per LoomRouter launch. The Codex
    /// catalog changes independently of this app, so trusting the previous
    /// capture strands new native models outside the picker until a user
    /// happens to edit a provider or presses Apply again.
    pub async fn repair_codex_integration(&self) {
        match self
            .repair_codex_integration_with(|config, port| codex::apply(&config, port))
            .await
        {
            Ok(true) => tracing::info!("Codex integration catalog refreshed at startup"),
            Ok(false) => {}
            Err(e) => tracing::warn!("startup repair of Codex integration failed: {e}"),
        }
    }

    async fn repair_codex_integration_with<F>(&self, regenerate: F) -> anyhow::Result<bool>
    where
        F: FnOnce(AppConfig, u16) -> anyhow::Result<()> + Send + 'static,
    {
        let cfg = self.config.read().await.clone();
        if !cfg.codex_integration {
            return Ok(false);
        }
        let port = cfg.port;
        tokio::task::spawn_blocking(move || regenerate(cfg, port)).await??;
        Ok(true)
    }

    pub async fn save_provider(&self, mut provider: crate::config::Provider) -> anyhow::Result<()> {
        let mut cfg = self.config.write().await;
        // The UI never receives the real key back, so an empty key on save
        // means "keep the existing one" — never overwrite with empty.
        let empty_key = provider
            .api_key
            .as_deref()
            .map(str::is_empty)
            .unwrap_or(true);
        if empty_key {
            if let Some(existing) = cfg.providers.get(&provider.id) {
                provider.api_key = existing.api_key.clone();
            }
        }
        provider.has_key = provider
            .api_key
            .as_deref()
            .map(|k| !k.is_empty())
            .unwrap_or(false);
        // The claude-code catalog is curated: stamp every model with its real
        // context window and fast-mode participation so the picker and UI
        // never show a guess, and re-seed models that exist in the catalog
        // but were dropped (e.g. a stale edit that listed a subset).
        if provider.id == crate::providers::CLAUDE_CODE_PROVIDER_ID {
            for m in provider.models.iter_mut() {
                m.context_window = crate::providers::claude_code_context(&m.id);
                m.fast_mode = crate::providers::claude_code_fast_mode(&m.id);
            }
            let seeded = crate::providers::CLAUDE_CODE_MODELS
                .iter()
                .map(|(id, ctx, fast)| {
                    let enabled = provider
                        .models
                        .iter()
                        .find(|m| m.id == *id)
                        .map(|m| m.enabled)
                        .unwrap_or(false);
                    crate::config::ProviderModel {
                        id: id.to_string(),
                        label: crate::providers::claude_code_label(id),
                        context_window: Some(*ctx),
                        protocol: None,
                        fast_mode: *fast,
                        enabled,
                        supports_vision: false,
                    }
                });
            provider.models = seeded.collect();
        }
        cfg.providers.insert(provider.id.clone(), provider);
        drop(cfg);
        self.persist().await?;
        self.maybe_auto_apply().await;
        Ok(())
    }

    pub async fn delete_provider(&self, id: &str) -> anyhow::Result<()> {
        self.config.write().await.providers.remove(id);
        self.persist().await?;
        self.maybe_auto_apply().await;
        Ok(())
    }

    pub async fn toggle_model(
        &self,
        provider_id: &str,
        model: &str,
        enabled: bool,
    ) -> anyhow::Result<()> {
        // Read BEFORE taking the config write lock: a model the user enables
        // right after discovery carries its discovered context window into
        // the persisted ProviderModel entry.
        let discovered_context = self
            .model_contexts
            .read()
            .await
            .get(provider_id)
            .and_then(|m| m.get(model))
            .copied();
        // A multi-dialect gateway's catalog does not say which endpoint a
        // model accepts. Validate before exposing a newly enabled model, so
        // Codex never routes its first real turn through a guessed wire.
        let detected_protocol = if enabled {
            let provider = self
                .config
                .read()
                .await
                .providers
                .get(provider_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unknown provider '{provider_id}'"))?;
            Some(probe_model_dialect(&provider, model).await?)
        } else {
            None
        };
        let mut cfg = self.config.write().await;
        let provider = cfg
            .providers
            .get_mut(provider_id)
            .ok_or_else(|| anyhow::anyhow!("unknown provider '{provider_id}'"))?;
        if let Some(m) = provider.models.iter_mut().find(|m| m.id == model) {
            m.enabled = enabled;
            if let Some(protocol) = detected_protocol {
                m.protocol = Some(protocol);
            }
            if provider.id == crate::providers::CLAUDE_CODE_PROVIDER_ID {
                m.context_window = crate::providers::claude_code_context(model);
                m.fast_mode = crate::providers::claude_code_fast_mode(model);
            }
        } else {
            provider.models.push(crate::config::ProviderModel {
                id: model.to_string(),
                label: crate::providers::claude_code_label(model),
                context_window: discovered_context,
                protocol: detected_protocol,
                fast_mode: if provider.id == crate::providers::CLAUDE_CODE_PROVIDER_ID {
                    crate::providers::claude_code_fast_mode(model)
                } else {
                    false
                },
                enabled,
                supports_vision: false,
            });
        }
        drop(cfg);
        self.persist().await?;
        self.maybe_auto_apply().await;
        Ok(())
    }

    /// Record the wire dialect one model is served in. Kept for backwards
    /// compatibility with existing configurations; automatic probes own the
    /// value for models fetched or enabled in the current application.
    pub async fn set_model_protocol(
        &self,
        provider_id: &str,
        model: &str,
        protocol: Option<crate::config::ProviderProtocol>,
    ) -> anyhow::Result<()> {
        {
            let mut cfg = self.config.write().await;
            let provider = cfg
                .providers
                .get_mut(provider_id)
                .ok_or_else(|| anyhow::anyhow!("unknown provider '{provider_id}'"))?;
            let entry = provider
                .models
                .iter_mut()
                .find(|m| m.id == model)
                .ok_or_else(|| anyhow::anyhow!("unknown model '{model}'"))?;
            entry.protocol = protocol;
        }
        self.persist().await?;
        self.maybe_auto_apply().await;
        Ok(())
    }

    /// Replace the global visual-assistance policy and re-apply Codex when
    /// the integration is active, like other persisted routing preferences.
    pub async fn set_visual_assistance(
        &self,
        config: VisualAssistanceConfig,
    ) -> anyhow::Result<()> {
        let mut current = self.config.write().await;
        if config.enabled && config.assistant_model.is_none() {
            anyhow::bail!(
                "visual assistance requires a primary visual assistant before it can be enabled"
            );
        }
        if config.enabled {
            let mut next = current.clone();
            next.visual_assistance = config.clone();
            crate::visual::validate_configuration(&next)?;
        }
        current.visual_assistance = config;
        drop(current);
        self.persist().await?;
        self.maybe_auto_apply().await;
        Ok(())
    }

    /// Mark an existing model as capable (or incapable) of receiving images.
    /// Discovery cannot infer this reliably, so it is an explicit per-model
    /// preference and must not create unknown model entries.
    pub async fn set_model_vision(
        &self,
        provider_id: &str,
        model: &str,
        supports: bool,
    ) -> anyhow::Result<()> {
        {
            let mut cfg = self.config.write().await;
            let provider = cfg
                .providers
                .get_mut(provider_id)
                .ok_or_else(|| anyhow::anyhow!("unknown provider '{provider_id}'"))?;
            let entry = provider
                .models
                .iter_mut()
                .find(|m| m.id == model)
                .ok_or_else(|| anyhow::anyhow!("unknown model '{model}'"))?;
            entry.supports_vision = supports;
        }
        self.persist().await?;
        self.maybe_auto_apply().await;
        Ok(())
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
        // The file is several MB and changes rarely: one fetch per hour.
        const TTL: std::time::Duration = std::time::Duration::from_secs(3600);
        let cached = {
            let guard = self.models_dev.read().await;
            guard
                .as_ref()
                .filter(|(fetched_at, _)| fetched_at.elapsed() < TTL)
                .map(|(_, json)| json.clone())
        };
        let catalog = match cached {
            Some(json) => Some(json),
            None => {
                let json = match http_client().get(MODELS_DEV_URL).send().await {
                    Ok(res) if res.status().is_success() => {
                        res.json::<serde_json::Value>().await.ok()
                    }
                    _ => None,
                };
                if let Some(json) = &json {
                    *self.models_dev.write().await =
                        Some((std::time::Instant::now(), json.clone()));
                }
                json
            }
        };
        let Some(catalog) = catalog else { return };
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
        let catalog = {
            let guard = self.models_dev.read().await;
            guard
                .as_ref()
                .filter(|(at, _)| at.elapsed() < std::time::Duration::from_secs(3600))
                .map(|(_, json)| json.clone())
        };
        let catalog = match catalog {
            Some(json) => json,
            None => {
                let response = http_client().get(MODELS_DEV_URL).send().await.ok()?;
                if !response.status().is_success() {
                    return None;
                }
                let json = response.json::<serde_json::Value>().await.ok()?;
                *self.models_dev.write().await = Some((std::time::Instant::now(), json.clone()));
                json
            }
        };
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

    pub async fn server_status(&self) -> ServerStatus {
        let running = self.server.read().await.is_some();
        self.status_with(running).await
    }

    pub async fn server_start(&self) -> anyhow::Result<ServerStatus> {
        let mut guard = self.server.write().await;
        if guard.is_some() {
            return Ok(self.status_with(true).await);
        }
        let port = self.config.read().await.port;
        let app = crate::proxy::router(self.config.clone(), self.stats.clone());
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });
        *guard = Some(ServerHandle { shutdown: tx });
        tracing::info!(port, "proxy listening on 127.0.0.1");
        drop(guard);
        self.maybe_auto_apply().await;
        Ok(self.status_with(true).await)
    }

    pub async fn server_stop(&self) -> anyhow::Result<ServerStatus> {
        let mut guard = self.server.write().await;
        if let Some(handle) = guard.take() {
            let _ = handle.shutdown.send(());
        }
        Ok(self.status_with(false).await)
    }

    async fn status_with(&self, running: bool) -> ServerStatus {
        let port = self.config.read().await.port;
        ServerStatus {
            running,
            port,
            url: running.then(|| format!("http://127.0.0.1:{port}/v1")),
        }
    }

    pub async fn codex_status(&self) -> codex::CodexStatus {
        let cfg = self.config.read().await.clone();
        // `codex::status` probes the CLI with a blocking `codex --version`;
        // keep it off the async executor (it runs on every screen open).
        tokio::task::spawn_blocking(move || codex::status(&cfg))
            .await
            .unwrap_or_else(|e| {
                // JoinError only happens if the probe panicked; report a
                // degraded status instead of panicking the command.
                tracing::warn!("codex status probe failed: {e}");
                codex::status(&crate::config::AppConfig::default())
            })
    }

    pub async fn codex_apply(&self) -> anyhow::Result<()> {
        let cfg = self.config.read().await.clone();
        // `codex::apply` shells out to `codex debug models` and rewrites two
        // files; keep it off the async executor like `codex_status` does.
        tokio::task::spawn_blocking(move || codex::apply(&cfg, cfg.port)).await??;
        self.config.write().await.codex_integration = true;
        self.persist().await
    }

    pub async fn codex_remove(&self) -> anyhow::Result<()> {
        let cfg = self.config.read().await.clone();
        codex::remove(Some(&cfg))?;
        {
            let mut cfg = self.config.write().await;
            cfg.codex_integration = false;
            // The backup has been handed back to Codex, so holding on to it
            // would overwrite whatever the user picks next time.
            cfg.codex_model_backup = None;
        }
        self.persist().await
    }

    /// Pick the model Codex starts new sessions with, as a canonical
    /// `provider/model` slug (`None` hands the choice back to Codex).
    ///
    /// Only persists here; the key itself is written to `config.toml` by
    /// `codex::apply`, which `maybe_auto_apply` runs when the integration is
    /// on. Codex reads it at startup, so a running Codex keeps its model.
    pub async fn set_active_model(&self, slug: Option<String>) -> anyhow::Result<()> {
        // Read what Codex is on now *before* taking the write lock: this
        // touches the disk, and it is what makes the choice reversible.
        let displaced = if self.config.read().await.codex_model_backup.is_none() {
            codex::current_root_model()
        } else {
            None
        };
        {
            let mut cfg = self.config.write().await;
            if let Some(previous) = displaced {
                // Only somebody else's model is worth remembering; one of
                // ours is not a state the user asked for.
                if !codex::owns_slug(&cfg, &previous) {
                    cfg.codex_model_backup = Some(previous);
                }
            }
            if let Some(slug) = &slug {
                let (provider_id, model_id) = slug
                    .split_once('/')
                    .ok_or_else(|| anyhow::anyhow!("expected a 'provider/model' slug"))?;
                let known = cfg.providers.get(provider_id).is_some_and(|p| {
                    p.enabled && p.models.iter().any(|m| m.enabled && m.id == model_id)
                });
                anyhow::ensure!(known, "unknown or disabled model '{slug}'");
            }
            cfg.active_model = slug;
        }
        self.persist().await?;
        self.maybe_auto_apply().await;
        Ok(())
    }

    /// Enable or disable a whole provider at once (its models disappear from
    /// the published catalog and the proxy stops routing to it).
    pub async fn set_provider_enabled(&self, id: &str, enabled: bool) -> anyhow::Result<()> {
        {
            let mut cfg = self.config.write().await;
            let provider = cfg
                .providers
                .get_mut(id)
                .ok_or_else(|| anyhow::anyhow!("unknown provider '{id}'"))?;
            provider.enabled = enabled;
        }
        self.persist().await?;
        self.maybe_auto_apply().await;
        Ok(())
    }

    /// Is LoomRouter "on"? Only when both halves are up: the proxy is
    /// listening *and* Codex is pointed at it. A half-up state reads as off,
    /// because that is what it means for the user — Codex is not routed.
    pub async fn power_state(&self) -> bool {
        let running = self.server_status().await.running;
        let integrated = self.config.read().await.codex_integration;
        running && integrated
    }

    /// Flip both halves at once, returning the state settled on.
    ///
    /// The *decision* is taken under the lock as well as the transition:
    /// reading the state first and locking after lets two fast clicks both
    /// see "off" and one of them is silently swallowed.
    pub async fn power_toggle(&self) -> anyhow::Result<bool> {
        let _turn = self.power.lock().await;
        if self.power_state().await {
            self.power_off_locked().await?;
            Ok(false)
        } else {
            self.power_on_locked().await?;
            Ok(true)
        }
    }

    /// Start the proxy and point Codex at it, as one operation.
    ///
    /// A failed `codex_apply` rolls the proxy back when we were the ones who
    /// started it, so a failed toggle never leaves a half-on state behind.
    pub async fn power_on(&self) -> anyhow::Result<()> {
        let _turn = self.power.lock().await;
        self.power_on_locked().await
    }

    /// Unpoint Codex and stop the proxy.
    pub async fn power_off(&self) -> anyhow::Result<()> {
        let _turn = self.power.lock().await;
        self.power_off_locked().await
    }

    async fn power_on_locked(&self) -> anyhow::Result<()> {
        let was_running = self.server_status().await.running;
        self.server_start().await?;
        if let Err(e) = self.codex_apply().await {
            if !was_running {
                let _ = self.server_stop().await;
            }
            return Err(e);
        }
        Ok(())
    }

    /// Codex is unpointed first, and the proxy is only stopped once that
    /// succeeded: a Codex still aimed at a port nobody is listening on is
    /// the one end state that breaks the user's next session outright.
    async fn power_off_locked(&self) -> anyhow::Result<()> {
        self.codex_remove().await?;
        self.server_stop().await?;
        Ok(())
    }

    /// Route Codex side/auxiliary calls (thread titles, probes) to a
    /// cheap/free "provider/model" slug. Persisted only; the proxy reads it
    /// live from the shared config.
    pub async fn set_side_call_fallback(&self, model: Option<String>) -> anyhow::Result<()> {
        self.config.write().await.side_call_fallback = model;
        self.persist().await
    }

    /// Toggle native slug mode (see codex.rs module docs). The merged
    /// catalog changes shape (bare slugs, no OpenAI-auth requirement), so
    /// re-apply the integration when it is active; a failed re-apply is
    /// logged by `maybe_auto_apply` and never blocks saving the preference.
    pub async fn set_native_slug_mode(&self, enabled: bool) -> anyhow::Result<()> {
        self.config.write().await.native_slug_mode = enabled;
        self.persist().await?;
        self.maybe_auto_apply().await;
        Ok(())
    }

    /// Mark the first-run walkthrough as finished, so it is not shown again.
    /// Called when the user reaches the end of it or skips the optional step.
    pub async fn complete_onboarding(&self) -> anyhow::Result<()> {
        self.config.write().await.complete_onboarding();
        self.persist().await
    }

    /// Fetch balance/quota for every enabled provider (best effort per
    /// provider; failures are reported inline, never fatal). Providers are
    /// probed concurrently so N slow providers don't serialize into N ×
    /// timeout of wall-clock latency.
    pub async fn provider_balances(&self) -> Vec<ProviderBalance> {
        let cfg = self.config.read().await.clone();
        let probes = cfg
            .providers
            .values()
            .filter(|p| p.enabled)
            .map(fetch_balance);
        futures::future::join_all(probes).await
    }
}

/// Shared HTTP client for provider probes: one connection pool and TLS
/// session cache for the whole app instead of rebuilding both per call.
fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .expect("failed to build shared HTTP client")
    })
}

/// One quota bar on the Overview card (e.g. "Weekly quota  52%").
#[derive(Debug, Clone, Serialize)]
pub struct QuotaBar {
    pub label: String,
    /// 0..100 remaining.
    pub percent: f64,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderBalance {
    pub provider_id: String,
    /// Credentials work (endpoint reachable and authorized).
    pub ok: bool,
    pub bars: Vec<QuotaBar>,
    pub balance_text: Option<String>,
    pub error: Option<String>,
}

fn quota_bar(label: &str, detail: &serde_json::Value) -> Option<QuotaBar> {
    let parse = |k: &str| {
        detail
            .get(k)
            .and_then(|v| {
                v.as_str()
                    .map(str::to_string)
                    .or_else(|| Some(v.to_string()))
            })
            .and_then(|s| s.trim_matches('"').parse::<f64>().ok())
    };
    let limit = parse("limit")?;
    let remaining = parse("remaining").unwrap_or(limit - parse("used").unwrap_or(0.0));
    if limit <= 0.0 {
        return None;
    }
    let reset = detail
        .get("resetTime")
        .and_then(serde_json::Value::as_str)
        .map(|s| s.chars().take(16).collect::<String>())
        .unwrap_or_default();
    Some(QuotaBar {
        label: label.to_string(),
        percent: (remaining / limit * 100.0).clamp(0.0, 100.0),
        detail: format!(
            "{} / {} left{}",
            remaining as u64,
            limit as u64,
            if reset.is_empty() {
                String::new()
            } else {
                format!(" · resets {reset}")
            }
        ),
    })
}

async fn fetch_balance(p: &crate::config::Provider) -> ProviderBalance {
    use crate::proxy::ProviderFamily;
    let base = p.base_url.trim_end_matches('/').to_string();
    let mut result = ProviderBalance {
        provider_id: p.id.clone(),
        ok: false,
        bars: Vec::new(),
        balance_text: None,
        error: None,
    };
    // claude-code has no remote quota/balance endpoint: report the local
    // CLI's subscription login instead (the credential health of the plan).
    if p.id == crate::providers::CLAUDE_CODE_PROVIDER_ID {
        let status = crate::claude_cli::auth_status().await;
        result.ok = status.logged_in;
        if status.logged_in {
            result.balance_text = status.plan.or(status.subscription_type).or(status.email);
        } else {
            result.error = status.error;
        }
        return result;
    }
    let client = http_client();
    let get = |url: String| {
        // Protocol-correct auth (Anthropic: x-api-key + anthropic-version;
        // others: Authorization bearer) shared with the proxy. A balance
        // probe belongs to no model, so the provider's own dialect applies.
        let mut req = crate::proxy::apply_provider_auth(client.get(&url), p, None)
            // Per-probe timeout tighter than the client default.
            .timeout(std::time::Duration::from_secs(10));
        if let Some(ua) = &p.user_agent {
            req = req.header("user-agent", ua);
        }
        req
    };

    match crate::proxy::family_of(p) {
        // Kimi Code quota: weekly allowance + rolling 5-hour window. Only
        // the Coding Plan endpoint exposes /usages; other Kimi-family
        // endpoints fall through to the credential-health probe.
        ProviderFamily::Kimi if base.contains("api.kimi.com/coding") => {
            match get(format!("{base}/usages")).send().await {
                Ok(res) if res.status().is_success() => {
                    result.ok = true;
                    if let Ok(body) = res.json::<serde_json::Value>().await {
                        if let Some(bar) = quota_bar("Weekly quota", &body["usage"]) {
                            result.bars.push(bar);
                        }
                        if let Some(window) = body["limits"].as_array().and_then(|a| a.first()) {
                            if let Some(bar) = quota_bar("5-hour window", &window["detail"]) {
                                result.bars.push(bar);
                            }
                        }
                    }
                }
                Ok(res) => result.error = Some(format!("usages returned {}", res.status())),
                Err(e) => result.error = Some(e.to_string()),
            }
        }
        ProviderFamily::OpenRouter => match get(format!("{base}/credits")).send().await {
            Ok(res) if res.status().is_success() => {
                result.ok = true;
                if let Ok(body) = res.json::<serde_json::Value>().await {
                    let credits = body
                        .pointer("/data/total_credits")
                        .and_then(serde_json::Value::as_f64)
                        .unwrap_or(0.0);
                    let used = body
                        .pointer("/data/total_usage")
                        .and_then(serde_json::Value::as_f64)
                        .unwrap_or(0.0);
                    result.balance_text = Some(format!("${:.2}", credits - used));
                }
            }
            Ok(res) => result.error = Some(format!("credits returned {}", res.status())),
            Err(e) => result.error = Some(e.to_string()),
        },
        ProviderFamily::DeepSeek => {
            let root = base.trim_end_matches("/v1");
            match get(format!("{root}/user/balance")).send().await {
                Ok(res) if res.status().is_success() => {
                    result.ok = true;
                    if let Ok(body) = res.json::<serde_json::Value>().await {
                        if let Some(info) = body["balance_infos"].as_array().and_then(|a| a.first())
                        {
                            let amount = info["total_balance"].as_str().unwrap_or("?");
                            let currency = info["currency"].as_str().unwrap_or("");
                            result.balance_text = Some(format!("{amount} {currency}"));
                        }
                    }
                }
                Ok(res) => result.error = Some(format!("balance returned {}", res.status())),
                Err(e) => result.error = Some(e.to_string()),
            }
        }
        _ => {
            // No known balance endpoint: report credential health only,
            // reusing the model-catalog probe.
            match list_models(p).await {
                Ok(_) => result.ok = true,
                Err(e) => result.error = Some(e.to_string()),
            }
        }
    }
    result
}

/// Public model-limit catalog (the same source OpenCode uses for its model
/// limits), used when a provider's own `/models` publishes no context
/// window.
const MODELS_DEV_URL: &str = "https://models.dev/api.json";

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
) -> std::collections::HashMap<String, crate::config::ProviderProtocol> {
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

async fn probe_model_dialect(
    provider: &crate::config::Provider,
    model: &str,
) -> anyhow::Result<crate::config::ProviderProtocol> {
    use crate::config::ProviderProtocol;

    // This provider executes through the local CLI, not an HTTP endpoint.
    if provider.id == crate::providers::CLAUDE_CODE_PROVIDER_ID {
        return Ok(ProviderProtocol::Anthropic);
    }

    let client = http_client();
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
/// only id/created/object/owned_by, which is what the models.dev
/// enrichment in `AppState::discover_models` covers.
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
    let url = format!("{}/models", p.base_url.trim_end_matches('/'));
    let client = http_client();
    // Protocol-correct auth shared with the proxy (Anthropic gets
    // x-api-key + anthropic-version; everything else a bearer token). The
    // catalog is the whole provider, not one model: provider dialect.
    let mut req = crate::proxy::apply_provider_auth(client.get(&url), p, None);
    if let Some(ua) = &p.user_agent {
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
    use crate::config::Provider;
    use serde_json::json;

    fn state_with_config(config: AppConfig) -> AppState {
        AppState {
            config: Arc::new(RwLock::new(config)),
            stats: Arc::new(RwLock::new(Stats::load())),
            server: RwLock::new(None),
            power: tokio::sync::Mutex::new(()),
            model_contexts: RwLock::new(std::collections::HashMap::new()),
            models_dev: RwLock::new(None),
            test_config_path: None,
        }
    }

    #[tokio::test]
    async fn set_model_vision_rejects_unknown_provider_and_model() {
        // Creating an entry here would silently turn a typo into a persisted
        // model selection, unlike the explicit model toggle workflow.
        let state = state_with_config(AppConfig::default());
        let err = state
            .set_model_vision("unknown", "anything", true)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "unknown provider 'unknown'");

        let mut config = AppConfig::default();
        let provider = Provider::from_preset(
            crate::providers::PRESETS
                .iter()
                .find(|preset| preset.id == "deepseek")
                .unwrap(),
        );
        config.providers.insert(provider.id.clone(), provider);
        let state = state_with_config(config);
        let err = state
            .set_model_vision("deepseek", "unknown", true)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "unknown model 'unknown'");
    }

    #[test]
    fn entry_context_window_reads_each_catalog_dialect() {
        // OpenRouter: flat context_length…
        let flat = json!({"id":"a","context_length":1_048_576});
        assert_eq!(entry_context_window(&flat), Some(1_048_576));
        // …with top_provider as backup…
        let nested = json!({"id":"b","top_provider":{"context_length":200_000}});
        assert_eq!(entry_context_window(&nested), Some(200_000));
        // …models.dev-shaped limit.context…
        let models_dev = json!({"id":"c","limit":{"context":256_000,"output":64_000}});
        assert_eq!(entry_context_window(&models_dev), Some(256_000));
        // …and the common case: nothing published (OpenCode Go returns only
        // id/created/object/owned_by).
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
        // The gateway publishes two catalog entries. Both the merged provider
        // and the per-dialect ones it replaces must resolve, or models.dev
        // enrichment silently no-ops and its models fall back to 128k.
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
        // Providers whose slug already matches the catalog pass through.
        assert_eq!(models_dev_key("openrouter"), "openrouter");
        // The kimi-coding preset is published by models.dev under its
        // canonical catalog name, not the CLI-facing slug.
        assert_eq!(models_dev_key("kimi-coding"), "kimi-for-coding");
    }

    #[test]
    fn dialect_probe_requests_use_each_wire_format() {
        let (path, chat) =
            dialect_probe_request(&crate::config::ProviderProtocol::OpenAI, "kimi-k3");
        assert_eq!(path, "chat/completions");
        assert_eq!(chat["model"], "kimi-k3");
        assert_eq!(chat["messages"][0]["content"], "Reply with OK.");

        let (path, anthropic) =
            dialect_probe_request(&crate::config::ProviderProtocol::Anthropic, "qwen3.8-max");
        assert_eq!(path, "messages");
        assert_eq!(anthropic["model"], "qwen3.8-max");
        assert_eq!(anthropic["max_tokens"], 16);

        let (path, responses) =
            dialect_probe_request(&crate::config::ProviderProtocol::Responses, "gpt-5.6-sol");
        assert_eq!(path, "responses");
        assert_eq!(responses["model"], "gpt-5.6-sol");
        assert_eq!(responses["input"], "Reply with OK.");
    }

    #[test]
    fn detected_dialect_prefers_the_provider_default_when_several_work() {
        use crate::config::ProviderProtocol;

        let detected = select_detected_dialect(
            ProviderProtocol::Responses,
            &[ProviderProtocol::OpenAI, ProviderProtocol::Responses],
        );

        assert_eq!(detected, Some(ProviderProtocol::Responses));
    }

    #[test]
    fn discovered_dialect_does_not_replace_a_known_model_protocol() {
        use crate::config::ProviderProtocol;

        assert_eq!(
            persisted_model_protocol(
                Some(&ProviderProtocol::Responses),
                Some(&ProviderProtocol::Anthropic),
            ),
            Some(ProviderProtocol::Responses),
        );
    }

    #[tokio::test]
    async fn startup_catalog_repair_regenerates_an_enabled_integration() {
        // This catches a regression where startup notices the integration but
        // leaves its generated model catalog stale. A newly shipped native
        // model then cannot reach Codex's picker until the user clicks Apply.
        let config = AppConfig {
            codex_integration: true,
            port: 4242,
            ..AppConfig::default()
        };
        let state = state_with_config(config);
        let regenerated = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = regenerated.clone();

        let repaired = state
            .repair_codex_integration_with(move |config, port| {
                assert!(config.codex_integration);
                assert_eq!(port, 4242);
                observed.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            })
            .await
            .unwrap();

        assert!(repaired);
        assert!(regenerated.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn startup_catalog_repair_leaves_disabled_integration_untouched() {
        let state = state_with_config(AppConfig::default());
        let repaired = state
            .repair_codex_integration_with(|_, _| -> anyhow::Result<()> {
                panic!("disabled integrations must not rewrite Codex configuration")
            })
            .await
            .unwrap();

        assert!(!repaired);
    }
}
