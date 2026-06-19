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

const BENCHLING_ORIGIN: &str = "https://benchling.com";
const TENANT_HOST: &str = "benchling.com";

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

/// Polls the open benchling webview until the user is logged in, then captures the
/// session, registers the connector, and emits `session:connected`.
///
/// Idempotent and self-terminating: returns once connected, after the timeout, or
/// once the captured session is already set (a prior connect succeeded).
pub async fn watch_for_login(app: AppHandle, state: AppState, window_label: String) {
    let token = state.auth_token.read().await.clone();

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

        if let Some(cookie) = read_session_cookie(&app, &window_label) {
            match probe_users_me(&cookie).await {
                Ok(Some(handle)) => {
                    // Capture the session for the live agent tools.
                    *state.benchling_session.write().await = Some(BenchlingSession {
                        session_cookie: cookie,
                        tenant_host: TENANT_HOST.to_string(),
                        user_handle: handle.clone(),
                    });

                    // Register the connector + this device with the backend so the
                    // agent's tools can RPC here. A missing device token is a
                    // non-fatal warning: the session is still captured locally, and
                    // the user just needs to pair the device.
                    match token.as_deref() {
                        Some(t) if !t.is_empty() => {
                            if let Err(e) = register_connector(t, &handle).await {
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
                                // Still report connected: the local session works;
                                // registration can be retried by reconnecting.
                            }
                        }
                        _ => {
                            log::warn!(
                                "Benchling connected but no device token — pair this device with Beakr first"
                            );
                        }
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
                    return;
                }
                Ok(None) => {
                    // Not logged in yet — keep waiting.
                }
                Err(e) => {
                    log::debug!("Benchling users/me probe error (will retry): {e}");
                }
            }
        }

        tokio::time::sleep(LOGIN_POLL_INTERVAL).await;
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
    let mut was_live = false;
    loop {
        tokio::time::sleep(LIVENESS_INTERVAL).await;

        let session = { state.benchling_session.read().await.clone() };
        match session {
            None => {
                if was_live {
                    // A tool call cleared the session on a 401 — surface it now.
                    crate::session::bridge::emit_disconnected(&app, "benchling");
                    if let Some(token) = state.auth_token.read().await.clone() {
                        report_disconnect(&token).await;
                    }
                    was_live = false;
                }
            }
            Some(sess) => match probe_users_me(&sess.session_cookie).await {
                Ok(Some(_)) => was_live = true,
                Ok(None) => {
                    // 401 — the session expired or the user logged out.
                    *state.benchling_session.write().await = None;
                    crate::session::bridge::emit_disconnected(&app, "benchling");
                    if let Some(token) = state.auth_token.read().await.clone() {
                        report_disconnect(&token).await;
                    }
                    was_live = false;
                }
                Err(_) => { /* transient error — leave the session intact */ }
            },
        }
    }
}
