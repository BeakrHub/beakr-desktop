//! Tauri commands and import driver for the Benchling connector.
//!
//! Flow:
//!   1. `connect_benchling` opens the benchling.com webview window (label
//!      "benchling") and starts the localhost data bridge (see `bridge.rs`). The
//!      bridge port is stashed in [`AppState`] for the import step.
//!   2. The user logs into Benchling with their own session inside that window.
//!   3. `benchling_import` injects the gather script (see `gather_script.rs`) into
//!      the open window via `webview.eval`. The script reads with the session
//!      cookie and POSTs results back to the localhost bridge.
//!   4. The import driver consumes bridge messages, emits frontend events
//!      (`benchling:progress` / `:needs_login` / `:error`), and on `Complete`
//!      POSTs the items to `{api_base}/v1/connectors/benchling/push` using the
//!      stored device token, then emits `benchling:done`.

use tauri::{AppHandle, Manager, State};

use crate::benchling::bridge::{self, BridgeMessage};
use crate::benchling::gather_script::{BENCHLING_GATHER_SCRIPT, PORT_PLACEHOLDER};
use crate::state::AppState;

const BENCHLING_WINDOW_LABEL: &str = "benchling";
const BENCHLING_URL: &str = "https://benchling.com";

/// Derive the Beakr backend API base from the WS URL (mirrors `claim_pairing_code`).
fn api_base() -> String {
    crate::ws_url()
        .replace("ws://", "http://")
        .replace("wss://", "https://")
        .replace("/v1/desktop-agent/ws", "")
}

/// Opens the Benchling webview window and starts the localhost data bridge.
///
/// Safe to call repeatedly: if the window already exists it is focused and the
/// existing bridge port is reused.
#[tauri::command]
pub async fn connect_benchling(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    // If the window already exists, just focus it (bridge is already running).
    if let Some(window) = app.get_webview_window(BENCHLING_WINDOW_LABEL) {
        let _ = window.show();
        let _ = window.set_focus();
        return Ok(());
    }

    // Start the localhost bridge listener before opening the window so it is ready
    // by the time the user clicks Import.
    let shutdown = state.benchling_bridge_shutdown.clone();
    shutdown.notify_waiters(); // tear down any prior listener
    let (port, mut rx) = bridge::start_bridge(shutdown).await?;
    *state.benchling_bridge_port.write().await = Some(port);

    // Drive the bridge message loop on a background task.
    {
        let app_drive = app.clone();
        let api = api_base();
        let token = state.auth_token.read().await.clone();
        tauri::async_runtime::spawn(async move {
            run_import_driver(app_drive, api, token, &mut rx).await;
        });
    }

    // Open the Benchling webview window.
    let url = BENCHLING_URL
        .parse()
        .map_err(|e| format!("Invalid Benchling URL: {e}"))?;

    let builder = tauri::WebviewWindowBuilder::new(
        &app,
        BENCHLING_WINDOW_LABEL,
        tauri::WebviewUrl::External(url),
    )
    .title("Connect Benchling")
    .inner_size(1100.0, 800.0)
    .resizable(true)
    .center();

    let window = builder
        .build()
        .map_err(|e| format!("Failed to open Benchling window: {e}"))?;

    // Clear the cached bridge port when the window is closed so a later connect
    // starts fresh.
    {
        let state_owned = (*state).clone();
        let shutdown_on_close = state.benchling_bridge_shutdown.clone();
        window.on_window_event(move |event| {
            if let tauri::WindowEvent::Destroyed = event {
                shutdown_on_close.notify_waiters();
                let state_inner = state_owned.clone();
                tauri::async_runtime::spawn(async move {
                    *state_inner.benchling_bridge_port.write().await = None;
                });
            }
        });
    }

    Ok(())
}

