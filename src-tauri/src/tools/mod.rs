mod benchling;
pub mod coding_agent;
mod file_info;
mod list_files;
mod open_terminal;
mod read_file;
pub mod read_file_chunked;
mod reveal_file;
mod search_files;

use serde_json::Value;

use crate::state::AppState;

/// Dispatch a tool request to the appropriate handler.
///
/// Returns `Ok((data, optional_bytes_transferred))` or `Err(error_message)`.
///
/// Local filesystem tools only need the scoped-folder allowlist. The live
/// Benchling tools (`benchling_*`) additionally need [`AppState`] to read the
/// captured Benchling session, so the full state is threaded through here.
pub async fn dispatch_request(
    tool: &str,
    params: Value,
    scoped_folders: &[String],
    state: &AppState,
) -> Result<(Value, Option<u64>), String> {
    if benchling::handles(tool) {
        return benchling::dispatch(tool, params, state).await;
    }
    // The filesystem tools do synchronous disk work (directory walks, index
    // refreshes, file reads). Run inline, a long one froze the async runtime
    // that also drives the WebSocket heartbeat and Ping/Pong: a filename search
    // that re-walked a large scoped tree took 2.5 minutes on 2026-11-10, the
    // engine dropped the connection for a missed heartbeat after ~70 s, and the
    // finished result had nowhere to go. They run on the blocking pool instead.
    let folders = scoped_folders.to_vec();
    let index = state.file_index.clone();
    match tool {
        "list_files" => {
            on_blocking_pool(move || async move { list_files::handle(params, &folders).await })
                .await
        }
        "search_files" => {
            on_blocking_pool(
                move || async move { search_files::handle(params, &folders, &index).await },
            )
            .await
        }
        "read_file" => {
            on_blocking_pool(
                move || async move { read_file::handle(params, &folders, &index).await },
            )
            .await
        }
        "file_info" => {
            on_blocking_pool(move || async move { file_info::handle(params, &folders).await }).await
        }
        "reveal_file" => {
            on_blocking_pool(move || async move { reveal_file::handle(params, &folders).await })
                .await
        }
        "open_terminal" => open_terminal::handle(params, scoped_folders).await,
        other => Err(format!("Unknown tool: {other}")),
    }
}

/// Run a tool whose body does blocking disk I/O on the runtime's blocking pool,
/// so the async worker threads (and with them the WebSocket heartbeat, Ping and
/// liveness checks) keep running however long the disk work takes.
///
/// The handlers are written as `async fn` but never await real I/O, so the
/// future is driven to completion with `block_on` on the blocking thread.
async fn on_blocking_pool<F, Fut>(make: F) -> Result<(Value, Option<u64>), String>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<(Value, Option<u64>), String>>,
{
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || runtime.block_on(make()))
        .await
        .map_err(|error| format!("The file tool stopped unexpectedly: {error}"))?
}

/// Streaming tools bypass `dispatch_request`: they need the chunk stream and
/// the cancel signal (their children require cleanup on cancel, so the
/// select-drop pattern used for read-only tools is not safe for them).
pub fn is_streaming(tool: &str) -> bool {
    coding_agent::handles(tool) || tool == read_file_chunked::TOOL
}

/// Optional protocol features this build supports, advertised on register so
/// the engine only requests what the connected desktop can serve.
pub fn capabilities() -> Vec<String> {
    vec![read_file_chunked::CAPABILITY.to_string()]
}

pub async fn dispatch_streaming(
    app: &tauri::AppHandle,
    state: &AppState,
    tool: &str,
    params: Value,
    scoped_folders: &[String],
    stream: &crate::ws::ToolStream,
    cancel: crate::ws::inflight::CancelSignal,
) -> Result<(Value, Option<u64>), String> {
    if coding_agent::handles(tool) {
        return coding_agent::handle_streaming(app, state, params, scoped_folders, stream, cancel)
            .await;
    }
    if tool == read_file_chunked::TOOL {
        // Opened before the first chunk is sent, so no ack can arrive for a
        // request that has no channel yet. Closed by inflight.finish().
        let acks = state.inflight.register_chunk_acks(stream.request_id());
        return read_file_chunked::handle_streaming(
            params,
            scoped_folders,
            &state.file_index,
            stream,
            acks,
            cancel,
        )
        .await;
    }
    Err(format!("Unknown streaming tool: {tool}"))
}

#[cfg(test)]
mod blocking_pool_tests {
    use super::on_blocking_pool;
    use serde_json::json;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// Regression for the 2026-11-10 disconnect: a slow filesystem tool must not
    /// stop other tasks on the runtime. On a single-threaded runtime (the worst
    /// case), a ticker task stands in for the heartbeat; it keeps ticking while
    /// a tool body blocks for 400 ms. Run inline, the ticker would not tick once.
    #[tokio::test(flavor = "current_thread")]
    async fn a_slow_file_tool_does_not_stall_other_tasks() {
        let ticks = Arc::new(AtomicU32::new(0));
        let ticker_ticks = Arc::clone(&ticks);
        let ticker = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(20));
            loop {
                interval.tick().await;
                ticker_ticks.fetch_add(1, Ordering::Relaxed);
            }
        });
        // Let the ticker start before the slow tool begins.
        tokio::time::sleep(Duration::from_millis(30)).await;
        let before = ticks.load(Ordering::Relaxed);

        let result = on_blocking_pool(|| async {
            std::thread::sleep(Duration::from_millis(400));
            Ok((json!({"ok": true}), None))
        })
        .await;

        let during = ticks.load(Ordering::Relaxed) - before;
        ticker.abort();
        assert_eq!(result.unwrap().0, json!({"ok": true}));
        assert!(
            during >= 5,
            "the heartbeat stand-in ticked only {during} times while the tool ran"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_tool_error_comes_back_unchanged() {
        let result =
            on_blocking_pool(|| async { Err("path is outside the shared folders".into()) }).await;
        assert_eq!(result.unwrap_err(), "path is outside the shared folders");
    }
}
