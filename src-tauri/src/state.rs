use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::RwLock;

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
    /// Notifies the WS client when scoped_folders are changed via the UI, so
    /// it pushes the new list to the backend. The WS client must be the only
    /// waiter on this channel — see `notify_folders_changed`.
    pub folders_changed: Arc<tokio::sync::Notify>,
    /// The same scoped-folders-changed signal for the filesystem watcher
    /// (file_watch.rs), which must rebind to the new roots. A separate channel
    /// because `Notify::notify_one` wakes a single waiter: with the watcher and
    /// the WS client parked on one Notify, a folder change woke only one of
    /// them and the other silently missed it — the backend kept a stale
    /// scoped_folders list (ENG-1624).
    pub watch_folders_changed: Arc<tokio::sync::Notify>,
    /// In-memory metadata cache over the scoped folders, so repeat filename
    /// searches answer without re-walking disk (ENG-1150).
    pub file_index: Arc<crate::file_index::FileIndex>,
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
            watch_folders_changed: Arc::new(tokio::sync::Notify::new()),
            file_index: Arc::new(crate::file_index::FileIndex::new()),
            benchling_session: Arc::new(RwLock::new(None)),
        }
    }

    /// Signal every consumer of a scoped-folders change: the WS client (which
    /// pushes the new list to the backend) and the filesystem watcher (which
    /// rebinds to the new roots). Always use this instead of notifying one
    /// channel directly, so a new consumer can't be starved by an existing one.
    pub fn notify_folders_changed(&self) {
        self.folders_changed.notify_one();
        self.watch_folders_changed.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Regression (ENG-1624): one folder-change signal must reach BOTH the WS
    /// client and the filesystem watcher. With a single shared Notify and
    /// notify_one(), the two waiters raced and the loser silently missed the
    /// change, leaving the backend with a stale scoped_folders list.
    #[tokio::test]
    async fn folder_change_signals_both_ws_and_watcher() {
        let state = AppState::new();
        state.notify_folders_changed();

        tokio::time::timeout(Duration::from_secs(1), state.folders_changed.notified())
            .await
            .expect("WS channel missed the folder change");
        tokio::time::timeout(Duration::from_secs(1), state.watch_folders_changed.notified())
            .await
            .expect("watcher channel missed the folder change");
    }
}
