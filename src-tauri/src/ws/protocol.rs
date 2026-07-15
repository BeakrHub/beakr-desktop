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
    /// One incremental chunk of a long-running request (ENG-1527). Additive:
    /// a request that streams sends 0..n `ResponseChunk`s and ALWAYS finishes
    /// with the terminal `Response` — including when cancelled. `seq` is
    /// monotonically increasing per request so the engine can order/replay.
    ResponseChunk {
        request_id: String,
        seq: u64,
        data: serde_json::Value,
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
    /// Cancel an in-flight request (ENG-1527). The request still emits its
    /// terminal `Response` (as an error) so the engine side never hangs —
    /// LSP `$/cancelRequest` semantics. Cancelling an unknown/finished
    /// request_id is a no-op.
    Cancel {
        request_id: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_chunk_serializes_with_snake_case_tag_and_seq() {
        let msg = OutgoingMessage::ResponseChunk {
            request_id: "req-1".into(),
            seq: 7,
            data: serde_json::json!({"text": "hello"}),
        };
        let v: serde_json::Value = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["type"], "response_chunk");
        assert_eq!(v["request_id"], "req-1");
        assert_eq!(v["seq"], 7);
        assert_eq!(v["data"]["text"], "hello");
    }

    #[test]
    fn cancel_deserializes_from_engine_envelope() {
        let incoming: IncomingMessage =
            serde_json::from_str(r#"{"type":"cancel","request_id":"req-9"}"#).unwrap();
        assert!(matches!(
            incoming,
            IncomingMessage::Cancel { request_id } if request_id == "req-9"
        ));
    }

    #[test]
    fn terminal_response_shape_is_unchanged() {
        // Regression guard: the additive chunk variant must not alter the
        // existing terminal envelope the engine broker already parses.
        let msg = OutgoingMessage::Response {
            request_id: "req-1".into(),
            status: ResponseStatus::Success,
            data: Some(serde_json::json!({"ok": true})),
            error: None,
            bytes_transferred: None,
        };
        let v: serde_json::Value = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["type"], "response");
        assert_eq!(v["status"], "success");
        assert!(v.get("error").is_none());
        assert!(v.get("bytes_transferred").is_none());
    }
}
