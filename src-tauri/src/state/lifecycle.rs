//! Application lifecycle and persisted configuration mutations.

use super::{
    codex_is_active, derive_setup_status, fetch_balance, model_discovery, AppConfig, AppState,
    ProviderBalance, ServerHandle, ServerStatus, SetupStatus, SetupValidation, NEXT_SERVER_ID,
    WIZARD_STEPS,
};
use crate::codex;
use crate::config::VisualAssistanceConfig;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use tokio::sync::oneshot;

impl AppState {
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
    pub(super) async fn maybe_auto_apply(&self) {
        let cfg = self.config.read().await.clone();
        if !cfg.codex_integration {
            return;
        }
        // `codex::apply` shells out and rewrites two files — off the async
        // executor, same as every other call site.
        let port = cfg.port;
        match tokio::task::spawn_blocking(move || codex::apply(&cfg, port)).await {
            Ok(Ok(())) => {
                tracing::info!("Codex integration auto-applied after config change");
                // Applying refreshes Codex's native catalog, so stale slugs
                // must not survive into the next picker read.
                self.invalidate_native_slugs_cache().await;
            }
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
            Ok(true) => {
                tracing::info!("Codex integration catalog refreshed at startup");
                // Warmed startup integration can include new CLI models.
                self.invalidate_native_slugs_cache().await;
            }
            Ok(false) => {}
            Err(e) => tracing::warn!("startup repair of Codex integration failed: {e}"),
        }
    }

    pub(super) async fn repair_codex_integration_with<F>(
        &self,
        regenerate: F,
    ) -> anyhow::Result<bool>
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

