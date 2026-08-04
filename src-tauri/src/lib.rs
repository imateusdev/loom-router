// LoomRouter — weave any model into your coding agent's picker.
//
// Core library: provider registry, local proxy, protocol translation,
// and agent catalog integration (Codex first).

pub mod codex;
pub mod config;
pub mod providers;
pub mod proxy;
pub mod sse;
pub mod state;
pub mod translate;

use state::AppState;

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
        .manage(AppState::load())
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::save_provider,
            commands::delete_provider,
            commands::discover_models,
            commands::validate_provider,
            commands::toggle_model,
            commands::server_status,
            commands::server_start,
            commands::server_stop,
            commands::codex_status,
            commands::codex_apply,
            commands::codex_remove,
        ])
        .run(tauri::generate_context!())
        .expect("error while running LoomRouter");
}

// Tauri commands live in lib.rs-adjacent module to keep the boundary thin.
pub mod commands {
    use crate::config::{AppConfig, Provider};
    use crate::state::{AppState, ServerStatus};
    use tauri::State;

    #[tauri::command]
    pub async fn get_config(state: State<'_, AppState>) -> Result<AppConfig, String> {
        Ok(state.config.read().await.clone())
    }

    #[tauri::command]
    pub async fn save_provider(
        state: State<'_, AppState>,
        provider: Provider,
    ) -> Result<(), String> {
        state.save_provider(provider).await.map_err(|e| e.to_string())
    }

    #[tauri::command]
    pub async fn delete_provider(state: State<'_, AppState>, id: String) -> Result<(), String> {
        state.delete_provider(&id).await.map_err(|e| e.to_string())
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

    #[tauri::command]
    pub async fn toggle_model(
        state: State<'_, AppState>,
        provider_id: String,
        model: String,
        enabled: bool,
    ) -> Result<(), String> {
        state
            .toggle_model(&provider_id, &model, enabled)
            .await
            .map_err(|e| e.to_string())
    }

    #[tauri::command]
    pub async fn server_status(state: State<'_, AppState>) -> Result<ServerStatus, String> {
        Ok(state.server_status().await)
    }

    #[tauri::command]
    pub async fn server_start(state: State<'_, AppState>) -> Result<ServerStatus, String> {
        state.server_start().await.map_err(|e| e.to_string())
    }

    #[tauri::command]
    pub async fn server_stop(state: State<'_, AppState>) -> Result<ServerStatus, String> {
        state.server_stop().await.map_err(|e| e.to_string())
    }

    #[tauri::command]
    pub async fn codex_status(
        state: State<'_, AppState>,
    ) -> Result<crate::codex::CodexStatus, String> {
        Ok(state.codex_status().await)
    }

    #[tauri::command]
    pub async fn codex_apply(state: State<'_, AppState>) -> Result<(), String> {
        state.codex_apply().await.map_err(|e| e.to_string())
    }

    #[tauri::command]
    pub async fn codex_remove(state: State<'_, AppState>) -> Result<(), String> {
        state.codex_remove().await.map_err(|e| e.to_string())
    }
}
