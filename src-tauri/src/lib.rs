// LoomRouter — weave any model into your coding agent's picker.
//
// Core library: provider registry, local proxy, protocol translation,
// and agent catalog integration (Codex first).

pub mod codex;
pub mod config;
pub mod providers;
pub mod proxy;
pub mod secure_fs;
pub mod sse;
pub mod state;
pub mod stats;
pub mod translate;

use config::AppConfig;
use state::{AppState, ServerStatus};
use tauri::{
    menu::{CheckMenuItem, IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu},
    tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent},
    Emitter, Manager, Wry,
};

/// Event the backend emits whenever it changes something the windows show
/// (the tray is a second author of the same state, so pages cannot keep
/// assuming a fetch-on-mount is still accurate).
const EVENT_STATE_CHANGED: &str = "loomrouter://state-changed";
/// Event carrying a route the UI should navigate to, e.g. "/providers".
const EVENT_NAVIGATE: &str = "loomrouter://navigate";

/// The pages the tray can jump to, as (menu id suffix, route, label).
const PAGES: &[(&str, &str, &str)] = &[
    ("overview", "/", "Overview"),
    ("providers", "/providers", "Providers"),
    ("logs", "/logs", "Logs"),
    ("server", "/server", "Server"),
    ("codex", "/codex", "Codex Integration"),
    ("agents", "/agents", "Agents"),
];

/// Live handles the periodic refresh writes into. Replaced wholesale on
/// every menu rebuild — the previous items belong to a menu that is no
/// longer attached, and writing to those is a silent no-op.
struct TrayHandles {
    tray: TrayIcon<Wry>,
    items: std::sync::Mutex<ActivityItems>,
    /// What went wrong on the last tray action, shown in the menu until the
    /// next one succeeds. Without it a failed toggle is invisible: the tray
    /// has no dialog, and the window may not even be open.
    last_error: std::sync::Mutex<Option<String>>,
    /// Fingerprint of the state the current menu was built from, so the
    /// heartbeat can skip a rebuild that would change nothing.
    signature: std::sync::Mutex<Option<String>>,
}

/// Everything `build_menu` reads that changes the menu's shape or labels.
fn menu_signature(cfg: &AppConfig, status: &ServerStatus, on: bool) -> String {
    let mut out = format!(
        "{on}|{}|{}|{}|{}",
        status.running,
        status.port,
        cfg.active_model.as_deref().unwrap_or("-"),
        codex::active_slug(cfg).unwrap_or_else(|| "-".into()),
    );
    for (id, p) in &cfg.providers {
        out.push_str(&format!("|{id}:{}:{}", p.name, p.enabled));
        for m in p.models.iter().filter(|m| m.enabled) {
            out.push('/');
            out.push_str(&m.id);
        }
    }
    out
}

#[derive(Clone)]
struct ActivityItems {
    hour: MenuItem<Wry>,
    last: MenuItem<Wry>,
}

/// Show and focus the main window (used by the tray icon and its menu).
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Open the window on a specific page. The route travels as an event
/// because the router lives in the webview; a window that is still booting
/// simply starts on its default page (the tray can be clicked again).
fn navigate(app: &tauri::AppHandle, route: &str) {
    show_main_window(app);
    if let Err(e) = app.emit(EVENT_NAVIGATE, route) {
        tracing::warn!("navigate event failed: {e}");
    }
}

