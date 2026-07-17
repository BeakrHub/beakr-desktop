use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use tauri::{AppHandle, Emitter};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, http::HeaderValue, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::state::{AppState, ConnectionStatus};
use crate::tools;
use crate::ws::protocol::{IncomingMessage, OutgoingMessage, ResponseStatus};

/// App-level heartbeat that refreshes the engine's Redis online key.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(45);
/// WebSocket-level Ping cadence used purely for liveness detection. A healthy
/// link answers each Ping with a Pong almost immediately; the absence of any
/// inbound frame within `LIVENESS_TIMEOUT` is how we detect a half-open socket
/// (sleep/wake, Wi-Fi/VPN/route change) that the OS hasn't yet torn down.
const PING_INTERVAL: Duration = Duration::from_secs(20);
/// Max time without ANY inbound frame before we treat the link as dead and force
/// a reconnect. Must be `> PING_INTERVAL` (so a live link always refreshes in
/// time) and `<` the engine's 60s online-key TTL (so `is_online` recovers via a
/// fresh `register()` without a manual app restart).
const LIVENESS_TIMEOUT: Duration = Duration::from_secs(30);
/// How often we re-check the liveness deadline.
const LIVENESS_CHECK_INTERVAL: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Outbound frame buffer between spawned request tasks and the socket loop.
/// Sized for bursts of streamed chunks; senders await (backpressure) when full.
const OUTBOUND_BUFFER: usize = 256;

/// WebSocket close codes from the server.
const CLOSE_REVOKED: u16 = 4010;
const CLOSE_SESSION_EXPIRED: u16 = 4011;

pub struct WsClient {
    app: AppHandle,
    state: AppState,
    ws_url: String,
}

impl WsClient {
    pub fn new(app: AppHandle, state: AppState, ws_url: String) -> Self {
        Self { app, state, ws_url }
    }

