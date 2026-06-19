//! Tauri commands and import driver for the generic session connector.
//!
//! The session connector is provider-agnostic: a provider is looked up in the
//! registry (see `session::registry`) by its key, which supplies the URL to open
//! and the gather script to inject. Adding a provider needs no changes here.
//!
//! Flow (identical to the original Benchling connector, now parameterized by
//! `provider`):
//!   1. `connect_session(provider)` opens the provider's webview window (label
//!      `session-<provider>`) and starts a per-provider localhost data bridge (see
//!      `bridge.rs`). The bridge port is stashed per-provider in [`AppState`].
//!   2. The user logs into the provider with their own session inside that window.
//!   3. `session_import(provider)` injects that provider's gather script (see
//!      `session::scripts`) into the open window via `webview.eval`. The script
//!      reads with the session cookie and POSTs results back to the localhost
//!      bridge.
//!   4. The import driver consumes bridge messages, emits frontend events
//!      (`session:progress` / `:needs_login` / `:error`, each carrying `provider`),
//!      and on `Complete` POSTs the items to `{api_base}/v1/connectors/session-push`
//!      using the stored device token, then emits `session:done`.

use tauri::{AppHandle, Manager, State};

use crate::session::bridge::{self, BridgeMessage};
use crate::session::registry::{self, Provider};
use crate::session::scripts::PORT_PLACEHOLDER;
use crate::state::{AppState, SessionBridge};

/// Derive the Beakr backend API base from the WS URL (mirrors `claim_pairing_code`).
fn api_base() -> String {
    crate::ws_url()
        .replace("ws://", "http://")
        .replace("wss://", "https://")
        .replace("/v1/desktop-agent/ws", "")
}

/// Opens a provider's session webview window and starts its localhost data bridge.
///
/// Safe to call repeatedly: if the window already exists it is focused and the
/// existing bridge is reused.
#[tauri::command]
pub async fn connect_session(
    provider: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let registered: Provider =
        registry::lookup(&provider).ok_or_else(|| format!("Unknown session provider: {provider}"))?;

    let window_label = registry::window_label(&provider);

    // If the window already exists, just focus it (bridge is already running).
    if let Some(window) = app.get_webview_window(&window_label) {
        let _ = window.show();
        let _ = window.set_focus();
        return Ok(());
    }

    // Start the localhost bridge listener before opening the window so it is ready
    // by the time the user clicks Import. Each provider gets its own shutdown
    // signal so connecting one provider never tears down another's bridge.
    let shutdown = std::sync::Arc::new(tokio::sync::Notify::new());
    let (port, mut rx) = bridge::start_bridge(shutdown.clone()).await?;
    state.session_bridges.write().await.insert(
        provider.clone(),
        SessionBridge {
            port,
            shutdown: shutdown.clone(),
        },
    );

    // Drive the bridge message loop on a background task.
    {
        let app_drive = app.clone();
        let api = api_base();
        let token = state.auth_token.read().await.clone();
        let provider_drive = provider.clone();
        tauri::async_runtime::spawn(async move {
            run_import_driver(app_drive, provider_drive, api, token, &mut rx).await;
        });
    }

    // Open the provider's webview window.
    let url = registered
        .url
        .parse()
        .map_err(|e| format!("Invalid provider URL: {e}"))?;

    let builder = tauri::WebviewWindowBuilder::new(
        &app,
        &window_label,
        tauri::WebviewUrl::External(url),
    )
    .title(format!("Connect {provider}"))
    .inner_size(1100.0, 800.0)
    .resizable(true)
    .center();

    let window = builder
        .build()
        .map_err(|e| format!("Failed to open session window: {e}"))?;

    // For Benchling, start watching for login so we can capture the session cookie,
    // register the free connector with the backend, and emit `session:connected`
    // once the user has logged in. This is what makes the live `benchling_*` agent
    // tools work (see `session::benchling` and `tools::benchling`). Other steps of
    // the legacy import flow (gather script + bridge) remain available but are no
    // longer exposed in the UI.
    if provider == "benchling" {
        let app_watch = app.clone();
        let state_watch = (*state).clone();
        let label = window_label.clone();
        tauri::async_runtime::spawn(async move {
            crate::session::benchling::watch_for_login(app_watch, state_watch, label).await;
        });
    }

    // Clear the cached bridge entry when the window is closed so a later connect
    // starts fresh.
    {
        let state_owned = (*state).clone();
        let provider_on_close = provider.clone();
        window.on_window_event(move |event| {
            if let tauri::WindowEvent::Destroyed = event {
                let state_inner = state_owned.clone();
                let provider_inner = provider_on_close.clone();
                tauri::async_runtime::spawn(async move {
                    if let Some(entry) =
                        state_inner.session_bridges.write().await.remove(&provider_inner)
                    {
                        entry.shutdown.notify_waiters();
                    }
                });
            }
        });
    }

    Ok(())
}

