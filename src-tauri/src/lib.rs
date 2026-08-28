// LoomRouter — weave any model into your coding agent's picker.
//
// Core library: provider registry, local proxy, protocol translation,
// and agent catalog integration (Codex first).

pub mod claude_cli;
mod cli_locator;
pub mod codex;
pub mod config;
pub mod keypool;
pub mod providers;
pub mod proxy;
pub mod secure_fs;
pub mod sse;
pub mod state;
pub mod stats;
pub mod tooling;
pub mod translate;
pub mod visual;
mod wake_lock;

mod tray;

use state::AppState;
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "loom_router=info".into()),
        )
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(AppState::load())
        .setup(|app| {
            tray::setup(app)?;
            // Keep the orchestrator skill in sync with the current roster at
            // every launch: it is otherwise only rewritten on agent
            // upsert/delete, so skill-text improvements in a new release
            // would never reach existing installs.
            if let Err(e) = codex::sync_orchestrator_skill() {
                tracing::warn!("orchestrator skill sync failed at startup: {e}");
            }
            // The app exists to run the proxy: start it on launch so Codex
            // works as soon as the window (or just the tray icon) is up.
            // A bind failure (e.g. port already taken) is logged, never
            // fatal — the UI still offers a manual Start.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state = handle.state::<AppState>();
                // A config whose provider ids were rewritten on load has to
                // reach disk and reach Codex before the first request: the
                // slug Codex holds still names a provider that is gone.
                if let Err(e) = state.persist_migration().await {
                    tracing::warn!("persisting the migrated config failed: {e}");
                }
                // If Codex isn't installed yet, fetch it before trying to
                // repair the integration. The installer can block, so it
                // runs off the async thread; a failure is only a warning
                // because the UI can still show the state and manual retry.
                match tokio::task::spawn_blocking(codex::ensure_codex_cli).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => tracing::warn!("ensuring the Codex CLI failed at startup: {e}"),
                    Err(e) => tracing::warn!("ensuring the Codex CLI task failed at startup: {e}"),
                }
                // Codex can ship a new native model between LoomRouter
                // launches. Refresh our generated files before the proxy
                // starts so the picker cannot remain stuck on an old capture.
                state.repair_codex_integration().await;
                if let Err(e) = state.server_start().await {
                    tracing::warn!("proxy autostart failed: {e}");
                }
                // The tray was built before this ran, so without a rebuild
                // it would keep claiming the proxy is stopped.
                crate::tray::rebuild(&handle);
            });
            Ok(())
        })
        // Closing the window hides it to the system tray instead of
        // quitting; only the tray menu's "Quit" exits the app.
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::claude_auth_status,
            commands::save_provider,
            commands::set_provider_rotation,
            commands::delete_provider,
            commands::discover_models,
            commands::validate_provider,
            commands::toggle_model,
            commands::set_model_protocol,
            commands::set_visual_assistance,
            commands::set_model_vision,
            commands::server_status,
            commands::server_start,
            commands::server_stop,
            commands::codex_status,
            commands::codex_native_models,
            commands::codex_apply,
            commands::codex_remove,
            commands::stats_summary,
            commands::recent_requests,
            commands::provider_balances,
            commands::agents_list,
            commands::agents_upsert,
            commands::agents_delete,
            commands::agent_templates,
            commands::multi_agent_status,
            commands::set_multi_agent,
            commands::set_side_call_fallback,
            commands::set_native_slug_mode,
            commands::set_sleep_prevention,
            commands::set_native_model_context_override,
            commands::clear_native_model_context_override,
            commands::set_onboarding_step,
            commands::detect_tools,
            commands::import_opencode_gateway,
            commands::import_claude_code,
            commands::setup_status,
            commands::complete_onboarding,
            commands::context_windows,
            commands::set_active_model,
            commands::set_provider_enabled,
        ])
        .run(tauri::generate_context!())
        .expect("error while running LoomRouter");
}

// Tauri commands live in lib.rs-adjacent module to keep the boundary thin.
pub mod commands {
    use crate::config::{AppConfig, Provider, SleepPreventionMode, VisualAssistanceConfig};
    use crate::state::{AppState, ServerStatus, SetupStatus};
    use tauri::State;

