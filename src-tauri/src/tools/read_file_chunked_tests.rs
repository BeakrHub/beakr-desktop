//! Tests for `read_file_chunked` (ENG-1643): exact chunk boundaries, digests,
//! the ack window, and that its access checks are `read_file`'s.

use std::sync::Arc;
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use super::*;
use crate::file_index::FileIndex;
use crate::ws::inflight::InflightRegistry;
use crate::ws::protocol::OutgoingMessage;
use crate::ws::ToolStream;

const REQUEST_ID: &str = "req-chunked";
const SMALL_CHUNK: usize = 64 * 1024;

fn options(chunk_bytes: usize, window: u64) -> ChunkedReadOptions {
    ChunkedReadOptions {
        chunk_bytes,
        window,
        max_bytes: None,
        ack_timeout: Duration::from_secs(5),
    }
}

fn sha_hex(bytes: &[u8]) -> String {
    hex_digest(&Sha256::digest(bytes))
}

/// Deterministic, non-repeating-per-chunk test bytes.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 % 251) as u8).collect()
}

/// Play the engine: receive every chunk, acknowledge each data chunk at once,
/// and return the chunk payloads in order.
fn spawn_acking_engine(
    registry: Arc<InflightRegistry>,
    mut rx: mpsc::Receiver<OutgoingMessage>,
) -> tokio::task::JoinHandle<Vec<Value>> {
    tokio::spawn(async move {
        let mut frames = Vec::new();
        while let Some(message) = rx.recv().await {
            if let OutgoingMessage::ResponseChunk { data, .. } = message {
                if data["kind"] == "data" {
                    registry.ack_chunk(REQUEST_ID, data["index"].as_u64().unwrap());
                }
                frames.push(data);
            }
        }
        frames
    })
}

/// Stream `content` through `stream_chunks` with an engine that acks
/// everything; returns the result and the frames the engine saw.
async fn transfer(
    content: &[u8],
    declared_size: u64,
    opts: ChunkedReadOptions,
) -> (Result<TransferSummary, String>, Vec<Value>) {
    let registry = Arc::new(InflightRegistry::new());
    let mut cancel = registry.register(REQUEST_ID);
    let mut acks = registry.register_chunk_acks(REQUEST_ID);
    let (tx, rx) = mpsc::channel(64);
    let engine = spawn_acking_engine(registry.clone(), rx);
    let stream = ToolStream::new(REQUEST_ID.to_string(), tx);
    let result = stream_chunks(
        content,
        declared_size,
        &opts,
        &stream,
        &mut acks,
        &mut cancel,
    )
    .await;
    drop(stream);
    (result, engine.await.unwrap())
}

fn data_frames(frames: &[Value]) -> Vec<&Value> {
    frames.iter().filter(|f| f["kind"] == "data").collect()
}

fn reassemble(frames: &[Value]) -> Vec<u8> {
    data_frames(frames)
        .iter()
        .flat_map(|f| BASE64.decode(f["data"].as_str().unwrap()).unwrap())
        .collect()
}

#[tokio::test]
async fn empty_file_sends_begin_and_no_data_chunks() {
    let (result, frames) = transfer(b"", 0, options(SMALL_CHUNK, 4)).await;
    let summary = result.unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(
        frames[0],
        json!({"kind": "begin", "size": 0, "chunk_bytes": SMALL_CHUNK, "chunk_count": 0})
    );
    assert_eq!(summary.size, 0);
    assert_eq!(summary.chunk_count, 0);
    assert_eq!(summary.sha256, sha_hex(b""));
}

#[tokio::test]
async fn one_byte_file_is_one_short_chunk() {
    let (result, frames) = transfer(b"x", 1, options(SMALL_CHUNK, 4)).await;
    let summary = result.unwrap();
    let data = data_frames(&frames);
    assert_eq!(data.len(), 1);
    assert_eq!(data[0]["length"], 1);
    assert_eq!(data[0]["offset"], 0);
    assert_eq!(summary.chunk_count, 1);
    assert_eq!(summary.sha256, sha_hex(b"x"));
}

