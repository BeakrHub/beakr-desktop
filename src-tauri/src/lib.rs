mod commands;
mod config;
mod diagnostics;
mod file_index;
mod file_watch;
mod process_group;
mod search_filter;
mod security;
mod session;
mod state;
mod tools;
mod tray;
pub mod unicode;
mod ws;

use state::AppState;
#[cfg(target_os = "windows")]
use tauri::Manager;
use tauri_plugin_autostart::MacosLauncher;

/// Returns the WebSocket URL based on build configuration.
/// Priority: BEAKR_WS_URL env var (compile-time) > debug=localhost > release=production
pub fn ws_url() -> String {
    if let Some(url) = option_env!("BEAKR_WS_URL") {
        return url.to_string();
    }
    if cfg!(debug_assertions) {
        "ws://localhost:8000/v1/desktop-agent/ws".to_string()
    } else {
        "wss://api.thebeakr.com/v1/desktop-agent/ws".to_string()
    }
}

#[cfg(not(target_os = "macos"))]
fn should_prevent_exit(exit_code: Option<i32>) -> bool {
    // A window-count/user exit has no code. Explicit app.exit()/restart() calls
    // carry a code and must remain able to terminate the tray application.
    exit_code.is_none()
}

fn spawn_benchling_liveness(app_handle: tauri::AppHandle, state: AppState) {
    tauri::async_runtime::spawn(session::benchling::watch_session_liveness(
        app_handle, state,
    ));
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app_state = AppState::new();

    tauri::Builder::default()
        // Must be registered first so a duplicate process exits before other
        // plugins initialize. Reuse the tray/Dock window recovery path so the
        // surviving instance is shown, unminimized, and focused.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            tray::show_settings_window(app);

            // On Windows the callback runs synchronously while the secondary
            // process is still inside its WM_COPYDATA handoff. The first focus
            // request can be rejected until that foreground-capable process
            // exits, so retry once after the handoff has returned.
            #[cfg(target_os = "windows")]
            {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    if let Some(window) = app.get_webview_window("settings") {
                        let _ = window.set_focus();
                    }
                });
            }
        }))
        // The default targets write both to stdout and to the platform log
        // directory (on Windows: %LOCALAPPDATA%\com.thebeakr.desktop\logs).
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                .max_file_size(1_000_000)
                .rotation_strategy(tauri_plugin_log::RotationStrategy::KeepSome(3))
                .build(),
        )
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_store::Builder::default().build())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .manage(app_state.clone())
        .setup(move |app| {
            diagnostics::install_panic_hook();
            diagnostics::spawn_log_flusher();
            diagnostics::trigger_test_panic_if_configured();
            log::info!(
                "Beakr Desktop startup diagnostics initialized (process_id={})",
                std::process::id()
            );
            log::logger().flush();

            // ENG-1377: keep the default Regular activation policy so the app has
            // a Dock icon users can click to open it. Do not set
            // ActivationPolicy::Accessory — tray-only proved too hidden (notched
            // menu bars can swallow the tray icon). Dock click → RunEvent::Reopen
            // (handled in run()) → settings window.

            // Load persisted settings
            let settings = config::load_settings(app.handle());
            let has_stored_token = {
                use tauri_plugin_store::StoreExt;
                app.handle()
                    .store("settings.json")
                    .ok()
                    .and_then(|store| store.get("device_token"))
                    .and_then(|v| v.as_str().map(|s| !s.is_empty()))
                    .unwrap_or(false)
            };

            {
                let state = app_state.clone();
                let settings_folders = settings.scoped_folders.clone();
                let settings_name = settings.device_name.clone();
                tauri::async_runtime::spawn(async move {
                    *state.scoped_folders.write().await = settings_folders;
                    if let Some(name) = settings_name {
                        *state.device_name.write().await = name;
                    }
                    // Start file-index maintenance now that the scoped folders
                    // are loaded (watcher + periodic rescan fallback).
                    file_watch::spawn(state.clone());
                });
            }

            // Enable launch-at-login by default so the agent stays available
            {
                use tauri_plugin_autostart::ManagerExt;
                let autostart = app.autolaunch();
                if !autostart.is_enabled().unwrap_or(false) {
                    let _ = autostart.enable();
                    log::info!("Autostart enabled");
                }
            }

            // Set up system tray, then set the pairing-aware menu label from the
            // stored token (claim_pairing_code / clear_token keep it in sync after).
            tray::setup_tray(app.handle())?;
            tray::update_tray_pairing(app.handle(), has_stored_token);

            // First run (no paired device): open the window on launch so a
            // Finder/Spotlight launch lands on the pairing screen instead of a
            // Dock icon with no window — macOS fires Reopen only on
            // RE-activation, never on first launch. Paired launches stay
            // silent so autostart at login doesn't pop a window.
            if !has_stored_token {
                tray::show_settings_window(app.handle());
            }

            // In dev builds, always auto-open the window on launch so testing
            // doesn't depend on clicking the Dock or tray icon.
            #[cfg(debug_assertions)]
            tray::show_settings_window(app.handle());

            // Auto-connect if we have a stored device token
            // (In dev mode without a token, the frontend will handle connection)
            if has_stored_token {
                log::info!("Found stored device token, auto-connecting on startup");
                let app_handle = app.handle().clone();
                let state_clone = app_state.clone();

                tauri::async_runtime::spawn(async move {
                    // Load token from store and set in state
                    use tauri_plugin_store::StoreExt;
                    if let Ok(store) = app_handle.store("settings.json") {
                        if let Some(token) = store
                            .get("device_token")
                            .and_then(|v| serde_json::from_value::<String>(v).ok())
                        {
                            *state_clone.auth_token.write().await = Some(token);
                        }
                    }

                    {
                        let ws_app = app_handle.clone();
                        let ws_state = state_clone.clone();
                        tauri::async_runtime::spawn(async move {
                            // Brief delay to let state initialization complete
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

                            let ws_url = ws_url();
                            let client = ws::WsClient::new(ws_app, ws_state, ws_url);
                            client.run().await;
                        });
                    }

                    let restored = session::benchling::restore_session_on_startup(
                        app_handle.clone(),
                        state_clone.clone(),
                    )
                    .await;
                    if restored {
                        log::info!("Benchling startup session restore succeeded");
                    }

                    spawn_benchling_liveness(app_handle, state_clone);
                });
            } else {
                // With no stored token there is nothing to restore yet, but keep
                // the liveness watcher alive so a later pairing/login flow is
                // monitored without needing an app restart.
                spawn_benchling_liveness(app.handle().clone(), app_state.clone());

                if cfg!(debug_assertions) {
                    log::info!("Dev mode: auto-connecting WebSocket client");
                    let app_handle = app.handle().clone();
                    let state_clone = app_state.clone();
                    tauri::async_runtime::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        let ws_url = ws_url();
                        let client = ws::WsClient::new(app_handle, state_clone, ws_url);
                        client.run().await;
                    });
                }
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::set_auth_token,
            commands::connect_ws,
            commands::disconnect_ws,
            commands::get_connection_status,
            commands::get_scoped_folders,
            commands::set_scoped_folders,
            commands::get_device_name,
            commands::set_device_name,
            commands::get_autostart,
            commands::set_autostart,
            commands::claim_pairing_code,
            commands::get_stored_token,
            commands::clear_token,
            commands::get_ws_url,
            commands::get_coding_agent_settings,
            commands::set_coding_agent_settings,
            diagnostics::open_log_folder,
            commands::get_active_coding_run,
            commands::stop_coding_run,
            commands::open_run_terminal,
            commands::get_coding_agent_readiness,
            session::commands::connect_session,
            session::commands::benchling_status,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app_handle, _event| {
            // Windows/Linux tray applications must stay resident after their last
            // window closes. Keep explicit app.exit()/restart() working, and do
            // not change macOS's normal Cmd-Q / application-menu quit behaviour.
            #[cfg(not(target_os = "macos"))]
            if let tauri::RunEvent::ExitRequested { code, api, .. } = _event {
                if should_prevent_exit(code) {
                    api.prevent_exit();
                }
            }

            // macOS fires Reopen when the Dock icon is clicked (and on
            // Finder/Spotlight re-launch of a running app). Only open the
            // settings window when nothing is visible — a Dock click while
            // e.g. the Benchling session window is up must not cover it.
            // A minimized-only app reports has_visible_windows == false.
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen {
                has_visible_windows,
                ..
            } = _event
            {
                if !has_visible_windows {
                    tray::show_settings_window(_app_handle);
                }
            }
        });
}

#[cfg(all(test, not(target_os = "macos")))]
mod lifecycle_tests {
    use super::should_prevent_exit;

    #[test]
    fn user_or_last_window_exit_is_prevented_for_tray_lifetime() {
        assert!(should_prevent_exit(None));
    }

    #[test]
    fn explicit_exit_and_restart_remain_allowed() {
        assert!(!should_prevent_exit(Some(0)));
        assert!(!should_prevent_exit(Some(1)));
    }
}