/// Build the whole tray menu for the current state.
///
/// Rebuilt from scratch on every change rather than patched in place: the
/// model and provider sections are lists whose *shape* follows the config,
/// and there is no "menu about to open" hook to lazily fill them from.
fn build_menu(
    app: &tauri::AppHandle,
    cfg: &AppConfig,
    status: &ServerStatus,
    on: bool,
    error: Option<&str>,
) -> tauri::Result<(Menu<Wry>, ActivityItems)> {
    let state_line = MenuItem::with_id(
        app,
        "tray-state",
        if on {
            format!("Proxy running on port {} · Codex routed", status.port)
        } else if status.running {
            format!("Proxy running on port {} · Codex not routed", status.port)
        } else {
            "Proxy stopped · Codex not routed".to_string()
        },
        false,
        None::<&str>,
    )?;
    let power = MenuItem::with_id(
        app,
        "power",
        if on {
            "Turn LoomRouter Off"
        } else {
            "Turn LoomRouter On"
        },
        true,
        None::<&str>,
    )?;

    let active = cfg.active_model.as_deref();
    // A pick whose provider or model got disabled is no longer published,
    // so Codex silently falls back to its own default. Say so instead of
    // showing a model that is not actually in effect.
    let active_line = MenuItem::with_id(
        app,
        "tray-model",
        match (active, codex::active_slug(cfg)) {
            (Some(slug), Some(_)) => format!("Model: {slug}"),
            (Some(slug), None) => format!("Model: {slug} (not published)"),
            (None, _) => "Model: Codex default".to_string(),
        },
        false,
        None::<&str>,
    )?;
    let models = build_models_submenu(app, cfg, active)?;
    let providers = build_providers_submenu(app, cfg)?;

    let hour = MenuItem::with_id(
        app,
        "tray-hour",
        "Requests (last hour): 0",
        false,
        None::<&str>,
    )?;
    let last = MenuItem::with_id(app, "tray-last", "No requests yet", false, None::<&str>)?;

    let open = MenuItem::with_id(app, "show", "Open LoomRouter", true, None::<&str>)?;
    let go_to_items = PAGES
        .iter()
        .map(|(key, _, label)| {
            MenuItem::with_id(app, format!("nav:{key}"), label, true, None::<&str>)
        })
        .collect::<tauri::Result<Vec<_>>>()?;
    let go_to = Submenu::with_id_and_items(
        app,
        "go-to",
        "Go To",
        true,
        &go_to_items
            .iter()
            .map(|i| i as &dyn IsMenuItem<Wry>)
            .collect::<Vec<_>>(),
    )?;
    let config_folder =
        MenuItem::with_id(app, "open-config", "Open Config Folder", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit LoomRouter", true, None::<&str>)?;

    // Only built after something failed, so it costs nothing in the normal
    // case — and the tray has no other way to tell the user.
    let error_line = match error {
        Some(message) => Some(MenuItem::with_id(
            app,
            "tray-error",
            message,
            false,
            None::<&str>,
        )?),
        None => None,
    };

    let mut entries: Vec<&dyn IsMenuItem<Wry>> = vec![&state_line, &power];
    if let Some(item) = &error_line {
        entries.push(item);
    }
    let separators = [
        PredefinedMenuItem::separator(app)?,
        PredefinedMenuItem::separator(app)?,
        PredefinedMenuItem::separator(app)?,
        PredefinedMenuItem::separator(app)?,
    ];
    entries.extend([
        &separators[0] as &dyn IsMenuItem<Wry>,
        &active_line,
        &models,
        &providers,
        &separators[1],
        &hour,
        &last,
        &separators[2],
        &open,
        &go_to,
        &config_folder,
        &separators[3],
        &quit,
    ]);
    let menu = Menu::with_items(app, &entries)?;
    Ok((menu, ActivityItems { hour, last }))
}

/// Models the user can switch to: only what is actually published, since
/// picking anything else would point Codex at a model it cannot route.
///
/// Flat while the list stays readable; grouped per provider once it does
/// not (an aggregator alone can contribute dozens).
fn build_models_submenu(
    app: &tauri::AppHandle,
    cfg: &AppConfig,
    active: Option<&str>,
) -> tauri::Result<Submenu<Wry>> {
    // (provider label, provider id, models), providers in display order.
    let groups: Vec<(&str, &str, Vec<&crate::config::ProviderModel>)> = cfg
        .providers
        .values()
        .filter(|p| p.enabled)
        .map(|p| {
            (
                p.name.as_str(),
                p.id.as_str(),
                p.models.iter().filter(|m| m.enabled).collect::<Vec<_>>(),
            )
        })
        .filter(|(_, _, models)| !models.is_empty())
        .collect();
    let total: usize = groups.iter().map(|(_, _, m)| m.len()).sum();

    let mut owned: Vec<Box<dyn IsMenuItem<Wry>>> = Vec::new();
    // "Codex default" is not a no-op: it clears our root `model` key and
    // hands the choice back to Codex.
    owned.push(Box::new(CheckMenuItem::with_id(
        app,
        "model-default",
        "Codex Default",
        true,
        active.is_none(),
        None::<&str>,
    )?));

    if total == 0 {
        owned.push(Box::new(PredefinedMenuItem::separator(app)?));
        owned.push(Box::new(MenuItem::with_id(
            app,
            "tray-no-models",
            "No models enabled",
            false,
            None::<&str>,
        )?));
        owned.push(Box::new(MenuItem::with_id(
            app,
            "nav:providers",
            "Manage Models…",
            true,
            None::<&str>,
        )?));
    } else if total > 12 {
        // One submenu per provider: a status-bar menu that runs off the
        // screen edge is worse than one extra hop.
        for (name, id, models) in &groups {
            let items = models
                .iter()
                .map(|m| model_item(app, id, m, active))
                .collect::<tauri::Result<Vec<_>>>()?;
            owned.push(Box::new(Submenu::with_id_and_items(
                app,
                format!("provider-models:{id}"),
                *name,
                true,
                &items
                    .iter()
                    .map(|i| i as &dyn IsMenuItem<Wry>)
                    .collect::<Vec<_>>(),
            )?));
        }
    } else {
        for (name, id, models) in &groups {
            owned.push(Box::new(PredefinedMenuItem::separator(app)?));
            owned.push(Box::new(MenuItem::with_id(
                app,
                format!("tray-group:{id}"),
                *name,
                false,
                None::<&str>,
            )?));
            for m in models {
                owned.push(Box::new(model_item(app, id, m, active)?));
            }
        }
    }

    if total > 0 {
        owned.push(Box::new(PredefinedMenuItem::separator(app)?));
        // Codex only reads the model at startup, so without this the switch
        // looks like it silently failed.
        owned.push(Box::new(MenuItem::with_id(
            app,
            "tray-restart-hint",
            "Restart Codex to apply",
            false,
            None::<&str>,
        )?));
    }

    Submenu::with_id_and_items(
        app,
        "models",
        "Models",
        true,
        &owned
            .iter()
            .map(|i| i.as_ref() as &dyn IsMenuItem<Wry>)
            .collect::<Vec<_>>(),
    )
}

/// One selectable model. Checked items behave as a radio group by
/// convention: the menu is rebuilt after every pick, so exactly one is
/// checked at any time.
fn model_item(
    app: &tauri::AppHandle,
    provider_id: &str,
    model: &crate::config::ProviderModel,
    active: Option<&str>,
) -> tauri::Result<CheckMenuItem<Wry>> {
    let slug = format!("{provider_id}/{}", model.id);
    let label = model.label.clone().unwrap_or_else(|| model.id.clone());
    let checked = active == Some(slug.as_str());
    CheckMenuItem::with_id(
        app,
        format!("model:{slug}"),
        label,
        true,
        checked,
        None::<&str>,
    )
}

/// Providers as checkboxes over `provider.enabled` — disabling one pulls
/// all of its models out of the catalog and stops the proxy routing to it.
fn build_providers_submenu(app: &tauri::AppHandle, cfg: &AppConfig) -> tauri::Result<Submenu<Wry>> {
    let mut owned: Vec<Box<dyn IsMenuItem<Wry>>> = Vec::new();
    for p in cfg.providers.values() {
        owned.push(Box::new(CheckMenuItem::with_id(
            app,
            format!("provider:{}", p.id),
            &p.name,
            true,
            p.enabled,
            None::<&str>,
        )?));
    }
    if owned.is_empty() {
        owned.push(Box::new(MenuItem::with_id(
            app,
            "tray-no-providers",
            "No providers yet",
            false,
            None::<&str>,
        )?));
    }
    owned.push(Box::new(PredefinedMenuItem::separator(app)?));
    owned.push(Box::new(MenuItem::with_id(
        app,
        "nav:providers",
        "Manage Providers…",
        true,
        None::<&str>,
    )?));

    Submenu::with_id_and_items(
        app,
        "providers",
        "Providers",
        true,
        &owned
            .iter()
            .map(|i| i.as_ref() as &dyn IsMenuItem<Wry>)
            .collect::<Vec<_>>(),
    )
}

/// Rebuild the menu from the live state and swap it onto the tray.
///
/// The state snapshot is taken off the main thread (it needs async locks),
/// and only the menu construction hops onto it — menus are main-thread
/// objects on macOS.
fn rebuild_tray_menu(app: &tauri::AppHandle) {
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let (cfg, status, on) = {
            let state = handle.state::<AppState>();
            let cfg = state.config.read().await.clone();
            let status = state.server_status().await;
            let on = state.power_state().await;
            (cfg, status, on)
        };
        // Everything the menu's *shape and labels* depend on. Swapping a
        // menu that would come out identical is pure cost — and worse, it
        // yanks the menu out from under a user who has it open, which the
        // 15s heartbeat would otherwise do on a timer.
        let error = handle
            .try_state::<TrayHandles>()
            .and_then(|h| h.last_error.lock().ok().and_then(|slot| slot.clone()));
        let signature = format!(
            "{}|{}",
            menu_signature(&cfg, &status, on),
            error.as_deref().unwrap_or("-")
        );
        if let Some(handles) = handle.try_state::<TrayHandles>() {
            let unchanged = handles
                .signature
                .lock()
                .map(|current| current.as_deref() == Some(signature.as_str()))
                .unwrap_or(false);
            if unchanged {
                refresh_tray_activity(&handle).await;
                return;
            }
        }
        let main = handle.clone();
        let hop = handle.run_on_main_thread(move || {
            let Some(handles) = main.try_state::<TrayHandles>() else {
                return;
            };
            match build_menu(&main, &cfg, &status, on, error.as_deref()) {
                Ok((menu, items)) => {
                    if let Err(e) = handles.tray.set_menu(Some(menu)) {
                        tracing::warn!("tray menu swap failed: {e}");
                        return;
                    }
                    // Same critical section as the swap: the 15s refresh
                    // must never write into items from the old menu.
                    if let Ok(mut slot) = handles.items.lock() {
                        *slot = items;
                    }
                    if let Ok(mut slot) = handles.signature.lock() {
                        *slot = Some(signature);
                    }
                    // Only now do the activity lines exist: refreshing
                    // before the swap would fill in the menu being replaced
                    // and leave the new one showing its placeholder text.
                    let after = main.clone();
                    tauri::async_runtime::spawn(async move {
                        refresh_tray_activity(&after).await;
                    });
                }
                Err(e) => tracing::warn!("tray menu rebuild failed: {e}"),
            }
        });
        if let Err(e) = hop {
            tracing::warn!("tray menu rebuild could not reach the main thread: {e}");
            refresh_tray_activity(&handle).await;
        }
    });
}