    #[tauri::command]
    pub async fn get_config(state: State<'_, AppState>) -> Result<AppConfig, String> {
        let mut cfg = state.config.read().await.clone();
        // Never hand real API keys to the webview: blank them out and
        // expose only `has_key`. On save, an empty key means "keep the
        // existing one" (see AppState::save_provider).
        cfg.sanitize_for_frontend();
        Ok(cfg)
    }

    #[tauri::command]
    pub async fn claude_auth_status() -> crate::claude_cli::ClaudeAuthStatus {
        crate::claude_cli::auth_status().await
    }

    #[tauri::command]
    pub async fn save_provider(
        app: tauri::AppHandle,
        state: State<'_, AppState>,
        provider: Provider,
    ) -> Result<(), String> {
        let result = state
            .save_provider(provider)
            .await
            .map_err(|e| e.to_string());
        crate::tray::rebuild(&app);
        result
    }

    #[tauri::command]
    pub async fn set_provider_rotation(
        app: tauri::AppHandle,
        state: State<'_, AppState>,
        provider_id: String,
        enabled: bool,
    ) -> Result<(), String> {
        let result = state
            .set_provider_rotation(&provider_id, enabled)
            .await
            .map_err(|e| e.to_string());
        crate::tray::rebuild(&app);
        result
    }

    #[tauri::command]
    pub async fn delete_provider(
        app: tauri::AppHandle,
        state: State<'_, AppState>,
        id: String,
    ) -> Result<(), String> {
        let result = state.delete_provider(&id).await.map_err(|e| e.to_string());
        crate::tray::rebuild(&app);
        result
    }

    #[tauri::command]
    pub async fn discover_models(
        state: State<'_, AppState>,
        provider_id: String,
    ) -> Result<Vec<String>, String> {
        state
            .discover_models(&provider_id)
            .await
            .map_err(|e| e.to_string())
    }

    /// Validate an API key by fetching the provider's live model catalog.
    /// Works for providers that are not saved yet (Add dialog).
    #[tauri::command]
    pub async fn validate_provider(provider: Provider) -> Result<Vec<String>, String> {
        crate::state::list_models(&provider)
            .await
            .map_err(|e| e.to_string())
    }

    /// `protocol: None` puts the model back on the provider's own dialect.
    #[tauri::command]
    pub async fn set_model_protocol(
        state: State<'_, AppState>,
        provider_id: String,
        model: String,
        protocol: Option<crate::config::ProviderProtocol>,
    ) -> Result<(), String> {
        state
            .set_model_protocol(&provider_id, &model, protocol)
            .await
            .map_err(|e| e.to_string())
    }

    #[tauri::command]
    pub async fn set_visual_assistance(
        state: State<'_, AppState>,
        config: VisualAssistanceConfig,
    ) -> Result<(), String> {
        set_visual_assistance_command(state.inner(), config).await
    }

    async fn set_visual_assistance_command(
        state: &AppState,
        config: VisualAssistanceConfig,
    ) -> Result<(), String> {
        state
            .set_visual_assistance(config)
            .await
            .map_err(|e| e.to_string())
    }

    #[tauri::command]
    pub async fn set_model_vision(
        state: State<'_, AppState>,
        provider_id: String,
        model: String,
        supports: bool,
    ) -> Result<(), String> {
        set_model_vision_command(state.inner(), provider_id, model, supports).await
    }

    async fn set_model_vision_command(
        state: &AppState,
        provider_id: String,
        model: String,
        supports: bool,
    ) -> Result<(), String> {
        state
            .set_model_vision(&provider_id, &model, supports)
            .await
            .map_err(|e| e.to_string())
    }

    #[tauri::command]
    pub async fn toggle_model(
        app: tauri::AppHandle,
        state: State<'_, AppState>,
        provider_id: String,
        model: String,
        enabled: bool,
    ) -> Result<(), String> {
        let result = state
            .toggle_model(&provider_id, &model, enabled)
            .await
            .map_err(|e| e.to_string());
        crate::tray::rebuild(&app);
        result
    }

    #[tauri::command]
    pub async fn server_status(state: State<'_, AppState>) -> Result<ServerStatus, String> {
        Ok(state.server_status().await)
    }

    #[tauri::command]
    pub async fn server_start(
        app: tauri::AppHandle,
        state: State<'_, AppState>,
    ) -> Result<ServerStatus, String> {
        let status = state.server_start().await.map_err(|e| e.to_string());
        crate::tray::rebuild(&app);
        status
    }

