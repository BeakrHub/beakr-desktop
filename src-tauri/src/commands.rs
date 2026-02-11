use tauri::{AppHandle, State};
use tauri_plugin_store::StoreExt;

use crate::config;
use crate::state::{AppState, ConnectionStatus};
use crate::ws::WsClient;

const STORE_FILE: &str = "settings.json";

/// Store a fresh auth token from the frontend.
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

    let ws_url = crate::ws_url();

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

/// Claim a pairing code from the backend and store the returned device token.
#[tauri::command]
pub async fn claim_pairing_code(
    app: AppHandle,
    state: State<'_, AppState>,
    code: String,
) -> Result<(), String> {
    let device_name = state.device_name.read().await.clone();
    let scoped_folders = state.scoped_folders.read().await.clone();

    let platform = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    };

    // Build the API URL from WS URL (same host)
    let ws_url = crate::ws_url();
    let api_base = ws_url
        .replace("ws://", "http://")
        .replace("wss://", "https://")
        .replace("/v1/desktop-agent/ws", "");

    let url = format!("{}/v1/desktop-agent/pair/claim", api_base);

    let platform_version = crate::ws::os_version();

    let body = serde_json::json!({
        "code": code,
        "device_name": device_name,
        "platform": platform,
        "scoped_folders": scoped_folders,
        "platform_version": platform_version,
        "app_version": env!("CARGO_PKG_VERSION"),
    });

    // Make HTTP request using reqwest-lite via tokio_tungstenite's underlying HTTP
    // We'll use a simple TCP + HTTP approach since we don't have reqwest
    let client = http_client();
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .body(serde_json::to_string(&body).unwrap())
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?;

    let status = resp.status();
    let text = resp.text().await.map_err(|e| format!("Failed to read response: {e}"))?;

    if !status.is_success() {
        // Try to extract detail from JSON response
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(detail) = json.get("detail").and_then(|d| d.as_str()) {
                return Err(detail.to_string());
            }
        }
        return Err(format!("Pairing failed (HTTP {})", status));
    }

    let result: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("Invalid response: {e}"))?;

    let device_token = result
        .get("device_token")
        .and_then(|v| v.as_str())
        .ok_or("Missing device_token in response")?;

    // Store token persistently
    store_token(&app, device_token)?;

    // Set token in runtime state for immediate WS connection
    *state.auth_token.write().await = Some(device_token.to_string());

    Ok(())
}

/// Get stored device token from persistent store.
#[tauri::command]
pub fn get_stored_token(app: AppHandle) -> Result<Option<String>, String> {
    let store = app
        .store(STORE_FILE)
        .map_err(|e| format!("Failed to open store: {e}"))?;

    Ok(store
        .get("device_token")
        .and_then(|v| serde_json::from_value(v).ok()))
}

/// Clear stored device token (unlink device).
#[tauri::command]
pub async fn clear_token(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    // Clear runtime state
    *state.auth_token.write().await = None;

    // Clear persistent store
    let store = app
        .store(STORE_FILE)
        .map_err(|e| format!("Failed to open store: {e}"))?;
    let _ = store.delete("device_token");

    Ok(())
}

fn store_token(app: &AppHandle, token: &str) -> Result<(), String> {
    let store = app
        .store(STORE_FILE)
        .map_err(|e| format!("Failed to open store: {e}"))?;
    let _ = store.set(
        "device_token",
        serde_json::to_value(token).unwrap_or_default(),
    );
    Ok(())
}

/// Get the WebSocket URL (so the frontend can determine the environment).
#[tauri::command]
pub fn get_ws_url() -> String {
    crate::ws_url()
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(format!("BeakrDesktop/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("failed to build HTTP client")
}
