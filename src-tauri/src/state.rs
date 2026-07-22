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

/// Lifecycle of the one active coding run, as shown to the user (ENG-1552).
///
/// `Stopping` is the truth-telling state: a cancel has been signalled (tray,
/// app window, or engine WS) and the child has been SIGINTed, but the process
/// is NOT yet confirmed dead. The UI must keep saying "Stopping…" until the
/// run is reaped and cleared — never claim a run is gone while it may still
/// be editing files.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodingRunStatus {
    Running,
    Stopping,
}

/// The active coding run, if any — what the tray and app window render.
/// Serialized as the `coding_run:changed` event payload and the
/// `get_active_coding_run` response, so field names are a frontend contract.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ActiveCodingRun {
    pub request_id: String,
    /// Canonicalized cwd of the run. The UI shows the basename, full path on
    /// hover — same convention as the web card.
    pub working_dir: String,
    pub cli: String,
    /// Unix epoch millis when the child spawned; the frontend derives elapsed
    /// from this instead of the backend streaming ticks.
    pub started_at_ms: u64,
    pub status: CodingRunStatus,
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
    /// In-flight request tracking: cancellation signals + the one-coding-run
    /// cap (ENG-1527). Shared state because cancels arrive from the engine
    /// (WS `cancel`) and, later, the local UI (tray "Stop run").
    pub inflight: Arc<crate::ws::inflight::InflightRegistry>,
    /// Live child process groups, reaped on quit (ENG-1527).
    pub processes: Arc<crate::process_group::ProcessRegistry>,
    /// The coding run in progress, if any (ENG-1528; enriched for ENG-1552
    /// run visibility). std RwLock on purpose: the tray "Stop run" handler is
    /// synchronous.
    pub active_coding_run: Arc<std::sync::RwLock<Option<ActiveCodingRun>>>,
    /// Sender into the live WS connection's outbound queue, set while
    /// connected (ENG-1536). Lets settings changes push a readiness_update
    /// without waiting for a reconnect. None when disconnected — pushes are
    /// simply skipped; the next register carries fresh state anyway.
    pub ws_outbound: Arc<
        std::sync::RwLock<Option<tokio::sync::mpsc::Sender<crate::ws::protocol::OutgoingMessage>>>,
    >,
}

/// Signal cancellation of the active coding run, from the tray or the app
/// window. Returns the request_id it cancelled, or None if no run was active.
/// The run's own loop observes the signal, SIGINTs the child, and moves the
/// UI state to Stopping — this function only fires the signal.
pub fn stop_active_coding_run(state: &AppState) -> Option<String> {
    let active = state
        .active_coding_run
        .read()
        .expect("active run lock poisoned")
        .as_ref()
        .map(|run| run.request_id.clone());
    if let Some(request_id) = &active {
        log::info!("Local stop: cancelling coding run {request_id}");
        state.inflight.cancel(request_id);
    }
    active
}

#[cfg(test)]
mod coding_run_tests {
    use super::*;

    // The serialized shape is the frontend contract for the
    // `coding_run:changed` event and `get_active_coding_run` — pin it.
    #[test]
    fn active_run_serializes_with_snake_case_status() {
        let run = ActiveCodingRun {
            request_id: "req-1".into(),
            working_dir: "/Users/d/repo".into(),
            cli: "claude".into(),
            started_at_ms: 1_700_000_000_000,
            status: CodingRunStatus::Stopping,
        };
        let v = serde_json::to_value(&run).unwrap();
        assert_eq!(v["status"], "stopping");
        assert_eq!(v["working_dir"], "/Users/d/repo");
        assert_eq!(v["started_at_ms"], 1_700_000_000_000u64);
    }

    #[test]
    fn stop_with_no_active_run_is_a_noop() {
        let state = AppState::new();
        assert_eq!(stop_active_coding_run(&state), None);
    }

    #[test]
    fn stop_cancels_the_active_run_by_request_id() {
        let state = AppState::new();
        let signal = state.inflight.register("req-9");
        *state.active_coding_run.write().unwrap() = Some(ActiveCodingRun {
            request_id: "req-9".into(),
            working_dir: "/tmp".into(),
            cli: "claude".into(),
            started_at_ms: 0,
            status: CodingRunStatus::Running,
        });

        assert_eq!(stop_active_coding_run(&state), Some("req-9".into()));
        // The registered cancel signal must have fired.
        assert!(signal.is_cancelled());
    }
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
            inflight: Arc::new(crate::ws::inflight::InflightRegistry::new()),
            processes: Arc::new(crate::process_group::ProcessRegistry::new()),
            active_coding_run: Arc::new(std::sync::RwLock::new(None)),
            ws_outbound: Arc::new(std::sync::RwLock::new(None)),
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
