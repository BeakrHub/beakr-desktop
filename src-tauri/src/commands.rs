use tauri::{AppHandle, State};

use crate::config;
use crate::state::{AppState, ConnectionStatus};
use crate::ws::WsClient;

/// Store a fresh JWT from the frontend's Clerk session.
#[tauri::command]
pub async fn set_auth_token(state: State<'_, AppState>, token: String) -> Result<(), String> {
    *state.auth_token.write().await = Some(token);
    Ok(())
}

/// Start the WebSocket connection to the backend.
#[tauri::command]
pub async fn connect_ws(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let current = state.ws_status.read().await.clone();
    if matches!(current, ConnectionStatus::Connected | ConnectionStatus::Connecting | ConnectionStatus::Reconnecting) {
        return Ok(());
    }

    let ws_url = if cfg!(debug_assertions) {
        "ws://localhost:8000/v1/desktop-agent/ws".to_string()
    } else {
        "wss://api.thebeakr.com/v1/desktop-agent/ws".to_string()
    };

    let state_clone = (*state).clone();
    let app_clone = app.clone();

    tauri::async_runtime::spawn(async move {
        let client = WsClient::new(app_clone, state_clone, ws_url);
        client.run().await;
    });

    Ok(())
}

/// Disconnect the WebSocket.
#[tauri::command]
pub async fn disconnect_ws(state: State<'_, AppState>) -> Result<(), String> {
    state.ws_shutdown.notify_one();
    Ok(())
}

/// Get current connection status.
#[tauri::command]
pub async fn get_connection_status(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let status = state.ws_status.read().await.clone();
    let device_id = state.device_id.read().await.clone();

    Ok(serde_json::json!({
        "status": status,
        "device_id": device_id,
    }))
}

/// Get scoped folders.
#[tauri::command]
pub async fn get_scoped_folders(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    Ok(state.scoped_folders.read().await.clone())
}

/// Set scoped folders and persist to store.
/// Also notifies the WS client to push the update to the backend.
#[tauri::command]
pub async fn set_scoped_folders(
    app: AppHandle,
    state: State<'_, AppState>,
    folders: Vec<String>,
) -> Result<(), String> {
    *state.scoped_folders.write().await = folders.clone();

    let settings = config::Settings {
        scoped_folders: folders.clone(),
        device_name: Some(state.device_name.read().await.clone()),
        auto_connect: true,
    };
    config::save_settings(&app, &settings);

    // Notify the WS client to push updated folders to backend
    state.folders_changed.notify_one();

    Ok(())
}

/// Check if launch-at-login is enabled.
#[tauri::command]
pub fn get_autostart(app: AppHandle) -> Result<bool, String> {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch()
        .is_enabled()
        .map_err(|e| format!("Failed to check autostart: {e}"))
}

/// Enable or disable launch-at-login.
#[tauri::command]
pub fn set_autostart(app: AppHandle, enabled: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let autostart = app.autolaunch();
    if enabled {
        autostart.enable().map_err(|e| format!("Failed to enable autostart: {e}"))
    } else {
        autostart.disable().map_err(|e| format!("Failed to disable autostart: {e}"))
    }
}

/// Get device name.
#[tauri::command]
pub async fn get_device_name(state: State<'_, AppState>) -> Result<String, String> {
    Ok(state.device_name.read().await.clone())
}

/// Set device name and persist to store.
#[tauri::command]
pub async fn set_device_name(
    app: AppHandle,
    state: State<'_, AppState>,
    name: String,
) -> Result<(), String> {
    *state.device_name.write().await = name.clone();

    let settings = config::Settings {
        scoped_folders: state.scoped_folders.read().await.clone(),
        device_name: Some(name),
        auto_connect: true,
    };
    config::save_settings(&app, &settings);

    Ok(())
}
