use serde_json::{json, Value};

use crate::security;

/// Handle a `file_info` request.
///
/// Params:
/// - `path` (string, required): File path to inspect
pub async fn handle(params: Value, scoped_folders: &[String]) -> Result<(Value, Option<u64>), String> {
    let path = params.get("path")
        .and_then(|v| v.as_str())
        .ok_or("file_info requires 'path' parameter")?;

    // Validate path
    let canonical = security::validate_path(path, scoped_folders)
        .map_err(|e| e.to_string())?;

    // Check deny list
    if security::is_denied(&canonical) {
        return Err(format!("Access denied — sensitive file: {path}"));
    }

    let metadata = std::fs::metadata(&canonical)
        .map_err(|e| format!("Cannot read file metadata: {e}"))?;

    let file_type = if metadata.is_dir() {
        "directory"
    } else if metadata.is_symlink() {
        "symlink"
    } else {
        "file"
    };

    let modified_at = metadata.modified().ok().map(|t| {
        chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339()
    });

    let permissions = format_permissions(&metadata);

    let is_readable = canonical.exists()
        && std::fs::File::open(&canonical).is_ok();

    let file_name = canonical
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    Ok((json!({
        "name": file_name,
        "path": canonical.display().to_string(),
        "size": metadata.len(),
        "type": file_type,
        "modified_at": modified_at,
        "permissions": permissions,
        "is_readable": is_readable,
    }), None))
}

/// Format file permissions in a cross-platform way.
/// Unix: octal mode string (e.g. "644"). Windows: "readonly" or "read-write".
fn format_permissions(metadata: &std::fs::Metadata) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        format!("{:o}", metadata.permissions().mode())
    }
    #[cfg(not(unix))]
    {
        if metadata.permissions().readonly() {
            "readonly".to_string()
        } else {
            "read-write".to_string()
        }
    }
}