    #[tauri::command]
    pub async fn server_stop(
        app: tauri::AppHandle,
        state: State<'_, AppState>,
    ) -> Result<ServerStatus, String> {
        let status = state.server_stop().await.map_err(|e| e.to_string());
        crate::tray::rebuild(&app);
        status
    }

    #[tauri::command]
    pub async fn codex_status(
        state: State<'_, AppState>,
    ) -> Result<crate::codex::CodexStatus, String> {
        Ok(state.codex_status().await)
    }

    #[tauri::command]
    pub async fn codex_native_models(state: State<'_, AppState>) -> Result<Vec<String>, String> {
        Ok(state.codex_native_models().await)
    }

    #[tauri::command]
    pub async fn codex_apply(
        app: tauri::AppHandle,
        state: State<'_, AppState>,
    ) -> Result<(), String> {
        let result = state.codex_apply().await.map_err(|e| e.to_string());
        crate::tray::rebuild(&app);
        result
    }

    #[tauri::command]
    pub async fn codex_remove(
        app: tauri::AppHandle,
        state: State<'_, AppState>,
    ) -> Result<(), String> {
        let result = state.codex_remove().await.map_err(|e| e.to_string());
        crate::tray::rebuild(&app);
        result
    }

    #[tauri::command]
    pub async fn stats_summary(
        state: State<'_, AppState>,
        period_secs: u64,
    ) -> Result<crate::stats::StatsSummary, String> {
        let mut summary = state.stats.read().await.summarize(period_secs);
        let config = state.config.read().await;
        for usage in &mut summary.per_key {
            usage.key_name = config
                .providers
                .values()
                .flat_map(|provider| provider.keys.iter())
                .find(|key| key.id == usage.key_id)
                .map(|key| key.name.clone())
                .unwrap_or_default();
        }
        Ok(summary)
    }

    #[tauri::command]
    pub async fn recent_requests(
        state: State<'_, AppState>,
        limit: Option<u32>,
    ) -> Result<Vec<crate::stats::RequestEntry>, String> {
        Ok(state.stats.read().await.recent(limit.unwrap_or(100)))
    }

    #[tauri::command]
    pub async fn provider_balances(
        state: State<'_, AppState>,
    ) -> Result<Vec<crate::state::ProviderBalance>, String> {
        Ok(state.provider_balances().await)
    }

    #[tauri::command]
    pub async fn agents_list() -> Result<Vec<crate::codex::AgentInfo>, String> {
        crate::codex::agents_list().map_err(|e| e.to_string())
    }

    #[tauri::command]
    pub async fn agents_upsert(agent: crate::codex::AgentInfo) -> Result<(), String> {
        crate::codex::agents_upsert(&agent).map_err(|e| e.to_string())
    }

    #[tauri::command]
    pub async fn agents_delete(name: String) -> Result<(), String> {
        crate::codex::agents_delete(&name).map_err(|e| e.to_string())
    }

    #[tauri::command]
    pub async fn agent_templates() -> Result<Vec<crate::codex::AgentTemplate>, String> {
        Ok(crate::codex::agent_templates())
    }

    #[tauri::command]
    pub async fn multi_agent_status() -> Result<bool, String> {
        Ok(crate::codex::multi_agent_enabled())
    }

    #[tauri::command]
    pub async fn set_multi_agent(enabled: bool) -> Result<bool, String> {
        crate::codex::set_multi_agent(enabled).map_err(|e| e.to_string())
    }

    #[tauri::command]
    pub async fn set_side_call_fallback(
        state: State<'_, AppState>,
        model: Option<String>,
    ) -> Result<(), String> {
        state
            .set_side_call_fallback(model)
            .await
            .map_err(|e| e.to_string())
    }

    #[tauri::command]
    pub async fn set_native_slug_mode(
        state: State<'_, AppState>,
        enabled: bool,
    ) -> Result<(), String> {
        state
            .set_native_slug_mode(enabled)
            .await
            .map_err(|e| e.to_string())
    }

    #[tauri::command]
    pub async fn set_sleep_prevention(
        state: State<'_, AppState>,
        mode: SleepPreventionMode,
    ) -> Result<(), String> {
        state
            .set_sleep_prevention(mode)
            .await
            .map_err(|e| e.to_string())
    }

