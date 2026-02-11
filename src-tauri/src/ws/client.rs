use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use tauri::{AppHandle, Emitter};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::{
    client::IntoClientRequest,
    http::HeaderValue,
    Message,
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::state::{AppState, ConnectionStatus};
use crate::tools;
use crate::ws::protocol::{IncomingMessage, OutgoingMessage, ResponseStatus};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(45);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

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

            // Check if shutdown was requested
            if self.is_shutdown_requested().await {
                self.set_status(ConnectionStatus::Disconnected).await;
                return;
            }

            self.set_status(ConnectionStatus::Reconnecting).await;

            // Exponential backoff with jitter
            let base = Duration::from_secs(1 << attempt.min(4));
            let backoff = base.min(MAX_BACKOFF);
            let jitter_factor = rand::thread_rng().gen_range(0.8..1.2);
            let wait = backoff.mul_f64(jitter_factor);

            log::info!("Reconnecting in {:.1}s (attempt {attempt})", wait.as_secs_f64());

            // Request a fresh token from the frontend before reconnecting
            let _ = self.app.emit("token_refresh_needed", ());

            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = self.state.ws_shutdown.notified() => {
                    self.set_status(ConnectionStatus::Disconnected).await;
                    return;
                }
            }

            // Brief wait for token to arrive
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Connect, register, and run the message loop. Returns the close code if any.
    async fn connect_and_run(&self) -> Result<Option<u16>, Box<dyn std::error::Error + Send + Sync>> {
        let token = self.state.auth_token.read().await.clone();

        // Build request — use subprotocol auth in production, query params in dev
        let user_agent = format!("BeakrDesktop/{}", env!("CARGO_PKG_VERSION"));

        let (ws_stream, _response) = if let Some(token) = token {
            let mut request = self.ws_url.as_str().into_client_request()?;
            let subprotocol = format!("beakr-v1, bearer.{token}");
            request.headers_mut().insert(
                "Sec-WebSocket-Protocol",
                HeaderValue::from_str(&subprotocol)?,
            );
            request.headers_mut().insert(
                "User-Agent",
                HeaderValue::from_str(&user_agent)?,
            );
            tokio_tungstenite::connect_async(request).await?
        } else if cfg!(debug_assertions) {
            // Dev mode: use query params for auth (matches backend dev bypass)
            let dev_url = format!(
                "{}?identity_id=dev_local&email=dev@localhost&identity_name=dev&display_name=Dev+User",
                self.ws_url
            );
            let mut request = dev_url.as_str().into_client_request()?;
            request.headers_mut().insert(
                "User-Agent",
                HeaderValue::from_str(&user_agent)?,
            );
            tokio_tungstenite::connect_async(request).await?
        } else {
            return Err("No auth token available".into());
        };
        let (mut write, mut read) = ws_stream.split();

        // Send register message
        let device_name = self.state.device_name.read().await.clone();
        let scoped_folders = self.state.scoped_folders.read().await.clone();

        let register = OutgoingMessage::Register {
            device_name,
            platform: current_platform().to_string(),
            scoped_folders,
            platform_version: Some(os_version()),
            app_version: Some(env!("CARGO_PKG_VERSION").to_string()),
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

        let registered_text = registered_msg.to_text().map_err(|_| {
            format!("Expected text message but got: {:?}", registered_msg)
        })?;

        let incoming: IncomingMessage = serde_json::from_str(registered_text)
            .map_err(|e| format!(
                "Failed to parse registration response: {e} (raw: {registered_text:?})"
            ))?;

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
        write: &mut futures_util::stream::SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
        read: &mut futures_util::stream::SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    ) -> Option<u16> {
        let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
        heartbeat.tick().await; // consume the first immediate tick

        loop {
            tokio::select! {
                _ = heartbeat.tick() => {
                    let msg = serde_json::to_string(&OutgoingMessage::Heartbeat)
                        .unwrap_or_default();
                    if write.send(Message::Text(msg)).await.is_err() {
                        return None;
                    }
                }

                msg = read.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            self.handle_text_message(&text, write).await;
                        }
                        Some(Ok(Message::Close(frame))) => {
                            let code = frame.as_ref().map(|f| f.code.into());
                            log::info!("WebSocket closed with code: {code:?}");
                            return code;
                        }
                        Some(Err(e)) => {
                            log::error!("WebSocket read error: {e}");
                            return None;
                        }
                        None => {
                            log::info!("WebSocket stream ended");
                            return None;
                        }
                        _ => {}
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
                    let _ = write.send(Message::Close(None)).await;
                    return None;
                }
            }
        }
    }

    /// Handle an incoming text message (tool request).
    async fn handle_text_message(
        &self,
        text: &str,
        write: &mut futures_util::stream::SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    ) {
        let incoming: IncomingMessage = match serde_json::from_str(text) {
            Ok(m) => m,
            Err(e) => {
                log::warn!("Failed to parse incoming message: {e}");
                return;
            }
        };

        match incoming {
            IncomingMessage::Request { request_id, tool, params } => {
                let scoped_folders = self.state.scoped_folders.read().await.clone();

                // Notify frontend that a tool request started
                let _ = self.app.emit("tool:request_started", serde_json::json!({
                    "request_id": &request_id,
                    "tool": &tool,
                    "params": &params,
                }));

                let response = tools::dispatch_request(&tool, params, &scoped_folders).await;

                let (outgoing, result_status) = match response {
                    Ok((data, bytes)) => (OutgoingMessage::Response {
                        request_id: request_id.clone(),
                        status: ResponseStatus::Success,
                        data: Some(data),
                        error: None,
                        bytes_transferred: bytes,
                    }, "success"),
                    Err(e) => (OutgoingMessage::Response {
                        request_id: request_id.clone(),
                        status: ResponseStatus::Error,
                        data: None,
                        error: Some(e),
                        bytes_transferred: None,
                    }, "error"),
                };

                // Notify frontend that the request completed
                let _ = self.app.emit("tool:request_completed", serde_json::json!({
                    "request_id": &request_id,
                    "status": result_status,
                }));

                let json = match serde_json::to_string(&outgoing) {
                    Ok(j) => j,
                    Err(e) => {
                        log::error!("Failed to serialize response: {e}");
                        return;
                    }
                };

                if let Err(e) = write.send(Message::Text(json)).await {
                    log::error!("Failed to send response: {e}");
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

    async fn is_shutdown_requested(&self) -> bool {
        // Check if the notify has been triggered by trying with zero timeout
        tokio::select! {
            _ = self.state.ws_shutdown.notified() => true,
            _ = tokio::time::sleep(Duration::ZERO) => false,
        }
    }
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