/// Injects a provider's gather script into its open session window.
///
/// This drives the legacy one-shot wiki-import flow: the gather script verifies
/// the session, lists items, and POSTs them to the localhost bridge (which the
/// driver started in `connect_session` consumes). It is INTENTIONALLY no longer
/// exposed in the UI — the Benchling connector now exposes live agent tools
/// instead of a bulk import (see `session::benchling` / `tools::benchling`). The
/// command, gather script, and bridge are kept in the codebase so the import path
/// can be re-enabled without rebuilding it.
#[tauri::command]
pub async fn session_import(
    provider: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let registered: Provider =
        registry::lookup(&provider).ok_or_else(|| format!("Unknown session provider: {provider}"))?;

    let port = state
        .session_bridges
        .read()
        .await
        .get(&provider)
        .map(|b| b.port)
        .ok_or("Session window is not open. Click Connect first.")?;

    let window_label = registry::window_label(&provider);
    let window = app
        .get_webview_window(&window_label)
        .ok_or("Session window is not open. Click Connect first.")?;

    let script = registered
        .gather_script
        .replace(PORT_PLACEHOLDER, &port.to_string());

    window
        .eval(&script)
        .map_err(|e| format!("Failed to start import: {e}"))?;

    bridge::emit_progress(&app, &provider, "start", "Starting import…", None, None);
    Ok(())
}

/// Returns the current Benchling connection status so the UI can reflect
/// "Connected" after a reload (the `session:connected` event only fires once, at
/// connect time). `null` when no session has been captured.
#[tauri::command]
pub async fn benchling_status(
    state: State<'_, AppState>,
) -> Result<Option<serde_json::Value>, String> {
    Ok(state
        .benchling_session
        .read()
        .await
        .as_ref()
        .map(|s| {
            serde_json::json!({
                "connected": true,
                "user_handle": s.user_handle,
                "tenant_host": s.tenant_host,
            })
        }))
}

/// Consumes bridge messages until a terminal message arrives, emitting frontend
/// events and (on completion) pushing items to the Beakr backend.
async fn run_import_driver(
    app: AppHandle,
    provider: String,
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
                bridge::emit_progress(&app, &provider, &stage, &message, current, total);
            }
            BridgeMessage::NeedsLogin { message } => {
                let m = if message.is_empty() {
                    "Please log in, then click Import."
                } else {
                    &message
                };
                bridge::emit_needs_login(&app, &provider, m);
                // Not terminal — the user may log in and retry Import.
            }
            BridgeMessage::Error { message } => {
                bridge::emit_error(&app, &provider, &message);
                // Not terminal — allow retry.
            }
            BridgeMessage::Complete {
                user_handle,
                tenant_host,
                items,
            } => {
                push_items(
                    &app,
                    &provider,
                    &api,
                    token.as_deref(),
                    &user_handle,
                    &tenant_host,
                    items,
                )
                .await;
                // Terminal: stop driving this bridge session.
                return;
            }
        }
    }
}

/// POSTs gathered items to the Beakr backend generic session-push endpoint.
async fn push_items(
    app: &AppHandle,
    provider: &str,
    api: &str,
    token: Option<&str>,
    user_handle: &str,
    tenant_host: &str,
    items: Vec<serde_json::Value>,
) {
    let token = match token {
        Some(t) if !t.is_empty() => t,
        _ => {
            bridge::emit_error(
                app,
                provider,
                "No device token. Pair this device with Beakr first.",
            );
            return;
        }
    };

    let count = items.len();
    bridge::emit_progress(
        app,
        provider,
        "push",
        &format!("Sending {count} items to Beakr…"),
        Some(count as u64),
        Some(count as u64),
    );

    let body = serde_json::json!({
        "provider": provider,
        "project_id": serde_json::Value::Null,
        "tenant_host": tenant_host,
        "user_handle": user_handle,
        "items": items,
    });

    let url = format!("{api}/v1/connectors/session-push");
    let client = match reqwest::Client::builder()
        .user_agent(format!("BeakrDesktop/{}", env!("CARGO_PKG_VERSION")))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            bridge::emit_error(app, provider, &format!("HTTP client error: {e}"));
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
                    provider,
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
                    provider,
                    &format!("Beakr push failed (HTTP {status}): {text}"),
                );
            }
        }
        Err(e) => {
            bridge::emit_error(app, provider, &format!("Beakr push request failed: {e}"));
        }
    }
}
