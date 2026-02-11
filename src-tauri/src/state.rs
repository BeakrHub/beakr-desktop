use std::sync::Arc;
use tokio::sync::RwLock;

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
    /// Notifies the WS client when scoped_folders are changed via the UI.
    pub folders_changed: Arc<tokio::sync::Notify>,
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
            folders_changed: Arc::new(tokio::sync::Notify::new()),
        }
    }
}
