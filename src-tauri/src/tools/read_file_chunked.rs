//! `read_file_chunked` (ENG-1643): send a whole file to the engine in fixed
//! chunks instead of one WebSocket message.
//!
//! `read_file` returns a file as ONE base64 message, and the engine closes any
//! message over its 16 MiB limit, so a file over ~12 MB could never be ingested.
//! This tool streams the file over the existing streaming-request path
//! (`response_chunk` frames terminated by the normal `response`, ENG-1530):
//!
//! 1. `{"kind": "begin", "size", "chunk_bytes", "chunk_count"}` - the size the
//!    file had when opened, so the engine can refuse it before bytes move.
//! 2. `{"kind": "data", "index", "offset", "length", "sha256", "data"}` per
//!    chunk, `data` base64. `sha256` is the hex digest of that chunk's bytes.
//! 3. The terminal response `{"size", "sha256", "chunk_count"}`, `sha256` being
//!    the digest of the whole file, which the engine checks against the bytes
//!    it received.
//!
//! Flow control: the engine acknowledges chunks (`chunk_ack`, cumulative) as it
//! consumes them, and this reader never has more than `window` chunks
//! unacknowledged. That bounds what the desktop, the engine's relay and the
//! engine itself hold for one file to `window` chunks, however large the file.
//!
//! Access checks are exactly `read_file`'s ([`resolve_readable_file`]): scoped
//! folders after symlink resolution, the deny list, no directories.

use std::time::Duration;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt};

use super::read_file::resolve_readable_file;
use crate::file_index::FileIndex;
use crate::ws::inflight::{CancelSignal, ChunkAcks};
use crate::ws::ToolStream;

/// Tool name the engine requests.
pub const TOOL: &str = "read_file_chunked";
/// Capability advertised on register so the engine knows it may request [`TOOL`].
pub const CAPABILITY: &str = "chunked_read";

/// Raw bytes per chunk when the engine does not ask for a size.
pub const DEFAULT_CHUNK_BYTES: usize = 4 * 1024 * 1024;
const MIN_CHUNK_BYTES: usize = 64 * 1024;
/// Largest chunk accepted: its base64 frame (~10.7 MB) stays well under the
/// engine's 16 MiB WebSocket message limit.
const MAX_CHUNK_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_WINDOW: u64 = 4;
const MAX_WINDOW: u64 = 16;
/// How long to wait for the engine to acknowledge a chunk before giving up.
/// The engine acknowledges as soon as a chunk is stored, so silence this long
/// means it is gone (the request died with no cancel reaching us).
const ACK_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ChunkedReadOptions {
    pub chunk_bytes: usize,
    pub window: u64,
    pub max_bytes: Option<u64>,
    pub ack_timeout: Duration,
}

impl ChunkedReadOptions {
    /// Read the engine's requested chunk size, window and size ceiling,
    /// clamped to what this reader supports.
    pub fn from_params(params: &Value) -> Self {
        let chunk_bytes = params
            .get("chunk_bytes")
            .and_then(Value::as_u64)
            .map(|n| (n.min(MAX_CHUNK_BYTES as u64) as usize).max(MIN_CHUNK_BYTES))
            .unwrap_or(DEFAULT_CHUNK_BYTES);
        let window = params
            .get("window")
            .and_then(Value::as_u64)
            .map(|n| n.clamp(1, MAX_WINDOW))
            .unwrap_or(DEFAULT_WINDOW);
        let max_bytes = params.get("max_bytes").and_then(Value::as_u64);
        Self {
            chunk_bytes,
            window,
            max_bytes,
            ack_timeout: ACK_TIMEOUT,
        }
    }
}

/// What the whole transfer sent, returned to the engine as the terminal data.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TransferSummary {
    pub size: u64,
    pub sha256: String,
    pub chunk_count: u64,
}

/// Handle a `read_file_chunked` request.
///
/// Params: `path` (required), and optionally `chunk_bytes`, `window` and
/// `max_bytes` (refuse a file larger than this before reading it).
pub async fn handle_streaming(
    params: Value,
    scoped_folders: &[String],
    index: &FileIndex,
    stream: &ToolStream,
    mut acks: ChunkAcks,
    mut cancel: CancelSignal,
) -> Result<(Value, Option<u64>), String> {
    let path = params
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or("read_file_chunked requires 'path' parameter")?;
    let options = ChunkedReadOptions::from_params(&params);

    let (canonical, metadata) = resolve_readable_file(path, scoped_folders)?;
    let declared_size = metadata.len();
    if let Some(max_bytes) = options.max_bytes {
        if declared_size > max_bytes {
            return Err(format!(
                "File too large ({:.1} MB). The limit is {:.1} MB.",
                declared_size as f64 / 1024.0 / 1024.0,
                max_bytes as f64 / 1024.0 / 1024.0
            ));
        }
    }

    index.record_access(&canonical);
    let file = tokio::fs::File::open(&canonical)
        .await
        .map_err(|e| format!("Cannot open file: {e}"))?;

    let summary = stream_chunks(
        file,
        declared_size,
        &options,
        stream,
        &mut acks,
        &mut cancel,
    )
    .await?;
    let size = summary.size;
    Ok((
        json!({
            "size": summary.size,
            "sha256": summary.sha256,
            "chunk_count": summary.chunk_count,
        }),
        Some(size),
    ))
}

