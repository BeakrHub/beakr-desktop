use std::sync::atomic::Ordering;

use tauri::{AppHandle, State};
use tauri_plugin_store::StoreExt;

use crate::config;
use crate::state::{AppState, ConnectionStatus};
use crate::ws::WsClient;

const STORE_FILE: &str = "settings.json";

/// Store a fresh auth token from the frontend.
#[tauri::command]
pub async fn set_auth_token(
    app: AppHandle,
    state: State<'_, AppState>,
    token: String,
) -> Result<(), String> {
    let state_clone = (*state).clone();
    *state.auth_token.write().await = Some(token);
    tauri::async_runtime::spawn(async move {
        crate::session::benchling::register_current_session_with_backend(&app, &state_clone).await;
    });
    Ok(())
}

/// Start the WebSocket connection to the backend.
#[tauri::command]
pub async fn connect_ws(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let current = state.ws_status.read().await.clone();
    if matches!(
        current,
        ConnectionStatus::Connected | ConnectionStatus::Connecting | ConnectionStatus::Reconnecting
    ) {
        return Ok(());
    }

    let ws_url = crate::ws_url();

    // Clear any prior disconnect request so the run loop will stay connected.
    state.shutdown_requested.store(false, Ordering::SeqCst);

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
    // Record the intent durably before notifying: the message loop may consume the
    // notify permit and tear down the socket before the run loop re-checks, so the
    // flag — not the permit — is what keeps it from auto-reconnecting.
    state.shutdown_requested.store(true, Ordering::SeqCst);
    state.ws_shutdown.notify_one();
    Ok(())
}

/// Get current connection status.
#[tauri::command]
pub async fn get_connection_status(
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
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

    // Wake both consumers of the change: the WS client (pushes the new list
    // to the backend) and the fs watcher (rebinds to the new roots).
    state.notify_folders_changed();

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
        autostart
            .enable()
            .map_err(|e| format!("Failed to enable autostart: {e}"))
    } else {
        autostart
            .disable()
            .map_err(|e| format!("Failed to disable autostart: {e}"))
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
    let text = resp
        .text()
        .await
        .map_err(|e| format!("Failed to read response: {e}"))?;

    if !status.is_success() {
        // Try to extract detail from JSON response
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(detail) = json.get("detail").and_then(|d| d.as_str()) {
                return Err(detail.to_string());
            }
        }
        return Err(format!("Pairing failed (HTTP {})", status));
    }

    let result: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("Invalid response: {e}"))?;

    let device_token = result
        .get("device_token")
        .and_then(|v| v.as_str())
        .ok_or("Missing device_token in response")?;

    // Store token persistently
    store_token(&app, device_token)?;

    // Set token in runtime state for immediate WS connection
    *state.auth_token.write().await = Some(device_token.to_string());

    // Device is now paired — reflect that in the tray menu label.
    crate::tray::update_tray_pairing(&app, true);

    crate::session::benchling::register_current_session_with_backend(&app, &state).await;

    // NOTE: we deliberately do NOT force a reconnect here, and that is correct
    // for production. Pairing is reached from the PairingScreen, which only shows
    // when there is no token and (in a release build) no live connection — so the
    // frontend's subsequent connect_ws() is a clean first connect using the token
    // just stored above.
    //
    // The one place this is imperfect is the DEV build: it auto-connects as a
    // throwaway `dev_local` device on startup (see lib.rs, gated on
    // debug_assertions), so a live "ghost" connection already exists when you
    // pair. connect_ws() no-ops while already connected, so the new token does not
    // take over until that ghost connection drops (heartbeat timeout, ~90s) or the
    // user toggles Disconnect -> Connect. This is a dev-only ergonomic quirk, not a
    // production bug, so it is intentionally left as-is.
    //
    // Do NOT "fix" this by calling disconnect_ws() then connect_ws() here: that can
    // leave the old run() loop racing the `shutdown_requested` flag and spawn a
    // second concurrent reconnect loop — the exact reconnect-race class ENG-758
    // stabilized. If a real "switch account while connected" flow is ever needed,
    // do it as a single-loop reconnect SIGNAL (a force_reconnect Notify handled
    // inside the existing message loop), never as disconnect-then-reconnect.

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

/// Unlink the device: drop the runtime token, delete it from the persistent
/// store, and flip the tray label back to "Pair device". Shared by the
/// `clear_token` command (user-initiated "Unlink Device") and the WS client's
/// revocation path, which must unlink authoritatively in Rust because the app
/// can run with no webview alive at startup (the window opens on demand), so
/// no frontend exists to handle the `token_invalid` event.
pub(crate) async fn clear_device_token(app: &AppHandle, state: &AppState) -> Result<(), String> {
    // Clear runtime state
    *state.auth_token.write().await = None;

    // Clear persistent store
    let store = app
        .store(STORE_FILE)
        .map_err(|e| format!("Failed to open store: {e}"))?;
    let _ = store.delete("device_token");

    // Device is no longer paired — reflect that in the tray menu label.
    crate::tray::update_tray_pairing(app, false);

    Ok(())
}

/// Clear stored device token (unlink device).
#[tauri::command]
pub async fn clear_token(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    clear_device_token(&app, &state).await
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