/// Tell the windows that the backend changed something they display. The
/// tray is a second author of the same state, so a page that only fetched
/// on mount would otherwise show a stale server/model/provider.
fn notify_state_changed(app: &tauri::AppHandle) {
    if let Err(e) = app.emit(EVENT_STATE_CHANGED, ()) {
        tracing::warn!("state-changed event failed: {e}");
    }
    rebuild_tray_menu(app);
}

/// Handle one tray menu click. Everything that touches app state is async,
/// and this runs on the main thread, so the work is spawned.
fn on_tray_menu_event(app: &tauri::AppHandle, id: &str) {
    match id {
        "show" => show_main_window(app),
        // AppHandle::exit() does not fire CloseRequested, so this is a
        // real quit (the window is not just hidden again).
        "quit" => app.exit(0),
        "open-config" => {
            use tauri_plugin_opener::OpenerExt;
            let path = crate::config::config_dir();
            if let Err(e) = app
                .opener()
                .open_path(path.display().to_string(), None::<&str>)
            {
                tracing::warn!("opening the config folder failed: {e}");
            }
        }
        "power" => {
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                let result = app.state::<AppState>().power_toggle().await;
                report_tray_result(&app, "Could not switch LoomRouter", result.map(|_| ()));
            });
        }
        "model-default" => set_active_model_from_tray(app, None),
        id if id.starts_with("model:") => {
            set_active_model_from_tray(app, id.strip_prefix("model:").map(str::to_string))
        }
        id if id.starts_with("provider:") => {
            let Some(provider) = id.strip_prefix("provider:").map(str::to_string) else {
                return;
            };
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                let state = app.state::<AppState>();
                let enabled = state
                    .config
                    .read()
                    .await
                    .providers
                    .get(&provider)
                    .map(|p| p.enabled);
                // The click already flipped the checkmark natively; the
                // rebuild below is what makes it agree with the config.
                let result = match enabled {
                    Some(enabled) => state.set_provider_enabled(&provider, !enabled).await,
                    None => Ok(()),
                };
                report_tray_result(&app, "Could not switch that provider", result);
            });
        }
        id if id.starts_with("nav:") => {
            let key = id.trim_start_matches("nav:");
            if let Some((_, route, _)) = PAGES.iter().find(|(k, _, _)| *k == key) {
                navigate(app, route);
            }
        }
        _ => {}
    }
}

