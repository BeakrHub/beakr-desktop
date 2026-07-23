mod benchling;
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
