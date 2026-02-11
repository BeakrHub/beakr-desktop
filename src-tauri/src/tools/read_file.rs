use std::io::{BufReader, Read};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde_json::{json, Value};

use crate::security;

const MAX_FILE_SIZE: u64 = 50 * 1024 * 1024; // 50 MB
const BINARY_CHECK_SIZE: usize = 8192;

/// Handle a `read_file` request.
///
/// Params:
/// - `path` (string, required): File to read
/// - `encoding` (string, optional): Text encoding (default "utf-8")
/// - `max_lines` (integer, optional): Limit lines returned
pub async fn handle(params: Value, scoped_folders: &[String]) -> Result<(Value, Option<u64>), String> {
    let path = params.get("path")
        .and_then(|v| v.as_str())
        .ok_or("read_file requires 'path' parameter")?;

    let max_lines = params.get("max_lines")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);

    // Validate path
    let canonical = security::validate_path(path, scoped_folders)
        .map_err(|e| e.to_string())?;

    // Check deny list
    if security::is_denied(&canonical) {
        return Err(format!("Access denied — sensitive file: {path}"));
    }

    // Check file size
    let metadata = std::fs::metadata(&canonical)
        .map_err(|e| format!("Cannot read file: {e}"))?;

    if metadata.len() > MAX_FILE_SIZE {
        return Err(format!(
            "File too large ({:.1} MB). Maximum is 50 MB. Use local_file_info for metadata.",
            metadata.len() as f64 / 1024.0 / 1024.0
        ));
    }

    if metadata.is_dir() {
        return Err("Cannot read a directory. Use list_files instead.".to_string());
    }

    // Binary detection: check first 8KB for null bytes
    let file = std::fs::File::open(&canonical)
        .map_err(|e| format!("Cannot open file: {e}"))?;

    let mut reader = BufReader::new(file);
    let mut check_buf = vec![0u8; BINARY_CHECK_SIZE];
    let bytes_read = reader.read(&mut check_buf)
        .map_err(|e| format!("Read error: {e}"))?;

    let is_binary = check_buf[..bytes_read].contains(&0);

    if is_binary {
        // Binary file: read raw bytes and return base64-encoded
        let raw_bytes = std::fs::read(&canonical)
            .map_err(|e| format!("Cannot read binary file: {e}"))?;
        let bytes_transferred = raw_bytes.len() as u64;
        let encoded = BASE64.encode(&raw_bytes);

        return Ok((
            json!({ "content": encoded, "encoding": "base64" }),
            Some(bytes_transferred),
        ));
    }

    // Text file: read as UTF-8 string
    let content = std::fs::read_to_string(&canonical)
        .map_err(|e| format!("Cannot read file as text: {e}"))?;

    let bytes_transferred = content.len() as u64;

    // Apply max_lines if specified
    let content = match max_lines {
        Some(limit) => {
            let lines: Vec<&str> = content.lines().take(limit).collect();
            lines.join("\n")
        }
        None => content,
    };

    Ok((json!({ "content": content, "encoding": "utf-8" }), Some(bytes_transferred)))
}