#[tokio::test]
async fn file_of_exactly_n_chunks_has_n_full_chunks() {
    let content = pattern(3 * SMALL_CHUNK);
    let (result, frames) = transfer(&content, content.len() as u64, options(SMALL_CHUNK, 4)).await;
    let summary = result.unwrap();
    let data = data_frames(&frames);
    assert_eq!(frames[0]["chunk_count"], 3);
    assert_eq!(data.len(), 3);
    for (i, frame) in data.iter().enumerate() {
        assert_eq!(frame["index"], i as u64);
        assert_eq!(frame["offset"], (i * SMALL_CHUNK) as u64);
        assert_eq!(frame["length"], SMALL_CHUNK as u64);
    }
    assert_eq!(summary.chunk_count, 3);
    assert_eq!(reassemble(&frames), content);
}

#[tokio::test]
async fn one_byte_over_n_chunks_adds_a_one_byte_chunk() {
    let content = pattern(3 * SMALL_CHUNK + 1);
    let (result, frames) = transfer(&content, content.len() as u64, options(SMALL_CHUNK, 4)).await;
    let summary = result.unwrap();
    let data = data_frames(&frames);
    assert_eq!(frames[0]["chunk_count"], 4);
    assert_eq!(data.len(), 4);
    assert_eq!(data[3]["length"], 1);
    assert_eq!(data[3]["offset"], (3 * SMALL_CHUNK) as u64);
    assert_eq!(summary.size, content.len() as u64);
    assert_eq!(reassemble(&frames), content);
}

#[tokio::test]
async fn chunk_and_whole_file_digests_match_the_bytes() {
    let content = pattern(2 * SMALL_CHUNK + 777);
    let (result, frames) = transfer(&content, content.len() as u64, options(SMALL_CHUNK, 2)).await;
    let summary = result.unwrap();
    for frame in data_frames(&frames) {
        let bytes = BASE64.decode(frame["data"].as_str().unwrap()).unwrap();
        assert_eq!(frame["sha256"], sha_hex(&bytes));
        assert_eq!(frame["length"], bytes.len() as u64);
    }
    assert_eq!(summary.sha256, sha_hex(&content));
}

#[tokio::test]
async fn a_file_that_grew_after_open_is_refused() {
    let content = pattern(SMALL_CHUNK + 10);
    let (result, _) = transfer(&content, SMALL_CHUNK as u64, options(SMALL_CHUNK, 4)).await;
    assert!(result
        .unwrap_err()
        .contains("changed while it was being read"));
}

#[tokio::test]
async fn a_file_that_shrank_after_open_is_refused() {
    let content = pattern(10);
    let (result, _) = transfer(&content, 20, options(SMALL_CHUNK, 4)).await;
    assert!(result
        .unwrap_err()
        .contains("changed while it was being read"));
}

#[tokio::test]
async fn reader_never_exceeds_the_ack_window() {
    let content = pattern(5 * SMALL_CHUNK);
    let registry = Arc::new(InflightRegistry::new());
    let mut cancel = registry.register(REQUEST_ID);
    let mut acks = registry.register_chunk_acks(REQUEST_ID);
    let (tx, mut rx) = mpsc::channel(64);
    let stream = ToolStream::new(REQUEST_ID.to_string(), tx);
    let opts = options(SMALL_CHUNK, 2);
    let size = content.len() as u64;
    let reader = tokio::spawn(async move {
        stream_chunks(&content[..], size, &opts, &stream, &mut acks, &mut cancel).await
    });

    // begin + window (2) data chunks, then the reader must stall.
    let mut seen = 0;
    while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
        seen += 1;
    }
    assert_eq!(seen, 3, "begin + 2 unacknowledged chunks, then wait");

    registry.ack_chunk(REQUEST_ID, 0);
    let next = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await;
    assert!(next.is_ok(), "one ack frees one slot in the window");
    assert!(
        tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .is_err(),
        "and only one"
    );

    registry.ack_chunk(REQUEST_ID, 4);
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        while rx.recv().await.is_some() {}
        reader.await.unwrap()
    })
    .await
    .unwrap();
    assert_eq!(result.unwrap().chunk_count, 5);
}

