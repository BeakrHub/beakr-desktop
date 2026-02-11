use serde_json::{json, Value};
use walkdir::WalkDir;

use crate::security;

/// Handle a `list_files` request.
///
/// Params:
/// - `path` (string, required): Directory to list
/// - `recursive` (bool, optional): Recurse into subdirectories
/// - `pattern` (string, optional): Glob pattern to filter files
pub async fn handle(params: Value, scoped_folders: &[String]) -> Result<(Value, Option<u64>), String> {
    let path = params.get("path")
        .and_then(|v| v.as_str())
        .ok_or("list_files requires 'path' parameter")?;

    let recursive = params.get("recursive")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let pattern = params.get("pattern")
        .and_then(|v| v.as_str());

    // Validate path is within scoped folders
    let canonical = security::validate_path(path, scoped_folders)
        .map_err(|e| e.to_string())?;

    // Compile glob pattern if provided
    let glob_pattern = match pattern {
        Some(p) => Some(glob::Pattern::new(p).map_err(|e| format!("Invalid glob pattern: {e}"))?),
        None => None,
    };

    let max_depth = if recursive { usize::MAX } else { 1 };

    let mut files = Vec::new();

    let walker = WalkDir::new(&canonical)
        .max_depth(max_depth)
        .follow_links(false);

    for entry in walker.into_iter().filter_map(|e| e.ok()) {
        // Skip the root directory itself
        if entry.path() == canonical {
            continue;
        }

        let entry_path = entry.path();

        // Silently skip denied files in listings
        if security::is_denied(entry_path) {
            continue;
        }

        let file_name = entry.file_name().to_string_lossy().to_string();

        // Apply glob filter
        if let Some(ref pat) = glob_pattern {
            if !pat.matches(&file_name) {
                continue;
            }
        }

        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };

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

        files.push(json!({
            "name": file_name,
            "path": entry_path.display().to_string(),
            "size": metadata.len(),
            "type": file_type,
            "modified_at": modified_at,
        }));
    }

    Ok((json!({ "files": files }), None))
}