    #[tauri::command]
    pub async fn set_native_model_context_override(
        state: State<'_, AppState>,
        model: String,
        context_window: u32,
    ) -> Result<(), String> {
        state
            .set_native_model_context_override(&model, context_window)
            .await
            .map_err(|e| e.to_string())
    }

    #[tauri::command]
    pub async fn clear_native_model_context_override(
        state: State<'_, AppState>,
        model: String,
    ) -> Result<(), String> {
        state
            .clear_native_model_context_override(&model)
            .await
            .map_err(|e| e.to_string())
    }

    #[tauri::command]
    pub async fn set_onboarding_step(
        state: State<'_, AppState>,
        step: String,
    ) -> Result<(), String> {
        state
            .set_onboarding_step(&step)
            .await
            .map_err(|e| e.to_string())
    }

    #[tauri::command]
    pub async fn detect_tools(
        state: State<'_, AppState>,
    ) -> Result<crate::tooling::ToolDetection, String> {
        Ok(state.detect_tools().await)
    }

    #[tauri::command]
    pub async fn import_opencode_gateway(
        app: tauri::AppHandle,
        state: State<'_, AppState>,
        gateway_id: String,
    ) -> Result<(), String> {
        state
            .import_opencode_gateway(&gateway_id)
            .await
            .map_err(|error| error.to_string())?;
        crate::tray::rebuild(&app);
        Ok(())
    }

    #[tauri::command]
    pub async fn import_claude_code(
        app: tauri::AppHandle,
        state: State<'_, AppState>,
    ) -> Result<(), String> {
        state
            .import_claude_code()
            .await
            .map_err(|error| error.to_string())?;
        crate::tray::rebuild(&app);
        Ok(())
    }

    #[tauri::command]
    pub async fn setup_status(state: State<'_, AppState>) -> Result<SetupStatus, String> {
        Ok(state.setup_status().await)
    }

    /// Pick the model Codex starts new sessions with. `None` (or an empty
    /// slug) hands the choice back to Codex.
    #[tauri::command]
    pub async fn set_active_model(
        app: tauri::AppHandle,
        state: State<'_, AppState>,
        slug: Option<String>,
    ) -> Result<(), String> {
        let slug = slug.filter(|s| !s.is_empty());
        state
            .set_active_model(slug)
            .await
            .map_err(|e| e.to_string())?;
        crate::tray::rebuild(&app);
        Ok(())
    }

    #[tauri::command]
    pub async fn set_provider_enabled(
        app: tauri::AppHandle,
        state: State<'_, AppState>,
        id: String,
        enabled: bool,
    ) -> Result<(), String> {
        state
            .set_provider_enabled(&id, enabled)
            .await
            .map_err(|e| e.to_string())?;
        crate::tray::rebuild(&app);
        Ok(())
    }

    #[tauri::command]
    pub async fn complete_onboarding(state: State<'_, AppState>) -> Result<(), String> {
        state.complete_onboarding().await.map_err(|e| e.to_string())
    }

