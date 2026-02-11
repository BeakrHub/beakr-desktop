use serde::{Deserialize, Serialize};

/// Messages sent FROM the desktop client TO the server.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutgoingMessage {
    Register {
        device_name: String,
        platform: String,
        scoped_folders: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        platform_version: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        app_version: Option<String>,
    },
    Heartbeat,
    Response {
        request_id: String,
        status: ResponseStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        bytes_transferred: Option<u64>,
    },
    UpdateFolders {
        scoped_folders: Vec<String>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseStatus {
    Success,
    Error,
}

/// Messages sent FROM the server TO the desktop client.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IncomingMessage {
    Registered {
        device_id: String,
    },
    Request {
        request_id: String,
        tool: String,
        params: serde_json::Value,
    },
}