    /// Main entry point: connect and run, with automatic reconnection.
    pub async fn run(&self) {
        log::info!("WsClient starting, url={}", self.ws_url);
        let mut attempt = 0u32;

        loop {
            self.set_status(ConnectionStatus::Connecting).await;
            log::debug!("Connection attempt {attempt}");

            match self.connect_and_run().await {
                Ok(close_code) => {
                    log::debug!("Connection closed with code: {close_code:?}");
                    if close_code == Some(CLOSE_REVOKED) {
                        log::warn!("Device revoked by server");
                        self.set_status(ConnectionStatus::Revoked).await;
                        // Unlink authoritatively in Rust: a revoked device can be
                        // revoked-at-startup (the app may have no webview yet —
                        // the window opens on demand), so we can't rely on the
                        // frontend's token_invalid listener to clear the token +
                        // flip the tray.
                        if let Err(e) =
                            crate::commands::clear_device_token(&self.app, &self.state).await
                        {
                            log::error!("Failed to clear device token after revocation: {e}");
                        }
                        // Also notify the frontend (when a window is open) so it
                        // routes to the PairingScreen instead of the paired view.
                        let _ = self.app.emit("token_invalid", ());
                        return;
                    }
                    if close_code == Some(CLOSE_SESSION_EXPIRED) {
                        log::info!("Session expired, reconnecting immediately");
                        let _ = self.app.emit("token_refresh_needed", ());
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        // Fall through to reconnect with attempt = 0 (no backoff)
                    }
                    // Normal disconnect — reconnect
                    attempt = 0;
                }
                Err(e) => {
                    log::error!("WebSocket error: {e}");
                    attempt = attempt.saturating_add(1);
                }
            }

            // Stop reconnecting if the user explicitly disconnected.
            if self.state.shutdown_requested.load(Ordering::SeqCst) {
                self.set_status(ConnectionStatus::Disconnected).await;
                return;
            }

            self.set_status(ConnectionStatus::Reconnecting).await;

            // Exponential backoff with jitter
            let base = Duration::from_secs(1 << attempt.min(4));
            let backoff = base.min(MAX_BACKOFF);
            let jitter_factor = rand::thread_rng().gen_range(0.8..1.2);
            let wait = backoff.mul_f64(jitter_factor);

            log::info!(
                "Reconnecting in {:.1}s (attempt {attempt})",
                wait.as_secs_f64()
            );

            // Request a fresh token from the frontend before reconnecting
            let _ = self.app.emit("token_refresh_needed", ());

            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = self.state.ws_shutdown.notified() => {}
            }

            // Re-check after waking: a disconnect during backoff should stop here.
            if self.state.shutdown_requested.load(Ordering::SeqCst) {
                self.set_status(ConnectionStatus::Disconnected).await;
                return;
            }

            // Brief wait for token to arrive
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Connect, register, and run the message loop. Returns the close code if any.
    async fn connect_and_run(
        &self,
    ) -> Result<Option<u16>, Box<dyn std::error::Error + Send + Sync>> {
        let token = self.state.auth_token.read().await.clone();

        // Build request — use subprotocol auth in production, query params in dev
        let user_agent = format!("BeakrDesktop/{}", env!("CARGO_PKG_VERSION"));

        let connect_result = if let Some(token) = token {
            let mut request = self.ws_url.as_str().into_client_request()?;
            let subprotocol = format!("beakr-v1, bearer.{token}");
            request.headers_mut().insert(
                "Sec-WebSocket-Protocol",
                HeaderValue::from_str(&subprotocol)?,
            );
            request
                .headers_mut()
                .insert("User-Agent", HeaderValue::from_str(&user_agent)?);
            tokio_tungstenite::connect_async(request).await
        } else if cfg!(debug_assertions) {
            // Dev mode: use query params for auth (matches backend dev bypass)
            let dev_url = format!(
                "{}?identity_id=dev_local&email=dev@localhost&identity_name=dev&display_name=Dev+User",
                self.ws_url
            );
            let mut request = dev_url.as_str().into_client_request()?;
            request
                .headers_mut()
                .insert("User-Agent", HeaderValue::from_str(&user_agent)?);
            tokio_tungstenite::connect_async(request).await
        } else {
            return Err("No auth token available".into());
        };

        let (ws_stream, _response) = match connect_result {
            Ok(pair) => pair,
            Err(e) => {
                // A 403 at the HTTP handshake is the engine rejecting this device
                // token outright ("Invalid or revoked device token"). It happens
                // *before* the WS upgrade, so it never arrives as a 4010 close
                // frame — without this it falls through to the generic reconnect
                // backoff and loops forever. Map it to the same terminal-revoked
                // path as 4010: stop reconnecting, mark Revoked, emit token_invalid.
                if is_revoked_handshake(&e) {
                    log::warn!("Handshake rejected with 403 Forbidden — device token revoked");
                    return Ok(Some(CLOSE_REVOKED));
                }
                return Err(e.into());
            }
        };
        let (mut write, mut read) = ws_stream.split();

        // Send register message
        let device_name = self.state.device_name.read().await.clone();
        let scoped_folders = self.state.scoped_folders.read().await.clone();

        // Per-CLI readiness rides registration (ENG-1536): free signals only,
        // no version spawn here — keep the connect handshake snappy.
        let settings = crate::config::load_settings(&self.app);
        let coding_agents = {
            use crate::tools::coding_agent::readiness::detect;
            let claude = detect(
                "claude",
                settings.claude_binary_path.as_deref(),
                settings.claude_auth_ok,
                false,
            );
            let codex = detect("codex", None, settings.codex_auth_ok, false);
            let (claude, codex) = tokio::join!(claude, codex);
            Some(vec![claude, codex])
        };

        let register = OutgoingMessage::Register {
            device_name,
            platform: current_platform().to_string(),
            scoped_folders,
            platform_version: Some(os_version()),
            app_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            coding_agents,
        };

        let register_json = serde_json::to_string(&register)?;
        write.send(Message::Text(register_json)).await?;

        // Wait for registered response
        let registered_msg = tokio::time::timeout(Duration::from_secs(10), read.next())
            .await?
            .ok_or("Connection closed before registration")??;

        // Handle close frames before trying to parse as JSON —
        // tungstenite's to_text() returns close frame reasons as text,
        // which would cause a confusing serde parse error.
        if let Message::Close(frame) = &registered_msg {
            let code = frame.as_ref().map(|f| f.code.into());
            log::info!("Server closed connection during registration: code={code:?}");
            return Ok(code);
        }

        let registered_text = registered_msg
            .to_text()
            .map_err(|_| format!("Expected text message but got: {:?}", registered_msg))?;

        let incoming: IncomingMessage = serde_json::from_str(registered_text).map_err(|e| {
            format!("Failed to parse registration response: {e} (raw: {registered_text:?})")
        })?;

        let device_id = match incoming {
            IncomingMessage::Registered { device_id } => device_id,
            _ => return Err("Expected 'registered' message".into()),
        };

        *self.state.device_id.write().await = Some(device_id.clone());
        self.set_status(ConnectionStatus::Connected).await;
        log::info!("Connected and registered as device {device_id}");

        // Run message loop
        let close_code = self.message_loop(&mut write, &mut read).await;

        self.set_status(ConnectionStatus::Disconnected).await;
        Ok(close_code)
    }

    /// Main message loop: heartbeat + incoming request handling.
    async fn message_loop(
        &self,
        write: &mut futures_util::stream::SplitSink<
            WebSocketStream<MaybeTlsStream<TcpStream>>,
            Message,
        >,
        read: &mut futures_util::stream::SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    ) -> Option<u16> {
        let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
        heartbeat.tick().await; // consume the first immediate tick

        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.tick().await; // consume the first immediate tick

        let mut liveness = tokio::time::interval(LIVENESS_CHECK_INTERVAL);
        liveness.tick().await; // consume the first immediate tick

        // Outbound channel for spawned request tasks (ENG-1527). Handlers run
        // as their own tasks so a minutes-long tool call can't stall this
        // select loop — stalling it would starve the Ping/Pong liveness cycle
        // and force a spurious reconnect mid-run (the ENG-1261 detector), and
        // would serialize every other tool behind the slow one. Tasks send
        // their frames here; only this loop touches the socket.
        let (out_tx, mut out_rx) =
            tokio::sync::mpsc::channel::<OutgoingMessage>(OUTBOUND_BUFFER);

        // Time of the last inbound frame of ANY kind. A live link refreshes this
        // on every Ping->Pong round-trip; a half-open socket lets it go stale,
        // which the liveness branch turns into a reconnect. Without this the read
        // arm could block forever on a silently-dead socket while the tray still
        // showed "Connected" and the engine's online key expired (ENG-1261).
        let mut last_inbound = Instant::now();

        loop {
            tokio::select! {
                _ = heartbeat.tick() => {
                    let msg = serde_json::to_string(&OutgoingMessage::Heartbeat)
                        .unwrap_or_default();
                    if write.send(Message::Text(msg)).await.is_err() {
                        return None;
                    }
                }

                _ = ping.tick() => {
                    // Active liveness probe: on a dead path the Pong never comes
                    // back, so `last_inbound` goes stale and the liveness branch
                    // below forces a reconnect. (A `write.send` that errors here —
                    // e.g. once the OS finally times out the socket — also exits.)
                    if write.send(Message::Ping(Vec::new())).await.is_err() {
                        return None;
                    }
                }

                _ = liveness.tick() => {
                    if link_is_dead(last_inbound.elapsed(), LIVENESS_TIMEOUT) {
                        log::warn!(
                            "No inbound frame for {:?} (> {LIVENESS_TIMEOUT:?}) — link is dead, reconnecting",
                            last_inbound.elapsed(),
                        );
                        return None;
                    }
                }

                outgoing = out_rx.recv() => {
                    // recv() can't yield None here: `out_tx` lives in this
                    // scope for the whole loop, so the channel never closes.
                    if let Some(msg) = outgoing {
                        match serde_json::to_string(&msg) {
                            Ok(json) => {
                                if let Err(e) = write.send(Message::Text(json)).await {
                                    log::error!("Failed to send outgoing frame: {e}");
                                    return None;
                                }
                            }
                            Err(e) => log::error!("Failed to serialize outgoing frame: {e}"),
                        }
                    }
                }

                msg = read.next() => {
                    match msg {
                        Some(Ok(message)) => {
                            // Any inbound frame (including Pong) proves the link is
                            // alive — record it before dispatching.
                            last_inbound = Instant::now();
                            match message {
                                Message::Text(text) => {
                                    self.handle_text_message(&text, &out_tx);
                                }
                                Message::Close(frame) => {
                                    let code = frame.as_ref().map(|f| f.code.into());
                                    log::info!("WebSocket closed with code: {code:?}");
                                    return code;
                                }
                                // Pong / Ping / Binary: liveness already recorded.
                                _ => {}
                            }
                        }
                        Some(Err(e)) => {
                            log::error!("WebSocket read error: {e}");
                            return None;
                        }
                        None => {
                            log::info!("WebSocket stream ended");
                            return None;
                        }
                    }
                }

                _ = self.state.folders_changed.notified() => {
                    let folders = self.state.scoped_folders.read().await.clone();
                    let msg = serde_json::to_string(&OutgoingMessage::UpdateFolders {
                        scoped_folders: folders,
                    }).unwrap_or_default();
                    if write.send(Message::Text(msg)).await.is_err() {
                        return None;
                    }
                    log::info!("Sent folder update to server");
                }

                _ = self.state.ws_shutdown.notified() => {
                    // Only tear down on a real, user-requested disconnect. A stray
                    // notify permit left over from a previous disconnect must not
                    // close a freshly re-established connection.
                    if self.state.shutdown_requested.load(Ordering::SeqCst) {
                        let _ = write.send(Message::Close(None)).await;
                        return None;
                    }
                }
            }
        }
    }

    /// Handle an incoming text message. Requests are dispatched as their own
    /// tasks (never awaited here — see the outbound-channel note in
    /// `message_loop`); cancels resolve against the in-flight registry.
    fn handle_text_message(
        &self,
        text: &str,
        out_tx: &tokio::sync::mpsc::Sender<OutgoingMessage>,
    ) {
        let incoming: IncomingMessage = match serde_json::from_str(text) {
            Ok(m) => m,
            Err(e) => {
                log::warn!("Failed to parse incoming message: {e}");
                return;
            }
        };

        match incoming {
            IncomingMessage::Request {
                request_id,
                tool,
                params,
            } => {
                let app = self.app.clone();
                let state = self.state.clone();
                let out_tx = out_tx.clone();
                tokio::spawn(run_request(app, state, request_id, tool, params, out_tx));
            }
            IncomingMessage::Cancel { request_id } => {
                if self.state.inflight.cancel(&request_id) {
                    log::info!("Cancelled in-flight request {request_id}");
                } else {
                    // Protocol contract: cancelling an unknown/finished id is
                    // a no-op (the terminal response may already be in flight).
                    log::debug!("Cancel for unknown/finished request {request_id}");
                }
            }
            IncomingMessage::Registered { .. } => {
                log::warn!("Unexpected 'registered' message during message loop");
            }
        }
    }

    async fn set_status(&self, status: ConnectionStatus) {
        crate::tray::update_tray_status(&self.app, &status);
        let status_str = serde_json::to_value(&status).ok();
        *self.state.ws_status.write().await = status;
        let _ = self.app.emit("ws:status_changed", status_str);
    }
}

/// Body of one spawned request task (ENG-1527): register for cancellation,
/// run the tool, always emit exactly one terminal `Response` — success, error,
/// or "cancelled" — so the engine side never hangs on a cancelled request.
async fn run_request(
    app: AppHandle,
    state: AppState,
    request_id: String,
    tool: String,
    params: serde_json::Value,
    out_tx: tokio::sync::mpsc::Sender<OutgoingMessage>,
) {
    let scoped_folders = state.scoped_folders.read().await.clone();
    let cancel = state.inflight.register(&request_id);

    // Notify frontend that a tool request started
    let _ = app.emit(
        "tool:request_started",
        serde_json::json!({
            "request_id": &request_id,
            "tool": &tool,
            "params": &params,
        }),
    );

    // Read-only tools are safely droppable mid-flight, so a plain select is
    // enough. Streaming tools (the coding runs of ENG-1528) must clean up
    // their children on cancel, so they receive the signal itself and manage
    // their own cancellation instead of being dropped by this outer race.
    let response = if tools::is_streaming(&tool) {
        let stream = crate::ws::ToolStream::new(request_id.clone(), out_tx.clone());
        tools::dispatch_streaming(&app, &state, &tool, params, &scoped_folders, &stream, cancel)
            .await
    } else {
        let mut cancel = cancel;
        tokio::select! {
            r = tools::dispatch_request(&tool, params, &scoped_folders, &state) => r,
            _ = cancel.cancelled() => Err("cancelled by server".to_string()),
        }
    };

    state.inflight.finish(&request_id);

    let (outgoing, result_status) = match response {
        Ok((data, bytes)) => (
            OutgoingMessage::Response {
                request_id: request_id.clone(),
                status: ResponseStatus::Success,
                data: Some(data),
                error: None,
                bytes_transferred: bytes,
            },
            "success",
        ),
        Err(e) => (
            OutgoingMessage::Response {
                request_id: request_id.clone(),
                status: ResponseStatus::Error,
                data: None,
                error: Some(e),
                bytes_transferred: None,
            },
            "error",
        ),
    };

    // Notify frontend that the request completed
    let _ = app.emit(
        "tool:request_completed",
        serde_json::json!({
            "request_id": &request_id,
            "status": result_status,
        }),
    );

    // A send failure means the connection this request arrived on is gone;
    // the engine's request future died with it, so dropping the response
    // mirrors the pre-ENG-1527 behavior.
    if out_tx.send(outgoing).await.is_err() {
        log::warn!("Connection closed before response for {request_id} could be sent");
    }
}

/// True when a connect error is a 403 Forbidden returned at the HTTP handshake —
/// the engine's signal that the device token is invalid or revoked. This is
/// distinct from a transient network failure: a 403 will never succeed on retry,
/// so it's treated as terminal-revoked rather than backed off.
fn is_revoked_handshake(err: &tokio_tungstenite::tungstenite::Error) -> bool {
    use tokio_tungstenite::tungstenite::http::StatusCode;
    use tokio_tungstenite::tungstenite::Error;
    matches!(err, Error::Http(response) if response.status() == StatusCode::FORBIDDEN)
}

/// Whether the WebSocket link should be considered dead, given how long it has
/// been since the last inbound frame. Extracted as a pure function so the
/// liveness invariant is unit-testable without a live socket.
fn link_is_dead(since_last_inbound: Duration, timeout: Duration) -> bool {
    since_last_inbound > timeout
}

fn current_platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    }
}

