mod file_info;
mod list_files;
mod read_file;
mod search_files;

use serde_json::Value;

/// Dispatch a tool request to the appropriate handler.
///
/// Returns `Ok((data, optional_bytes_transferred))` or `Err(error_message)`.
pub async fn dispatch_request(
    tool: &str,
    params: Value,
    scoped_folders: &[String],
) -> Result<(Value, Option<u64>), String> {
    match tool {
        "list_files" => list_files::handle(params, scoped_folders).await,
        "search_files" => search_files::handle(params, scoped_folders).await,
        "read_file" => read_file::handle(params, scoped_folders).await,
        "file_info" => file_info::handle(params, scoped_folders).await,
        other => Err(format!("Unknown tool: {other}")),
    }
}