/// Send `reader` as a begin frame plus data chunks, honouring the ack window.
/// Separate from [`handle_streaming`] so chunking is testable on any reader.
pub(crate) async fn stream_chunks<R: AsyncRead + Unpin>(
    mut reader: R,
    declared_size: u64,
    options: &ChunkedReadOptions,
    stream: &ToolStream,
    acks: &mut ChunkAcks,
    cancel: &mut CancelSignal,
) -> Result<TransferSummary, String> {
    let chunk_bytes = options.chunk_bytes;
    let chunk_count = declared_size.div_ceil(chunk_bytes as u64);
    stream
        .try_chunk(json!({
            "kind": "begin",
            "size": declared_size,
            "chunk_bytes": chunk_bytes,
            "chunk_count": chunk_count,
        }))
        .await
        .map_err(|_| connection_closed())?;

    let mut whole_file = Sha256::new();
    let mut buffer = vec![0u8; chunk_bytes];
    let mut sent: u64 = 0;
    let mut index: u64 = 0;

    loop {
        let filled = tokio::select! {
            filled = fill(&mut reader, &mut buffer) => filled?,
            _ = cancel.cancelled() => return Err(cancelled()),
        };
        if filled == 0 {
            break;
        }
        if sent + filled as u64 > declared_size {
            return Err(file_changed());
        }

        wait_for_window(index, options, acks, cancel).await?;

        let bytes = &buffer[..filled];
        whole_file.update(bytes);
        stream
            .try_chunk(json!({
                "kind": "data",
                "index": index,
                "offset": sent,
                "length": filled,
                "sha256": hex_digest(&Sha256::digest(bytes)),
                "data": BASE64.encode(bytes),
            }))
            .await
            .map_err(|_| connection_closed())?;

        sent += filled as u64;
        index += 1;
        if filled < chunk_bytes {
            // `fill` only returns a short chunk at end of file.
            break;
        }
    }

    if sent != declared_size {
        return Err(file_changed());
    }
    Ok(TransferSummary {
        size: sent,
        sha256: hex_digest(&whole_file.finalize()),
        chunk_count: index,
    })
}

/// Block until sending chunk `index` keeps at most `window` chunks
/// unacknowledged.
async fn wait_for_window(
    index: u64,
    options: &ChunkedReadOptions,
    acks: &mut ChunkAcks,
    cancel: &mut CancelSignal,
) -> Result<(), String> {
    if index < options.window {
        return Ok(());
    }
    let needed = index + 1 - options.window;
    if acks.acknowledged() >= needed {
        return Ok(());
    }
    let acknowledged = tokio::select! {
        waited = tokio::time::timeout(options.ack_timeout, acks.wait_for(needed)) => waited,
        _ = cancel.cancelled() => return Err(cancelled()),
    };
    match acknowledged {
        Ok(true) => Ok(()),
        Ok(false) => Err("The transfer was stopped before it finished.".to_string()),
        Err(_) => Err(format!(
            "Beakr stopped acknowledging the file transfer after chunk {}.",
            needed.saturating_sub(1)
        )),
    }
}

/// Read until `buffer` is full or the reader is at end of file. Returns the
/// number of bytes read; anything short of `buffer.len()` means end of file.
async fn fill<R: AsyncRead + Unpin>(reader: &mut R, buffer: &mut [u8]) -> Result<usize, String> {
    let mut filled = 0;
    while filled < buffer.len() {
        let read = reader
            .read(&mut buffer[filled..])
            .await
            .map_err(|e| format!("Read error: {e}"))?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    Ok(filled)
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn file_changed() -> String {
    "The file changed while it was being read. Try again once it is saved.".to_string()
}

fn cancelled() -> String {
    "cancelled by server".to_string()
}

fn connection_closed() -> String {
    "The connection to Beakr closed during the transfer.".to_string()
}

#[cfg(test)]
#[path = "read_file_chunked_tests.rs"]
mod tests;
