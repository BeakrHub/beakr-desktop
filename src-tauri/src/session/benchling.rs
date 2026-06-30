//! Benchling connect: session capture + connector registration.
//!
//! When the user clicks "Connect Benchling", `session::commands::connect_session`
//! opens the benchling.com webview and (for the benchling provider) spawns
//! [`watch_for_login`]. That task polls the webview's cookie store until the user
//! has logged in, then:
//!   1. Captures the HttpOnly `session` cookie into [`AppState::benchling_session`]
//!      (the live `benchling_*` agent tools read it from there — see
//!      `tools::benchling`).
//!   2. Probes `GET https://benchling.com/1/api/users/me` with that cookie to
//!      confirm the session is valid and read the user's handle.
//!   3. Registers this device's free Benchling connector with the Beakr backend
//!      (`POST {api_base}/v1/connectors/benchling-desktop/connect`) so the agent's
//!      tools can RPC back here.
//!   4. Emits the `session:connected` frontend event so the UI shows "Connected".
//!
//! Session-capture choice (cookie vs in-webview eval) is documented in
//! `tools::benchling`. We capture the cookie so the tools are pure-Rust HTTP and
//! work even after the connect window is closed.

use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager};
use url::Url;

use crate::state::{AppState, BenchlingSession};

/// How long to keep polling the webview for a logged-in session before giving up.
const LOGIN_WATCH_TIMEOUT: Duration = Duration::from_secs(600);
/// Interval between cookie/probe polls while waiting for login.
const LOGIN_POLL_INTERVAL: Duration = Duration::from_secs(3);
/// How long startup should wait for the persisted webview cookie store to load.
const STARTUP_RESTORE_TIMEOUT: Duration = Duration::from_secs(20);
/// Interval between startup cookie/probe polls.
const STARTUP_RESTORE_POLL_INTERVAL: Duration = Duration::from_millis(750);

const BENCHLING_ORIGIN: &str = "https://benchling.com";
const TENANT_HOST: &str = "benchling.com";
const RESTORE_WINDOW_LABEL: &str = "session-benchling-restore";

