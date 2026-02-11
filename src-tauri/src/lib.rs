mod commands;
mod config;
mod security;
mod state;
mod tools;
mod tray;
mod ws;

use state::AppState;
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Default to showing info-level logs in dev mode
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "beakr_desktop=info");
    }
    env_logger::init();

    let app_state = AppState::new();

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_store::Builder::default().build())
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, None))
        .manage(app_state.clone())
        .setup(move |app| {
            // macOS: hide from Dock, show only in menu bar
            #[cfg(target_os = "macos")]
            {
                app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            }

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

            // Set up system tray
            tray::setup_tray(app.handle())?;

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

                    // Brief delay to let state initialization complete
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

                    let ws_url = ws_url();
                    let client = ws::WsClient::new(app_handle, state_clone, ws_url);
                    client.run().await;
                });
            } else if cfg!(debug_assertions) {
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
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