    /// Context window per configured model, keyed by `provider/model`.
    ///
    /// Read from `codex::context_window_for` so the UI shows exactly the
    /// window published to Codex, rather than a second guess at it.
    #[tauri::command]
    pub async fn context_windows(
        state: State<'_, AppState>,
    ) -> Result<std::collections::BTreeMap<String, crate::codex::ContextWindow>, String> {
        let cfg = state.config.read().await;
        Ok(cfg
            .providers
            .values()
            .flat_map(|p| {
                p.models.iter().map(move |m| {
                    (
                        format!("{}/{}", p.id, m.id),
                        crate::codex::context_window_for(p, &m.id),
                    )
                })
            })
            .collect())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::config::{ProviderKey, ProviderModel, ProviderProtocol};

        fn config_with_model() -> AppConfig {
            let mut config = AppConfig::default();
            config.providers.insert(
                "test".into(),
                crate::config::Provider {
                    id: "test".into(),
                    name: "Test".into(),
                    protocol: ProviderProtocol::OpenAI,
                    base_url: "https://test.invalid/v1".into(),
                    api_key: Some("key".into()),
                    keys: vec![ProviderKey {
                        id: "test-key".into(),
                        name: "Principal".into(),
                        enabled: true,
                        api_key: Some("key".into()),
                        has_key: true,
                    }],
                    rotation_enabled: false,
                    has_key: true,
                    context_window: None,
                    user_agent: None,
                    models: vec![ProviderModel {
                        id: "text-model".into(),
                        label: None,
                        context_window: None,
                        protocol: None,
                        enabled: true,
                        supports_vision: false,
                        fast_mode: false,
                    }],
                    enabled: true,
                },
            );
            config
        }

        #[tokio::test]
        async fn set_visual_assistance_command_persists_its_configuration() {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("config.json");
            let mut config = config_with_model();
            config.providers.get_mut("test").unwrap().models[0].supports_vision = true;
            let state = AppState::for_test(config, path.clone());
            set_visual_assistance_command(
                &state,
                VisualAssistanceConfig {
                    enabled: true,
                    assistant_model: Some("test/text-model".into()),
                    fallback_models: vec![],
                },
            )
            .await
            .unwrap();

            let saved: AppConfig =
                serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
            assert!(saved.visual_assistance.enabled);
            assert_eq!(
                saved.visual_assistance.assistant_model.as_deref(),
                Some("test/text-model")
            );
        }

        #[tokio::test]
        async fn set_visual_assistance_command_accepts_cataloged_vision_models_disabled_for_routing(
        ) {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("config.json");
            let mut config = config_with_model();
            let model = &mut config.providers.get_mut("test").unwrap().models[0];
            model.enabled = false;
            model.supports_vision = true;
            let state = AppState::for_test(config, path);

            set_visual_assistance_command(
                &state,
                VisualAssistanceConfig {
                    enabled: true,
                    assistant_model: Some("test/text-model".into()),
                    fallback_models: vec![],
                },
            )
            .await
            .unwrap();
        }

        #[tokio::test]
        async fn set_visual_assistance_command_requires_a_primary_only_when_enabled() {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("config.json");
            let state = AppState::for_test(config_with_model(), path);

            let error = set_visual_assistance_command(
                &state,
                VisualAssistanceConfig {
                    enabled: true,
                    assistant_model: None,
                    fallback_models: vec![],
                },
            )
            .await
            .unwrap_err();
            assert!(error.contains("primary visual assistant"));

            set_visual_assistance_command(
                &state,
                VisualAssistanceConfig {
                    enabled: false,
                    assistant_model: None,
                    fallback_models: vec![],
                },
            )
            .await
            .unwrap();
            let config = state.config.read().await;
            assert!(!config.visual_assistance.enabled);
            assert_eq!(config.visual_assistance.assistant_model, None);
        }

        #[tokio::test]
        async fn set_visual_assistance_command_rejects_responses_protocol_models() {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("config.json");
            let mut config = config_with_model();
            let model = &mut config.providers.get_mut("test").unwrap().models[0];
            model.supports_vision = true;
            model.protocol = Some(ProviderProtocol::Responses);
            let state = AppState::for_test(config, path);

            let error = set_visual_assistance_command(
                &state,
                VisualAssistanceConfig {
                    enabled: true,
                    assistant_model: Some("test/text-model".into()),
                    fallback_models: vec![],
                },
            )
            .await
            .unwrap_err();

            assert!(error.contains("unsupported Responses protocol"));
        }

        #[tokio::test]
        async fn set_model_vision_command_updates_persisted_model() {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("config.json");
            let state = AppState::for_test(config_with_model(), path.clone());
            set_model_vision_command(&state, "test".into(), "text-model".into(), true)
                .await
                .unwrap();

            let saved: AppConfig =
                serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
            assert!(saved.providers["test"].models[0].supports_vision);
        }

        #[tokio::test]
        async fn set_model_vision_command_rejects_unknown_provider_and_model() {
            let temp = tempfile::tempdir().unwrap();
            let state = AppState::for_test(config_with_model(), temp.path().join("config.json"));

            assert_eq!(
                set_model_vision_command(&state, "missing".into(), "text-model".into(), true).await,
                Err("unknown provider 'missing'".into())
            );
            assert_eq!(
                set_model_vision_command(&state, "test".into(), "missing".into(), true).await,
                Err("unknown model 'missing'".into())
            );
        }
    }
}