fn set_active_model_from_tray(app: &tauri::AppHandle, slug: Option<String>) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let result = app.state::<AppState>().set_active_model(slug).await;
        report_tray_result(&app, "Could not switch model", result);
    });
}

/// Record the outcome of a tray action and redraw the menu. A failure stays
/// on screen as a disabled line rather than only reaching the log.
fn report_tray_result(app: &tauri::AppHandle, context: &str, result: anyhow::Result<()>) {
    let message = match result {
        Ok(()) => None,
        Err(e) => {
            tracing::warn!("{context}: {e}");
            Some(format!("{context}: {e}"))
        }
    };
    if let Some(handles) = app.try_state::<TrayHandles>() {
        if let Ok(mut slot) = handles.last_error.lock() {
            *slot = message;
        }
    }
    notify_state_changed(app);
}

/// Build the system-tray icon: left click restores the window, right click
/// opens the menu built by `build_menu`.
fn setup_tray(app: &tauri::App) -> tauri::Result<()> {
    let handle = app.handle().clone();
    // A placeholder menu so the tray is never menu-less between launch and
    // the first rebuild (which needs async state).
    let (menu, items) = build_menu(
        &handle,
        &AppConfig::default(),
        &ServerStatus {
            running: false,
            port: 0,
            url: None,
        },
        false,
        None,
    )?;

    let mut builder = TrayIconBuilder::with_id("main")
        .tooltip("LoomRouter")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| on_tray_menu_event(app, event.id.as_ref()))
        .on_tray_icon_event(move |tray, event| {
            // Best-effort freshness on interaction: a right click opens
            // the menu immediately (the OS owns that path, so there is no
            // "menu about to open" hook), but this rebuild usually lands
            // before the next open; the periodic task below bounds the
            // staleness to ~15s either way.
            if matches!(
                event,
                TrayIconEvent::Click {
                    button_state: MouseButtonState::Up,
                    ..
                }
            ) {
                rebuild_tray_menu(tray.app_handle());
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
    app.manage(TrayHandles {
        tray,
        items: std::sync::Mutex::new(items),
        last_error: std::sync::Mutex::new(None),
        signature: std::sync::Mutex::new(None),
    });
    rebuild_tray_menu(&handle);

    // Periodic activity refresh: keeps the menu/tooltip fresh while the
    // window stays hidden. refresh_tray_activity() never panics (every
    // failure is logged), so this task only ends with the runtime itself.
    tauri::async_runtime::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
        loop {
            // The first interval tick fires immediately, so the tray is
            // populated once at startup and then every 15s. A rebuild is
            // cheap when nothing changed (the signature short-circuits it
            // to a text refresh), and it is what keeps the power line
            // honest when the proxy is started or stopped from elsewhere.
            tick.tick().await;
            rebuild_tray_menu(&handle);
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
async fn refresh_tray_activity(app: &tauri::AppHandle) {
    let Some(handles) = app.try_state::<TrayHandles>() else {
        return;
    };
    // Clone the handles out of the lock: the std Mutex must not be held
    // across the awaits below, and the items are Arc-backed anyway.
    let Some(items) = handles.items.lock().ok().map(|i| i.clone()) else {
        return;
    };
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

    if let Err(e) = items.hour.set_text(&hour_text) {
        tracing::warn!("tray menu update failed: {e}");
    }
    if let Err(e) = items.last.set_text(&last_text) {
        tracing::warn!("tray menu update failed: {e}");
    }
    if let Err(e) = handles.tray.set_tooltip(Some(&tooltip)) {
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
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(AppState::load())
        .setup(|app| {
            setup_tray(app)?;
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
                if let Err(e) = state.server_start().await {
                    tracing::warn!("proxy autostart failed: {e}");
                }
                // The tray was built before this ran, so without a rebuild
                // it would keep claiming the proxy is stopped.
                crate::rebuild_tray_menu(&handle);
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
            commands::agent_templates,
            commands::multi_agent_status,
            commands::set_multi_agent,
            commands::set_side_call_fallback,
            commands::set_native_slug_mode,
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
        app: tauri::AppHandle,
        state: State<'_, AppState>,
        provider: Provider,
    ) -> Result<(), String> {
        let result = state
            .save_provider(provider)
            .await
            .map_err(|e| e.to_string());
        crate::rebuild_tray_menu(&app);
        result
    }

    #[tauri::command]
    pub async fn delete_provider(
        app: tauri::AppHandle,
        state: State<'_, AppState>,
        id: String,
    ) -> Result<(), String> {
        let result = state.delete_provider(&id).await.map_err(|e| e.to_string());
        crate::rebuild_tray_menu(&app);
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
        crate::rebuild_tray_menu(&app);
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
        crate::rebuild_tray_menu(&app);
        status
    }

    #[tauri::command]
    pub async fn server_stop(
        app: tauri::AppHandle,
        state: State<'_, AppState>,
    ) -> Result<ServerStatus, String> {
        let status = state.server_stop().await.map_err(|e| e.to_string());
        crate::rebuild_tray_menu(&app);
        status
    }

    #[tauri::command]
    pub async fn codex_status(
        state: State<'_, AppState>,
    ) -> Result<crate::codex::CodexStatus, String> {
        Ok(state.codex_status().await)
    }

    #[tauri::command]
    pub async fn codex_apply(
        app: tauri::AppHandle,
        state: State<'_, AppState>,
    ) -> Result<(), String> {
        let result = state.codex_apply().await.map_err(|e| e.to_string());
        crate::rebuild_tray_menu(&app);
        result
    }

    #[tauri::command]
    pub async fn codex_remove(
        app: tauri::AppHandle,
        state: State<'_, AppState>,
    ) -> Result<(), String> {
        let result = state.codex_remove().await.map_err(|e| e.to_string());
        crate::rebuild_tray_menu(&app);
        result
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
        crate::rebuild_tray_menu(&app);
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
        crate::rebuild_tray_menu(&app);
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
}
