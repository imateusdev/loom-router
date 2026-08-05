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
pub mod stats;
pub mod translate;

use state::AppState;
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent},
    Manager,
};

/// Show and focus the main window (used by the tray icon and its menu).
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Build the system-tray icon: left click restores the window, right click
/// opens a menu. The menu starts with two disabled, informational items
/// (request activity) refreshed by a background task; "Show LoomRouter"
/// and "Quit" follow a separator.
fn setup_tray(app: &tauri::App) -> tauri::Result<()> {
    // Informational items: disabled (non-clickable) and rewritten by
    // refresh_tray_activity().
    let hour = MenuItem::with_id(
        app,
        "tray-hour",
        "Requests (last hour): 0",
        false,
        None::<&str>,
    )?;
    let last = MenuItem::with_id(app, "tray-last", "No requests yet", false, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let show = MenuItem::with_id(app, "show", "Show LoomRouter", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&hour, &last, &separator, &show, &quit])?;

    // Clones for the tray-event closure (menu items are Arc-backed and
    // cheap to clone; all clones share the same native item).
    let hour_click = hour.clone();
    let last_click = last.clone();

    let mut builder = TrayIconBuilder::new()
        .tooltip("LoomRouter")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_main_window(app),
            // AppHandle::exit() does not fire CloseRequested, so this is a
            // real quit (the window is not just hidden again).
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(move |tray, event| {
            // Best-effort freshness on interaction: a right click opens
            // the menu immediately (the OS owns that path, so there is no
            // "menu about to open" hook), but this refresh usually lands
            // before the next open; the periodic task below bounds the
            // staleness to ~15s either way.
            if matches!(
                event,
                TrayIconEvent::Click {
                    button_state: MouseButtonState::Up,
                    ..
                }
            ) {
                let app = tray.app_handle().clone();
                let tray = tray.clone();
                let hour = hour_click.clone();
                let last = last_click.clone();
                tauri::async_runtime::spawn(async move {
                    refresh_tray_activity(&app, &hour, &last, &tray).await;
                });
            }
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    let tray = builder.build(app)?;

    // Periodic activity refresh: keeps the menu/tooltip fresh while the
    // window stays hidden. refresh_tray_activity() never panics (every
    // failure is logged), so this task only ends with the runtime itself.
    let handle = app.handle().clone();
    tauri::async_runtime::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
        loop {
            // The first interval tick fires immediately, so the tray is
            // populated once at startup and then every 15s.
            tick.tick().await;
            refresh_tray_activity(&handle, &hour, &last, &tray).await;
        }
    });
    Ok(())
}

/// Human-friendly relative time: "5s ago", "3m ago", "2h ago", "1d ago".
fn time_ago(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3_600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3_600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// Pull the latest stats and rewrite the tray menu items and tooltip.
/// Every failure is logged and swallowed — the tray keeps its previous
/// text and the next tick tries again.
async fn refresh_tray_activity(
    app: &tauri::AppHandle,
    hour_item: &MenuItem<tauri::Wry>,
    last_item: &MenuItem<tauri::Wry>,
    tray: &TrayIcon,
) {
    let state = app.state::<AppState>();
    let stats = state.stats.read().await;
    // Note: summarize() counts successful (status = "ok") requests only.
    let summary = stats.summarize(3_600);
    let recent = stats.recent(1);
    drop(stats);

    let hour_text = format!("Requests (last hour): {}", summary.requests);
    let last_text = match recent.first() {
        Some(e) => {
            let age = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs().saturating_sub(e.ts))
                .unwrap_or(0);
            format!(
                "Last: {}/{} · {} · {}",
                e.provider,
                e.model,
                if e.status == "ok" { "ok" } else { "err" },
                time_ago(age)
            )
        }
        None => "No requests yet".to_string(),
    };
    // Keep the tooltip short: Windows caps tray tooltips at ~128 chars.
    let tooltip = format!("LoomRouter — {} req/h", summary.requests);

    if let Err(e) = hour_item.set_text(&hour_text) {
        tracing::warn!("tray menu update failed: {e}");
    }
    if let Err(e) = last_item.set_text(&last_text) {
        tracing::warn!("tray menu update failed: {e}");
    }
    if let Err(e) = tray.set_tooltip(Some(&tooltip)) {
        tracing::warn!("tray tooltip update failed: {e}");
    }
}

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
        .setup(|app| {
            setup_tray(app)?;
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
            commands::stats_summary,
            commands::recent_requests,
            commands::provider_balances,
            commands::agents_list,
            commands::agents_upsert,
            commands::agents_delete,
            commands::set_side_call_fallback,
            commands::set_native_slug_mode,
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
        let mut cfg = state.config.read().await.clone();
        // Never hand real API keys to the webview: blank them out and
        // expose only `has_key`. On save, an empty key means "keep the
        // existing one" (see AppState::save_provider).
        for p in cfg.providers.values_mut() {
            p.has_key = p.api_key.as_deref().map(|k| !k.is_empty()).unwrap_or(false);
            p.api_key = Some(String::new());
        }
        Ok(cfg)
    }

    #[tauri::command]
    pub async fn save_provider(
        state: State<'_, AppState>,
        provider: Provider,
    ) -> Result<(), String> {
        state
            .save_provider(provider)
            .await
            .map_err(|e| e.to_string())
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

    #[tauri::command]
    pub async fn stats_summary(
        state: State<'_, AppState>,
        period_secs: u64,
    ) -> Result<crate::stats::StatsSummary, String> {
        Ok(state.stats.read().await.summarize(period_secs))
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
}
