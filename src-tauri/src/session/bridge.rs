//! Localhost data bridge between an injected provider gather script and the Rust
//! app.
//!
//! Why a localhost HTTP listener (and not Tauri IPC):
//! The gather script runs in the page context of a REMOTE provider origin (e.g.
//! benchling.com, mynotebook.labarchives.com). Exposing Tauri's `invoke` IPC to a
//! remote origin is a security hazard and is awkward to scope safely in Tauri v2.
//! Instead we run a tiny HTTP/1.1 listener bound to 127.0.0.1 on an ephemeral port
//! and respond with `Access-Control-Allow-Origin: *`, so a page-context `fetch`
//! from any origin can POST to it. Rust HTTP/TCP is not gated by Tauri
//! capabilities, so no extra permission is required for the listener itself.
//!
//! We deliberately use a hand-rolled HTTP/1.1 read over a tokio `TcpListener`
//! rather than pulling in a new crate (e.g. tiny_http): tokio is already a
//! full-feature dependency, the protocol surface we need is trivial (one POST +
//! CORS preflight), and it integrates directly with the existing async runtime.
//!
//! The bridge is provider-agnostic: gather scripts POST to `/session/ingest` and
//! the listener forwards whatever JSON arrives. One bridge instance runs per open
//! provider window (port + shutdown tracked per provider in `AppState`).

use std::sync::Arc;

use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

/// A message handed from the bridge listener to the import driver.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BridgeMessage {
    /// Incremental progress from the gather script.
    Progress {
        #[serde(default)]
        stage: String,
        #[serde(default)]
        message: String,
        #[serde(default)]
        current: Option<u64>,
        #[serde(default)]
        total: Option<u64>,
    },
    /// The provider session is idle/expired — the user must log in again.
    NeedsLogin {
        #[serde(default)]
        message: String,
    },
    /// Terminal success: the full item list was gathered.
    Complete {
        #[serde(default)]
        user_handle: String,
        #[serde(default)]
        tenant_host: String,
        #[serde(default)]
        items: Vec<serde_json::Value>,
    },
    /// Terminal failure inside the gather script.
    Error {
        #[serde(default)]
        message: String,
    },
}

/// Spawns the localhost bridge listener on an ephemeral port.
///
/// Returns the bound port (to substitute into the gather script) and a receiver
/// that yields parsed [`BridgeMessage`]s. The listener runs until the returned
/// `shutdown` notify is triggered or the receiver is dropped.
pub async fn start_bridge(
    shutdown: Arc<tokio::sync::Notify>,
) -> Result<(u16, mpsc::Receiver<BridgeMessage>), String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("Failed to bind bridge listener: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("Failed to read bridge address: {e}"))?
        .port();

    let (tx, rx) = mpsc::channel::<BridgeMessage>(64);

    tauri::async_runtime::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.notified() => {
                    log::info!("Session bridge listener shutting down");
                    break;
                }
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, _addr)) => {
                            let tx = tx.clone();
                            tauri::async_runtime::spawn(async move {
                                if let Err(e) = handle_conn(stream, tx).await {
                                    log::warn!("Session bridge connection error: {e}");
                                }
                            });
                        }
                        Err(e) => {
                            log::warn!("Session bridge accept error: {e}");
                            break;
                        }
                    }
                }
            }
        }
    });

    Ok((port, rx))
}

/// Reads one HTTP/1.1 request, parses the JSON body (for POST), forwards the
/// parsed message on `tx`, and writes a minimal CORS-enabled HTTP response.
async fn handle_conn(mut stream: TcpStream, tx: mpsc::Sender<BridgeMessage>) -> Result<(), String> {
    // Read until we have headers; then read the remaining Content-Length bytes.
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut tmp = [0u8; 4096];

    // Read headers.
    let header_end = loop {
        if let Some(pos) = find_header_end(&buf) {
            break pos;
        }
        let n = stream
            .read(&mut tmp)
            .await
            .map_err(|e| format!("read failed: {e}"))?;
        if n == 0 {
            return Err("connection closed before headers".to_string());
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 64 * 1024 * 1024 {
            return Err("request header too large".to_string());
        }
    };

    let header_text = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let method = request_line.split_whitespace().next().unwrap_or("");

    // CORS preflight — answer immediately.
    if method.eq_ignore_ascii_case("OPTIONS") {
        write_response(&mut stream, 204, "").await?;
        return Ok(());
    }

    if !method.eq_ignore_ascii_case("POST") {
        write_response(&mut stream, 405, "{\"error\":\"method not allowed\"}").await?;
        return Ok(());
    }

    // Find Content-Length.
    let mut content_length: usize = 0;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }

    // Body bytes already buffered after the header terminator.
    let body_start = header_end + 4; // skip the trailing \r\n\r\n
    let mut body: Vec<u8> = if body_start <= buf.len() {
        buf[body_start..].to_vec()
    } else {
        Vec::new()
    };

    // Read any remaining body bytes.
    while body.len() < content_length {
        let n = stream
            .read(&mut tmp)
            .await
            .map_err(|e| format!("read body failed: {e}"))?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
        if body.len() > 256 * 1024 * 1024 {
            return Err("request body too large".to_string());
        }
    }

    match serde_json::from_slice::<BridgeMessage>(&body) {
        Ok(msg) => {
            let _ = tx.send(msg).await;
            write_response(&mut stream, 200, "{\"ok\":true}").await?;
        }
        Err(e) => {
            log::warn!("Session bridge: bad JSON body: {e}");
            write_response(&mut stream, 400, "{\"error\":\"bad json\"}").await?;
        }
    }

    Ok(())
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Writes a minimal HTTP/1.1 response with permissive CORS headers.
async fn write_response(stream: &mut TcpStream, status: u16, body: &str) -> Result<(), String> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        405 => "Method Not Allowed",
        _ => "OK",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Methods: POST, OPTIONS\r\n\
         Access-Control-Allow-Headers: Content-Type\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        len = body.as_bytes().len(),
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|e| format!("write failed: {e}"))?;
    let _ = stream.flush().await;
    Ok(())
}

/// Emits a frontend `session:progress` event for a [`BridgeMessage::Progress`].
///
/// Every emitted payload carries the `provider` key so a `SessionConnect`
/// component can filter to its own provider's events.
pub fn emit_progress(
    app: &AppHandle,
    provider: &str,
    stage: &str,
    message: &str,
    current: Option<u64>,
    total: Option<u64>,
) {
    let _ = app.emit(
        "session:progress",
        serde_json::json!({
            "provider": provider,
            "stage": stage,
            "message": message,
            "current": current,
            "total": total,
        }),
    );
}

/// Emits the terminal `session:done` event. `payload` must already be an object;
/// the `provider` key is merged in.
pub fn emit_done(app: &AppHandle, provider: &str, mut payload: serde_json::Value) {
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("provider".to_string(), serde_json::json!(provider));
    }
    let _ = app.emit("session:done", payload);
}

/// Emits the `session:needs_login` event (idle/expired provider session).
pub fn emit_needs_login(app: &AppHandle, provider: &str, message: &str) {
    let _ = app.emit(
        "session:needs_login",
        serde_json::json!({ "provider": provider, "message": message }),
    );
}

/// Emits a terminal `session:error` event.
pub fn emit_error(app: &AppHandle, provider: &str, message: &str) {
    let _ = app.emit(
        "session:error",
        serde_json::json!({ "provider": provider, "message": message }),
    );
}