/// Injects the gather script into the open Benchling window.
///
/// Called by the frontend "Import" button after the user has logged in. The
/// gather script verifies the session, lists folders/items, and POSTs them to the
/// localhost bridge (which the driver started in `connect_benchling` consumes).
#[tauri::command]
pub async fn benchling_import(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let port = state
        .benchling_bridge_port
        .read()
        .await
        .ok_or("Benchling window is not open. Click Connect Benchling first.")?;

    let window = app
        .get_webview_window(BENCHLING_WINDOW_LABEL)
        .ok_or("Benchling window is not open. Click Connect Benchling first.")?;

    let script = BENCHLING_GATHER_SCRIPT.replace(PORT_PLACEHOLDER, &port.to_string());

    window
        .eval(&script)
        .map_err(|e| format!("Failed to start Benchling import: {e}"))?;

    bridge::emit_progress(&app, "start", "Starting Benchling import…", None, None);
    Ok(())
}

/// Consumes bridge messages until a terminal message arrives, emitting frontend
/// events and (on completion) pushing items to the Beakr backend.
async fn run_import_driver(
    app: AppHandle,
    api: String,
    token: Option<String>,
    rx: &mut tokio::sync::mpsc::Receiver<BridgeMessage>,
) {
    while let Some(msg) = rx.recv().await {
        match msg {
            BridgeMessage::Progress {
                stage,
                message,
                current,
                total,
            } => {
                bridge::emit_progress(&app, &stage, &message, current, total);
            }
            BridgeMessage::NeedsLogin { message } => {
                let m = if message.is_empty() {
                    "Please log in to Benchling, then click Import."
                } else {
                    &message
                };
                bridge::emit_needs_login(&app, m);
                // Not terminal — the user may log in and retry Import.
            }
            BridgeMessage::Error { message } => {
                bridge::emit_error(&app, &message);
                // Not terminal — allow retry.
            }
            BridgeMessage::Complete {
                user_handle,
                tenant_host,
                items,
            } => {
                push_items(&app, &api, token.as_deref(), &user_handle, &tenant_host, items).await;
                // Terminal: stop driving this bridge session.
                return;
            }
        }
    }
}

/// POSTs gathered items to the Beakr backend push endpoint.
async fn push_items(
    app: &AppHandle,
    api: &str,
    token: Option<&str>,
    user_handle: &str,
    tenant_host: &str,
    items: Vec<serde_json::Value>,
) {
    let token = match token {
        Some(t) if !t.is_empty() => t,
        _ => {
            bridge::emit_error(app, "No device token. Pair this device with Beakr first.");
            return;
        }
    };

    let count = items.len();
    bridge::emit_progress(
        app,
        "push",
        &format!("Sending {count} items to Beakr…"),
        Some(count as u64),
        Some(count as u64),
    );

    let body = serde_json::json!({
        "project_id": serde_json::Value::Null,
        "tenant_host": tenant_host,
        "user_handle": user_handle,
        "items": items,
    });

    let url = format!("{api}/v1/connectors/benchling/push");
    let client = match reqwest::Client::builder()
        .user_agent(format!("BeakrDesktop/{}", env!("CARGO_PKG_VERSION")))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            bridge::emit_error(app, &format!("HTTP client error: {e}"));
            return;
        }
    };

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(serde_json::to_string(&body).unwrap_or_default())
        .send()
        .await;

    match resp {
        Ok(r) => {
            let status = r.status();
            let text = r.text().await.unwrap_or_default();
            if status.is_success() {
                let parsed: serde_json::Value =
                    serde_json::from_str(&text).unwrap_or_else(|_| serde_json::json!({}));
                bridge::emit_done(
                    app,
                    serde_json::json!({
                        "received": parsed.get("received"),
                        "sync_job_id": parsed.get("sync_job_id"),
                        "connector_id": parsed.get("connector_id"),
                        "items_sent": count,
                    }),
                );
            } else {
                bridge::emit_error(
                    app,
                    &format!("Beakr push failed (HTTP {status}): {text}"),
                );
            }
        }
        Err(e) => {
            bridge::emit_error(app, &format!("Beakr push request failed: {e}"));
        }
    }
}
