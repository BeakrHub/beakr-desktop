mod benchling;
pub mod coding_agent;
mod file_info;
mod list_files;
mod read_file;
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
    match tool {
        "list_files" => list_files::handle(params, scoped_folders).await,
        "search_files" => {
            search_files::handle(params, scoped_folders, &state.file_index).await
        }
        "read_file" => read_file::handle(params, scoped_folders, &state.file_index).await,
        "file_info" => file_info::handle(params, scoped_folders).await,
        "reveal_file" => reveal_file::handle(params, scoped_folders).await,
        other => Err(format!("Unknown tool: {other}")),
    }
}

/// Streaming tools bypass `dispatch_request`: they need the chunk stream and
/// the cancel signal (their children require cleanup on cancel, so the
/// select-drop pattern used for read-only tools is not safe for them).
pub fn is_streaming(tool: &str) -> bool {
    coding_agent::handles(tool)
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
    Err(format!("Unknown streaming tool: {tool}"))
}
