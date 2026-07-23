mod client;
pub mod inflight;
pub mod protocol;

pub use client::os_version;
pub use client::WsClient;

/// Handle a streaming tool holds to emit `response_chunk` frames (ENG-1528).
/// Seq numbers are per-request and monotonic; the terminal `response` is sent
/// by the request task in `client.rs`, never through this.
pub struct ToolStream {
    request_id: String,
    seq: std::sync::atomic::AtomicU64,
    out_tx: tokio::sync::mpsc::Sender<protocol::OutgoingMessage>,
}

impl ToolStream {
    pub fn new(
        request_id: String,
        out_tx: tokio::sync::mpsc::Sender<protocol::OutgoingMessage>,
    ) -> Self {
        Self {
            request_id,
            seq: std::sync::atomic::AtomicU64::new(0),
            out_tx,
        }
    }

    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// Send one chunk. A send failure (connection died) is logged, not fatal:
    /// the run continues and its terminal response reports the outcome.
    pub async fn chunk(&self, data: serde_json::Value) {
        let seq = self.seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let msg = protocol::OutgoingMessage::ResponseChunk {
            request_id: self.request_id.clone(),
            seq,
            data,
        };
        if self.out_tx.send(msg).await.is_err() {
            log::debug!("chunk dropped: connection closed (req {})", self.request_id);
        }
    }
}
