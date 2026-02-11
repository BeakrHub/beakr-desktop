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

            // Auto-connect in dev mode or when auto_connect is enabled
            if cfg!(debug_assertions) || settings.auto_connect {
                log::info!("Auto-connecting WebSocket client on startup");
                let app_handle = app.handle().clone();
                let state_clone = app_state.clone();
                tauri::async_runtime::spawn(async move {
                    // Brief delay to let state initialization complete
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
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
