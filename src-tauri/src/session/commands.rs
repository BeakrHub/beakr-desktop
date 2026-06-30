//! Tauri commands for the live session connector.
//!
//! The session connector is provider-agnostic: a provider is looked up in the
//! registry (see `session::registry`) by its key, which supplies the URL to open.
//! Adding a provider needs no changes here.
//!
//! Flow:
//!   1. `connect_session(provider)` opens the provider's webview window (label
//!      `session-<provider>`) where the user logs in with their own session.
//!   2. For Benchling, it spawns `session::benchling::watch_for_login`, which polls
//!      the webview for the logged-in session cookie, captures it into
//!      [`AppState::benchling_session`], registers the free connector with the
//!      backend, and emits `session:connected`. The live `benchling_*` agent tools
//!      then use the captured cookie (see `tools::benchling`).

use tauri::{AppHandle, Manager, State};

use crate::session::registry::{self, Provider};
use crate::state::AppState;

/// Opens a provider's session webview window so the user can log in.
///
/// Safe to call repeatedly: if the window already exists it is simply focused.
#[tauri::command]
pub async fn connect_session(
    provider: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let registered: Provider = registry::lookup(&provider)
        .ok_or_else(|| format!("Unknown session provider: {provider}"))?;

    let window_label = registry::window_label(&provider);

    // If the window already exists, just focus it.
    if let Some(window) = app.get_webview_window(&window_label) {
        let _ = window.show();
        let _ = window.set_focus();
        return Ok(());
    }

    // Open the provider's webview window.
    let url = registered
        .url
        .parse()
        .map_err(|e| format!("Invalid provider URL: {e}"))?;

    let builder =
        tauri::WebviewWindowBuilder::new(&app, &window_label, tauri::WebviewUrl::External(url))
            .title(format!("Connect {provider}"))
            .inner_size(1100.0, 800.0)
            .resizable(true)
            .center();

    builder
        .build()
        .map_err(|e| format!("Failed to open session window: {e}"))?;

    // For Benchling, start watching for login so we can capture the session cookie,
    // register the free connector with the backend, and emit `session:connected`
    // once the user has logged in. This is what makes the live `benchling_*` agent
    // tools work (see `session::benchling` and `tools::benchling`).
    if provider == "benchling" {
        let app_watch = app.clone();
        let state_watch = (*state).clone();
        let label = window_label.clone();
        tauri::async_runtime::spawn(async move {
            crate::session::benchling::watch_for_login(app_watch, state_watch, label).await;
        });
    }

    Ok(())
}

/// Returns the current Benchling connection status so the UI can reflect
/// "Connected" after a reload (the `session:connected` event only fires once, at
/// connect time). `null` when no session has been captured.
#[tauri::command]
pub async fn benchling_status(
    state: State<'_, AppState>,
) -> Result<Option<serde_json::Value>, String> {
    Ok(state.benchling_session.read().await.as_ref().map(|s| {
        serde_json::json!({
            "connected": true,
            "user_handle": s.user_handle,
            "tenant_host": s.tenant_host,
        })
    }))
}