    /// Replaces `keys` wholesale, which is what the key manager means when it
    /// deletes the last key. So a caller that builds a Provider without
    /// populating `keys` ERASES every stored credential: the Edit Provider
    /// dialog did exactly that and silently wiped them on a rename. Carry the
    /// existing key list through unless you mean to replace it.
    pub async fn save_provider(&self, mut provider: crate::config::Provider) -> anyhow::Result<()> {
        let mut cfg = self.config.write().await;
        provider.migrate_provider_keys();
        let existing_values: HashMap<String, String> = cfg
            .providers
            .get(&provider.id)
            .into_iter()
            .flat_map(|existing| existing.keys.iter())
            .filter_map(|key| {
                key.api_key
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .map(|value| (key.id.clone(), value.to_string()))
            })
            .collect();
        let mut seen_ids = HashSet::new();
        let mut seen_names = HashSet::new();
        for key in &mut provider.keys {
            key.name = key.name.trim().to_string();
            if key.name.is_empty() {
                anyhow::bail!("key name is required");
            }
            if !seen_names.insert(key.name.clone()) {
                anyhow::bail!("key name '{}' is already used", key.name);
            }
            if key.id.trim().is_empty() {
                key.id = uuid::Uuid::new_v4().to_string();
            }
            if !seen_ids.insert(key.id.clone()) {
                anyhow::bail!("duplicate key id '{}'", key.id);
            }
            // The UI never receives the real key back, so an empty key on save
            // means "keep the existing one" — never overwrite with empty.
            let keep_existing = key.api_key.as_deref().map(str::is_empty).unwrap_or(true);
            if keep_existing {
                if let Some(stored) = existing_values.get(&key.id) {
                    key.api_key = Some(stored.clone());
                } else {
                    anyhow::bail!("key value is required for new key '{}'", key.name);
                }
            }
            key.has_key = key
                .api_key
                .as_deref()
                .map(|value| !value.is_empty())
                .unwrap_or(false);
        }
        provider.has_key = provider.keys.iter().any(|key| {
            key.api_key
                .as_deref()
                .is_some_and(|value| !value.is_empty())
        });
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
                        supports_vision: true,
                    }
                });
            provider.models = seeded.collect();
        }
        let provider_id = provider.id.clone();
        let saved_keys = provider.keys.clone();
        cfg.providers.insert(provider_id.clone(), provider);
        drop(cfg);
        self.persist().await?;
        self.key_pools
            .prune_provider(&provider_id, &saved_keys)
            .await;
        for key in saved_keys.iter().filter(|key| {
            key.api_key
                .as_deref()
                .is_some_and(|value| !value.is_empty())
        }) {
            self.key_pools.record_success(&provider_id, &key.id).await;
        }
        self.maybe_auto_apply().await;
        Ok(())
    }

    pub async fn set_provider_rotation(
        &self,
        provider_id: &str,
        enabled: bool,
    ) -> anyhow::Result<()> {
        {
            let mut cfg = self.config.write().await;
            let Some(provider) = cfg.providers.get_mut(provider_id) else {
                anyhow::bail!("unknown provider '{provider_id}'");
            };
            provider.rotation_enabled = enabled;
        }
        self.persist().await?;
        self.maybe_auto_apply().await;
        Ok(())
    }

    fn opencode_path(&self) -> Option<std::path::PathBuf> {
        #[cfg(test)]
        if let Some(path) = &self.test_opencode_path {
            return Some(path.clone());
        }
        crate::tooling::opencode_config_path()
    }

    pub async fn detect_tools(&self) -> crate::tooling::ToolDetection {
        let config = self.config.read().await.clone();
        crate::tooling::detect_tools(&config, self.opencode_path().unwrap_or_default()).await
    }

    pub async fn import_opencode_gateway(&self, gateway_id: &str) -> anyhow::Result<()> {
        let _guard = self.tool_import.lock().await;
        if !crate::tooling::is_opencode_gateway(gateway_id) {
            anyhow::bail!("unknown OpenCode gateway '{gateway_id}'");
        }
        if self.config.read().await.providers.contains_key(gateway_id) {
            return Ok(());
        }
        let path = self
            .opencode_path()
            .ok_or_else(|| anyhow::anyhow!("OpenCode config directory is unavailable"))?;
        let gateway_id = gateway_id.to_string();
        let provider = tokio::task::spawn_blocking(move || {
            crate::tooling::provider_from_opencode(&path, &gateway_id)
        })
        .await
        .map_err(|error| anyhow::anyhow!("OpenCode import panicked: {error}"))??;
        self.save_provider(provider).await
    }

    pub async fn import_claude_code(&self) -> anyhow::Result<()> {
        let _guard = self.tool_import.lock().await;
        if self
            .config
            .read()
            .await
            .providers
            .contains_key(crate::providers::CLAUDE_CODE_PROVIDER_ID)
        {
            return Ok(());
        }
        let detection = crate::tooling::detect_claude(false).await;
        if !detection.detected || detection.logged_in != Some(true) {
            anyhow::bail!("Claude Code must be installed and logged in before import");
        }
        self.save_provider(crate::tooling::claude_provider()).await
    }

    pub async fn delete_provider(&self, id: &str) -> anyhow::Result<()> {
        self.config.write().await.providers.remove(id);
        self.persist().await?;
        self.key_pools.prune_provider(id, &[]).await;
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
            Some(model_discovery::probe_model_dialect(&provider, model).await?)
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
                m.supports_vision = true;
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
                supports_vision: provider.id == crate::providers::CLAUDE_CODE_PROVIDER_ID,
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
        let app = crate::proxy::router_with_pools_and_wake(
            self.config.clone(),
            self.stats.clone(),
            self.key_pools.clone(),
            self.wake.clone(),
        );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
        let (tx, rx) = oneshot::channel::<()>();
        let server_id = NEXT_SERVER_ID.fetch_add(1, Ordering::Relaxed);
        let server = self.server.clone();
        let wake = self.wake.clone();
        tokio::spawn(async move {
            let result = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
            if let Err(error) = result {
                tracing::error!(%error, "proxy server stopped unexpectedly");
            }

            let mut guard = server.write().await;
            let owns_wake_lock = match guard.as_ref() {
                Some(handle) if handle.id == server_id => {
                    guard.take();
                    true
                }
                None => true,
                Some(_) => false,
            };
            if owns_wake_lock {
                // Keep this send serialized with server_start's matching send,
                // or an old task can disable a replacement server after it starts.
                wake.set_proxy_running(false);
            }
            drop(guard);
        });
        *guard = Some(ServerHandle {
            id: server_id,
            shutdown: tx,
        });
        self.wake.set_proxy_running(true);
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

    pub async fn set_onboarding_step(&self, step: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            WIZARD_STEPS.contains(&step),
            "invalid onboarding step '{step}'"
        );
        let boundary_id = if step == "validate" {
            self.stats.read().await.latest_request_id()
        } else {
            None
        };
        let mut cfg = self.config.write().await;
        cfg.onboarding_step = Some(step.to_string());
        if step == "validate" && cfg.validation_started_at.is_none() {
            cfg.validation_started_at = Some(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_secs(),
            );
            cfg.validation_started_request_id = boundary_id;
        }
        drop(cfg);
        self.persist().await
    }

    pub async fn setup_status(&self) -> SetupStatus {
        let cfg = self.config.read().await.clone();
        let codex = self.codex_status().await;
        let codex_active = codex_is_active(&cfg, &codex);
        let needs_claude_probe = cfg.providers.values().any(|provider| {
            provider.enabled && provider.id == crate::providers::CLAUDE_CODE_PROVIDER_ID
        });
        let claude_logged_in = if needs_claude_probe {
            crate::claude_cli::auth_status().await.logged_in
        } else {
            false
        };
        let validation = match cfg.validation_started_at {
            Some(started_at) => {
                let (first_ok_request_at, failed_attempt) = self
                    .stats
                    .read()
                    .await
                    .validation_since(started_at, cfg.validation_started_request_id);
                SetupValidation {
                    started_at: Some(started_at),
                    first_ok_request_at,
                    failed_attempt,
                }
            }
            None => SetupValidation::default(),
        };
        derive_setup_status(&cfg, codex_active, claude_logged_in, &validation)
    }

    pub async fn codex_apply(&self) -> anyhow::Result<()> {
        let cfg = self.config.read().await.clone();
        // `codex::apply` shells out to `codex debug models` and rewrites two
        // files; keep it off the async executor like `codex_status` does.
        tokio::task::spawn_blocking(move || codex::apply(&cfg, cfg.port)).await??;
        // The apply refreshed the native catalog, so discard the old picker
        // result before reporting integration as enabled.
        self.invalidate_native_slugs_cache().await;
        // The catalog on disk just changed; the cached `codex doctor` verdict
        // describes the pre-apply state and would keep the panel red for the
        // rest of its TTL.
        codex::invalidate_merged_catalog_validation();
        self.config.write().await.codex_integration = true;
        self.persist().await
    }

    pub async fn codex_remove(&self) -> anyhow::Result<()> {
        let cfg = self.config.read().await.clone();
        codex::remove(Some(&cfg))?;
        codex::invalidate_merged_catalog_validation();
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

    pub async fn set_sleep_prevention(
        &self,
        mode: crate::config::SleepPreventionMode,
    ) -> anyhow::Result<()> {
        let previous = {
            let mut cfg = self.config.write().await;
            let previous = cfg.sleep_prevention;
            cfg.sleep_prevention = mode;
            previous
        };
        // Roll the in-memory field back on a write failure, or the UI reports a
        // mode the wake thread was never told about.
        if let Err(error) = self.persist().await {
            self.config.write().await.sleep_prevention = previous;
            return Err(error);
        }
        self.wake.set_mode(mode);
        Ok(())
    }

    /// Override the catalog window for one native Codex model. The bounds
    /// prevent a typo from making Codex retain an unbounded conversation or
    /// compact so early that the model is unusable.
    pub async fn set_native_model_context_override(
        &self,
        model: &str,
        context_window: u32,
    ) -> anyhow::Result<()> {
        if model.trim().is_empty() || model.trim() != model || model.contains('/') {
            anyhow::bail!("native model must be a non-empty bare slug");
        }
        if !(32_000..=2_000_000).contains(&context_window) {
            anyhow::bail!("context window must be between 32000 and 2000000 tokens");
        }
        self.config
            .write()
            .await
            .native_model_context_overrides
            .insert(model.to_string(), context_window);
        self.persist().await?;
        self.maybe_auto_apply().await;
        Ok(())
    }

    /// Return one native model to the window published by the Codex catalog.
    pub async fn clear_native_model_context_override(&self, model: &str) -> anyhow::Result<()> {
        if model.trim().is_empty() || model.trim() != model || model.contains('/') {
            anyhow::bail!("native model must be a non-empty bare slug");
        }
        self.config
            .write()
            .await
            .native_model_context_overrides
            .remove(model);
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
        let probes: Vec<_> = cfg
            .providers
            .values()
            .filter(|p| p.enabled)
            .flat_map(|p| {
                // Routing skips disabled keys, so probing them only spends a
                // request to render an "unreachable" card for a key nobody
                // uses. A provider whose keys are all off keeps its single
                // provider-level row: the card then reports the same failure
                // the proxy would, instead of silently leaving the dashboard.
                let enabled: Vec<_> = p.keys.iter().filter(|key| key.enabled).collect();
                if p.id == crate::providers::CLAUDE_CODE_PROVIDER_ID || enabled.is_empty() {
                    vec![fetch_balance(p, None)]
                } else {
                    enabled
                        .into_iter()
                        .map(|key| fetch_balance(p, Some(key)))
                        .collect()
                }
            })
            .collect();
        use futures::stream::{self, StreamExt};
        const MAX_PARALLEL_BALANCE_PROBES: usize = 4;
        // `buffered`, not `buffer_unordered`: the same concurrency cap, but
        // rows come back in config order. Completion order made the Overview
        // cards swap places on every refresh, depending on which provider
        // answered first.
        stream::iter(probes)
            .buffered(MAX_PARALLEL_BALANCE_PROBES)
            .collect()
            .await
    }
}