#[tokio::test]
async fn silence_from_the_engine_times_out() {
    let content = pattern(3 * SMALL_CHUNK);
    let registry = Arc::new(InflightRegistry::new());
    let mut cancel = registry.register(REQUEST_ID);
    let mut acks = registry.register_chunk_acks(REQUEST_ID);
    let (tx, mut rx) = mpsc::channel(64);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let stream = ToolStream::new(REQUEST_ID.to_string(), tx);
    let mut opts = options(SMALL_CHUNK, 1);
    opts.ack_timeout = Duration::from_millis(100);
    let result = stream_chunks(
        &content[..],
        content.len() as u64,
        &opts,
        &stream,
        &mut acks,
        &mut cancel,
    )
    .await;
    assert!(result.unwrap_err().contains("stopped acknowledging"));
}

#[tokio::test]
async fn cancel_stops_a_reader_waiting_for_acks() {
    let content = pattern(3 * SMALL_CHUNK);
    let registry = Arc::new(InflightRegistry::new());
    let mut cancel = registry.register(REQUEST_ID);
    let mut acks = registry.register_chunk_acks(REQUEST_ID);
    let (tx, mut rx) = mpsc::channel(64);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let stream = ToolStream::new(REQUEST_ID.to_string(), tx);
    let opts = options(SMALL_CHUNK, 1);
    let size = content.len() as u64;
    let reader = tokio::spawn(async move {
        stream_chunks(&content[..], size, &opts, &stream, &mut acks, &mut cancel).await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(registry.cancel(REQUEST_ID));
    let result = tokio::time::timeout(Duration::from_secs(1), reader)
        .await
        .expect("cancel must stop the reader promptly")
        .unwrap();
    assert_eq!(result.unwrap_err(), "cancelled by server");
}

#[tokio::test]
async fn a_closed_connection_stops_the_transfer() {
    let content = pattern(3 * SMALL_CHUNK);
    let registry = Arc::new(InflightRegistry::new());
    let mut cancel = registry.register(REQUEST_ID);
    let mut acks = registry.register_chunk_acks(REQUEST_ID);
    let (tx, rx) = mpsc::channel(64);
    drop(rx);
    let stream = ToolStream::new(REQUEST_ID.to_string(), tx);
    let result = stream_chunks(
        &content[..],
        content.len() as u64,
        &options(SMALL_CHUNK, 4),
        &stream,
        &mut acks,
        &mut cancel,
    )
    .await;
    assert!(result.unwrap_err().contains("connection to Beakr closed"));
}

#[test]
fn options_are_clamped_to_supported_bounds() {
    let defaults = ChunkedReadOptions::from_params(&json!({"path": "/x"}));
    assert_eq!(defaults.chunk_bytes, DEFAULT_CHUNK_BYTES);
    assert_eq!(defaults.window, DEFAULT_WINDOW);
    assert_eq!(defaults.max_bytes, None);

    let huge = ChunkedReadOptions::from_params(
        &json!({"chunk_bytes": 1u64 << 40, "window": 1000, "max_bytes": 52_428_800u64}),
    );
    assert_eq!(huge.chunk_bytes, MAX_CHUNK_BYTES);
    assert_eq!(huge.window, MAX_WINDOW);
    assert_eq!(huge.max_bytes, Some(52_428_800));

    let tiny = ChunkedReadOptions::from_params(&json!({"chunk_bytes": 1, "window": 0}));
    assert_eq!(tiny.chunk_bytes, MIN_CHUNK_BYTES);
    assert_eq!(tiny.window, 1);
}

#[test]
fn max_chunk_frame_fits_the_engine_message_limit() {
    // base64 is 4/3 of the raw bytes; the frame must stay under the engine's
    // 16 MiB WebSocket message limit with room for the JSON envelope.
    let encoded = MAX_CHUNK_BYTES.div_ceil(3) * 4;
    assert!(encoded + 64 * 1024 < 16 * 1024 * 1024);
}

// ----------------------------------------------------- access checks (handle_streaming)

/// Run the full handler against a real path, with an engine that acks.
async fn handle(
    path: &std::path::Path,
    scope: &std::path::Path,
    extra: Value,
) -> Result<Value, String> {
    let registry = Arc::new(InflightRegistry::new());
    let cancel = registry.register(REQUEST_ID);
    let acks = registry.register_chunk_acks(REQUEST_ID);
    let (tx, rx) = mpsc::channel(64);
    let engine = spawn_acking_engine(registry.clone(), rx);
    let stream = ToolStream::new(REQUEST_ID.to_string(), tx);
    let mut params = json!({"path": path.to_str().unwrap(), "chunk_bytes": SMALL_CHUNK});
    for (key, value) in extra.as_object().unwrap() {
        params[key] = value.clone();
    }
    let scoped = vec![scope.to_str().unwrap().to_string()];
    let result = handle_streaming(params, &scoped, &FileIndex::new(), &stream, acks, cancel).await;
    drop(stream);
    engine.await.unwrap();
    result.map(|(data, _)| data)
}

#[tokio::test]
async fn reads_a_file_inside_a_scoped_folder() {
    let dir = tempfile::tempdir().unwrap();
    let content = pattern(SMALL_CHUNK * 2 + 5);
    let file = dir.path().join("report.pdf");
    std::fs::write(&file, &content).unwrap();
    let data = handle(&file, dir.path(), json!({})).await.unwrap();
    assert_eq!(data["size"], content.len() as u64);
    assert_eq!(data["sha256"], sha_hex(&content));
    assert_eq!(data["chunk_count"], 3);
}

#[tokio::test]
async fn refuses_a_file_over_the_engines_ceiling_before_reading() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("big.bin");
    std::fs::write(&file, pattern(1000)).unwrap();
    let error = handle(&file, dir.path(), json!({"max_bytes": 999}))
        .await
        .unwrap_err();
    assert!(error.starts_with("File too large"), "{error}");
}

#[tokio::test]
async fn refuses_a_path_outside_the_scoped_folders() {
    let scope = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let file = elsewhere.path().join("secret.txt");
    std::fs::write(&file, b"nope").unwrap();
    let error = handle(&file, scope.path(), json!({})).await.unwrap_err();
    assert!(error.contains("outside scoped folders"), "{error}");
}

#[tokio::test]
async fn refuses_a_denied_file_inside_scope() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(".env");
    std::fs::write(&file, b"TOKEN=1").unwrap();
    let error = handle(&file, dir.path(), json!({})).await.unwrap_err();
    assert!(error.contains("sensitive file"), "{error}");
}

#[tokio::test]
async fn refuses_a_directory() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("folder");
    std::fs::create_dir(&sub).unwrap();
    let error = handle(&sub, dir.path(), json!({})).await.unwrap_err();
    assert!(error.contains("Cannot read a directory"), "{error}");
}

#[cfg(unix)]
#[tokio::test]
async fn refuses_a_symlink_that_escapes_the_scope() {
    let scope = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let target = elsewhere.path().join("outside.txt");
    std::fs::write(&target, b"outside").unwrap();
    let link = scope.path().join("link.txt");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let error = handle(&link, scope.path(), json!({})).await.unwrap_err();
    assert!(error.contains("outside scoped folders"), "{error}");
}