/// Derive the Beakr backend API base from the WS URL (mirrors `claim_pairing_code`).
fn api_base() -> String {
    crate::ws_url()
        .replace("ws://", "http://")
        .replace("wss://", "https://")
        .replace("/v1/desktop-agent/ws", "")
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(format!("BeakrDesktop/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("failed to build HTTP client")
}

/// Reads the benchling.com `session` cookie from the open webview's cookie store.
///
/// macOS WKWebView (wry) returns HttpOnly cookies via `WKHTTPCookieStore`, so the
/// `session` cookie is visible here even though it is HttpOnly. This must be called
/// off the main/event-loop thread (it blocks on the event loop); the watch task
/// below runs on the async runtime, so that holds.
fn read_session_cookie(app: &AppHandle, window_label: &str) -> Option<String> {
    let window = app.get_webview_window(window_label)?;
    let url = Url::parse(BENCHLING_ORIGIN).ok()?;
    let cookies = window.cookies_for_url(url).ok()?;
    cookies
        .into_iter()
        .find(|c| c.name() == "session")
        .map(|c| c.value().to_string())
}

/// Probes `GET /1/api/users/me` with the given session cookie.
///
/// Returns `Ok(Some(handle))` when logged in, `Ok(None)` when the session is not
/// (yet) valid (401 / idle), or `Err` on a transport/other-status failure.
async fn probe_users_me(session_cookie: &str) -> Result<Option<String>, String> {
    let resp = http_client()
        .get(format!("{BENCHLING_ORIGIN}/1/api/users/me"))
        .header("Cookie", format!("session={session_cookie}"))
        .header("X-Requested-With", "XMLHttpRequest")
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("users/me request failed: {e}"))?;

    if resp.status().as_u16() == 401 {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(format!("users/me returned HTTP {}", resp.status()));
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("users/me bad JSON: {e}"))?;
    let handle = body
        .get("handle")
        .and_then(|v| v.as_str())
        .or_else(|| body.get("username").and_then(|v| v.as_str()))
        .map(|s| s.to_string())
        .or_else(|| {
            body.get("email")
                .and_then(|v| v.as_str())
                .and_then(|e| e.split('@').next())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());
    Ok(Some(handle))
}

/// Registers this device's free Benchling connector with the Beakr backend.
///
/// `POST {api_base}/v1/connectors/benchling-desktop/connect` with the device token
/// authorizes this device so the agent's Benchling tools can be RPC'd here.
async fn register_connector(token: &str, user_handle: &str) -> Result<(), String> {
    let url = format!("{}/v1/connectors/benchling-desktop/connect", api_base());
    let body = serde_json::json!({
        "tenant_host": TENANT_HOST,
        "user_handle": user_handle,
    });

    let resp = http_client()
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(serde_json::to_string(&body).unwrap_or_default())
        .send()
        .await
        .map_err(|e| format!("connector register request failed: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("connector register failed (HTTP {status}): {text}"));
    }
    Ok(())
}

/// Register the currently captured Benchling session with Beakr, if possible.
///
/// This is intentionally best-effort: the local Benchling tools can still work
/// with a captured cookie even if the backend registration is temporarily down.
pub async fn register_current_session_with_backend(app: &AppHandle, state: &AppState) {
    let session = { state.benchling_session.read().await.clone() };
    let token = { state.auth_token.read().await.clone() };

    let (Some(session), Some(token)) = (session, token) else {
        return;
    };
    if token.is_empty() {
        return;
    }

    if let Err(e) = register_connector(&token, &session.user_handle).await {
        log::warn!("Benchling connector registration failed: {e}");
        let _ = app.emit(
            "session:error",
            serde_json::json!({
                "provider": "benchling",
                "message": format!(
                    "Connected to Benchling, but registering with Beakr failed: {e}"
                ),
            }),
        );
    }
}

/// Captures a valid Benchling session from a webview, registers it with Beakr,
/// and emits the frontend connected event.
async fn capture_valid_session(
    app: &AppHandle,
    state: &AppState,
    window_label: &str,
) -> Result<Option<String>, String> {
    let Some(cookie) = read_session_cookie(app, window_label) else {
        return Ok(None);
    };

    let Some(handle) = probe_users_me(&cookie).await? else {
        return Ok(None);
    };

    // Capture the session for the live agent tools.
    *state.benchling_session.write().await = Some(BenchlingSession {
        session_cookie: cookie,
        tenant_host: TENANT_HOST.to_string(),
        user_handle: handle.clone(),
    });

    // Register the connector + this device with the backend so the agent's tools
    // can RPC here. A missing device token is a non-fatal warning: the session is
    // still captured locally, and registration is retried when a token is set.
    if state
        .auth_token
        .read()
        .await
        .as_deref()
        .is_some_and(|token| !token.is_empty())
    {
        register_current_session_with_backend(app, state).await;
    } else {
        log::warn!("Benchling connected but no device token — pair this device with Beakr first");
    }

    let _ = app.emit(
        "session:connected",
        serde_json::json!({
            "provider": "benchling",
            "user_handle": handle,
            "tenant_host": TENANT_HOST,
        }),
    );
    log::info!("Benchling session connected for handle={handle}");
    Ok(Some(handle))
}

/// Polls the open benchling webview until the user is logged in, then captures the
/// session, registers the connector, and emits `session:connected`.
///
/// Idempotent and self-terminating: returns once connected, after the timeout, or
/// once the captured session is already set (a prior connect succeeded).
pub async fn watch_for_login(app: AppHandle, state: AppState, window_label: String) {
    let deadline = tokio::time::Instant::now() + LOGIN_WATCH_TIMEOUT;
    loop {
        if tokio::time::Instant::now() >= deadline {
            log::info!("Benchling login watch timed out before login was detected");
            return;
        }
        // Stop if the connect window was closed by the user.
        if app.get_webview_window(&window_label).is_none() {
            log::info!("Benchling connect window closed before login completed");
            return;
        }

        match capture_valid_session(&app, &state, &window_label).await {
            Ok(Some(_)) => return,
            Ok(None) => {
                // Not logged in yet — keep waiting.
            }
            Err(e) => {
                log::debug!("Benchling users/me probe error (will retry): {e}");
            }
        }

        tokio::time::sleep(LOGIN_POLL_INTERVAL).await;
    }
}

fn close_restore_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(RESTORE_WINDOW_LABEL) {
        let _ = window.close();
    }
}

fn open_restore_window(app: &AppHandle) -> Result<(), String> {
    if app.get_webview_window(RESTORE_WINDOW_LABEL).is_some() {
        return Ok(());
    }

    let url = Url::parse(BENCHLING_ORIGIN).map_err(|e| format!("invalid Benchling URL: {e}"))?;
    tauri::WebviewWindowBuilder::new(app, RESTORE_WINDOW_LABEL, tauri::WebviewUrl::External(url))
        .title("Benchling session restore")
        .inner_size(640.0, 480.0)
        .resizable(false)
        .visible(false)
        .skip_taskbar(true)
        .build()
        .map_err(|e| format!("failed to open hidden Benchling restore window: {e}"))?;

    Ok(())
}

/// Restores a previously connected Benchling session from the webview cookie
/// store on app startup.
///
/// The Benchling `session` cookie itself remains in the OS/webview cookie store;
/// Beakr only rehydrates its in-memory copy after validating it with
/// `/1/api/users/me`. This prevents the liveness task from immediately marking a
/// healthy backend connector as disconnected just because the app process
/// restarted.
pub async fn restore_session_on_startup(app: AppHandle, state: AppState) -> bool {
    if state.benchling_session.read().await.is_some() {
        return true;
    }

    let manual_window_label = "session-benchling";
    let label = if app.get_webview_window(manual_window_label).is_some() {
        manual_window_label
    } else {
        match open_restore_window(&app) {
            Ok(()) => RESTORE_WINDOW_LABEL,
            Err(e) => {
                log::debug!("Benchling startup restore skipped: {e}");
                return false;
            }
        }
    };

    let deadline = tokio::time::Instant::now() + STARTUP_RESTORE_TIMEOUT;
    loop {
        match capture_valid_session(&app, &state, label).await {
            Ok(Some(handle)) => {
                if label == RESTORE_WINDOW_LABEL {
                    close_restore_window(&app);
                }
                log::info!("Restored Benchling session on startup for handle={handle}");
                return true;
            }
            Ok(None) => {
                // Cookie store not ready yet, or the user is not logged in.
            }
            Err(e) => {
                log::debug!("Benchling startup restore probe error (will retry): {e}");
            }
        }

        if tokio::time::Instant::now() >= deadline {
            if label == RESTORE_WINDOW_LABEL {
                close_restore_window(&app);
            }
            log::info!("No valid Benchling session found during startup restore");
            return false;
        }

        tokio::time::sleep(STARTUP_RESTORE_POLL_INTERVAL).await;
    }
}

/// Interval between background liveness checks of the captured Benchling session.
const LIVENESS_INTERVAL: Duration = Duration::from_secs(60);

/// Reports to the backend that the live Benchling session has ended, so the web
/// connector card reflects the lost session (best-effort).
async fn report_disconnect(token: &str) {
    let url = format!("{}/v1/connectors/benchling-desktop/disconnect", api_base());
    let _ = http_client()
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await;
}

/// Long-lived task that keeps the UI and backend honest about the live Benchling
/// session. Every [`LIVENESS_INTERVAL`] it inspects the captured session: if it
/// has died (401 from `/1/api/users/me`) or was cleared by a failed tool call,
/// it clears the session, emits `session:disconnected`, and reports the
/// disconnect to the backend. Transient network errors are ignored so a blip
/// does not produce a false disconnect.
pub async fn watch_session_liveness(app: AppHandle, state: AppState) {
    // Whether the current "dead" period still needs a disconnect reported. Starts
    // true so a fresh launch with NO captured session (e.g. after quit -> reopen,
    // where the in-memory session is gone but the backend connector may still read
    // "healthy" and the device is back online) reports once and the web reflects
    // "not connected".
    let mut needs_report = true;
    // Check immediately on startup, then once per interval.
    loop {
        let session = { state.benchling_session.read().await.clone() };
        match session {
            None => {
                // No live session. Report once; retry next tick if the device
                // token has not been loaded into state yet.
                if needs_report {
                    if let Some(token) = state.auth_token.read().await.clone() {
                        let _ = app.emit(
                            "session:disconnected",
                            serde_json::json!({ "provider": "benchling" }),
                        );
                        report_disconnect(&token).await;
                        needs_report = false;
                    }
                }
            }
            Some(sess) => match probe_users_me(&sess.session_cookie).await {
                // Live — re-arm so a later death/quit is reported again.
                Ok(Some(_)) => needs_report = true,
                Ok(None) => {
                    // 401 — the session expired or the user logged out.
                    *state.benchling_session.write().await = None;
                    if needs_report {
                        let _ = app.emit(
                            "session:disconnected",
                            serde_json::json!({ "provider": "benchling" }),
                        );
                        if let Some(token) = state.auth_token.read().await.clone() {
                            report_disconnect(&token).await;
                        }
                        needs_report = false;
                    }
                }
                Err(_) => { /* transient error — leave the session intact */ }
            },
        }

        tokio::time::sleep(LIVENESS_INTERVAL).await;
    }
}