pub fn os_version() -> String {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("sw_vers")
            .arg("-productVersion")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "unknown".to_string())
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "ver"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "unknown".to_string())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        "unknown".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::tungstenite::http::{Response, StatusCode};
    use tokio_tungstenite::tungstenite::Error;

    fn http_error(status: StatusCode) -> Error {
        let body: Option<Vec<u8>> = None;
        Error::Http(Response::builder().status(status).body(body).unwrap())
    }

    #[test]
    fn forbidden_handshake_is_terminal_revoked() {
        // Regression: the engine rejects a revoked/invalid device token with a
        // 403 at the HTTP handshake (pre-WS-upgrade, so never a 4010 close).
        // It must be classified terminal so run() stops instead of looping forever.
        assert!(is_revoked_handshake(&http_error(StatusCode::FORBIDDEN)));
    }

    #[test]
    fn other_http_statuses_are_not_revoked() {
        // 401/500 are not the revoke signal — they must stay retryable, not
        // collapse into a permanent unlink.
        assert!(!is_revoked_handshake(&http_error(StatusCode::UNAUTHORIZED)));
        assert!(!is_revoked_handshake(&http_error(
            StatusCode::INTERNAL_SERVER_ERROR
        )));
    }

    #[test]
    fn transient_connection_errors_are_not_revoked() {
        // A dropped socket must remain retryable (backoff), not terminal-revoked.
        assert!(!is_revoked_handshake(&Error::ConnectionClosed));
    }

    #[test]
    fn link_is_dead_after_timeout() {
        // ENG-1261: once no inbound frame has arrived for longer than the
        // liveness timeout, the link is treated as dead so the loop reconnects.
        assert!(link_is_dead(
            Duration::from_secs(31),
            Duration::from_secs(30)
        ));
    }

    #[test]
    fn link_is_alive_within_timeout() {
        assert!(!link_is_dead(
            Duration::from_secs(10),
            Duration::from_secs(30)
        ));
        // Exactly at the boundary is still alive (strictly-greater comparison).
        assert!(!link_is_dead(
            Duration::from_secs(30),
            Duration::from_secs(30)
        ));
    }

    #[test]
    fn liveness_constants_are_ordered_for_recovery() {
        // The Ping must fire before the liveness deadline so a healthy link
        // always refreshes `last_inbound` in time; the deadline must beat the
        // engine's 60s online-key TTL so a dead link reconnects (fresh
        // register()) before `is_online` goes stale and strands the Ask "+".
        assert!(PING_INTERVAL < LIVENESS_TIMEOUT);
        assert!(LIVENESS_TIMEOUT < Duration::from_secs(60));
    }
}
