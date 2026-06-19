use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::RwLock;

/// A running per-provider session-capture localhost bridge.
///
/// One instance exists per open provider window. The `port` is substituted into
/// the provider's gather script; triggering `shutdown` tears down only this
/// provider's listener, leaving other providers' bridges untouched.
#[derive(Clone)]
pub struct SessionBridge {
    /// Port of this provider's running localhost data bridge.
    pub port: u16,
    /// Notifies this provider's bridge listener to shut down.
    pub shutdown: Arc<tokio::sync::Notify>,
}

/// The captured Benchling browser session used by the live agent tools.
///
/// When the user Connects Benchling and logs in, we capture the HttpOnly
/// `session` cookie from the benchling.com webview's cookie store (see
/// `session::commands::connect_session`). The live `benchling_*` tools then call
/// Benchling's internal `/1/api/*` directly from Rust using `http_client()` with
/// this cookie, so they work even after the connect window has been closed.
#[derive(Clone, Debug)]
pub struct BenchlingSession {
    /// Value of the benchling.com `session` cookie (HttpOnly).
    pub session_cookie: String,
    /// The Benchling tenant host (always `benchling.com` for the free tier).
    pub tenant_host: String,
    /// The logged-in user's handle, captured from `GET /1/api/users/me`.
    pub user_handle: String,
}

/// Connection status for the WebSocket client.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionStatus {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting,
    Revoked,
}

impl std::fmt::Display for ConnectionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disconnected => write!(f, "Disconnected"),
            Self::Connecting => write!(f, "Connecting…"),
            Self::Connected => write!(f, "Connected"),
            Self::Reconnecting => write!(f, "Reconnecting…"),
            Self::Revoked => write!(f, "Device Revoked"),
        }
    }
}

/// Shared application state, accessible from commands and the WS client.
#[derive(Clone)]
pub struct AppState {
    pub auth_token: Arc<RwLock<Option<String>>>,
    pub ws_status: Arc<RwLock<ConnectionStatus>>,
    pub scoped_folders: Arc<RwLock<Vec<String>>>,
    pub device_name: Arc<RwLock<String>>,
    pub device_id: Arc<RwLock<Option<String>>>,
    pub ws_shutdown: Arc<tokio::sync::Notify>,
    /// Set to true when the user explicitly disconnects, so the WS run loop stops
    /// reconnecting. A lone `ws_shutdown` notify permit can be consumed by the
    /// message loop before the run loop checks it, so this flag — not the notify —
    /// is the durable source of truth for "the user asked to stay disconnected."
    /// Cleared on the next connect. Distinguishes a deliberate disconnect (stay
    /// down) from a dropped socket (auto-reconnect).
    pub shutdown_requested: Arc<AtomicBool>,
    /// Notifies the WS client when scoped_folders are changed via the UI.
    pub folders_changed: Arc<tokio::sync::Notify>,
    /// Running session-capture localhost bridges, keyed by provider key
    /// (currently only "benchling"). One entry per open provider window.
    pub session_bridges: Arc<RwLock<HashMap<String, SessionBridge>>>,
    /// The captured Benchling browser session, set once the user connects and
    /// logs in. `None` until a successful connect; the live `benchling_*` tools
    /// return a reconnect error while it is `None` or after the session expires.
    pub benchling_session: Arc<RwLock<Option<BenchlingSession>>>,
}

impl AppState {
    pub fn new() -> Self {
        let device_name = hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_else(|| "My Computer".to_string());

        Self {
            auth_token: Arc::new(RwLock::new(None)),
            ws_status: Arc::new(RwLock::new(ConnectionStatus::Disconnected)),
            scoped_folders: Arc::new(RwLock::new(Vec::new())),
            device_name: Arc::new(RwLock::new(device_name)),
            device_id: Arc::new(RwLock::new(None)),
            ws_shutdown: Arc::new(tokio::sync::Notify::new()),
            shutdown_requested: Arc::new(AtomicBool::new(false)),
            folders_changed: Arc::new(tokio::sync::Notify::new()),
            session_bridges: Arc::new(RwLock::new(HashMap::new())),
            benchling_session: Arc::new(RwLock::new(None)),
        }
    }
}
