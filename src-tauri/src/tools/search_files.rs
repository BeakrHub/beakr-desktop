use std::io::{BufRead, BufReader};
use std::path::Path;

use serde_json::{json, Value};
use walkdir::WalkDir;

use crate::security;

/// Handle a `search_files` request.
///
/// Params:
/// - `query` (string, required): Search term
/// - `path` (string, optional): Limit search to this directory
/// - `search_content` (bool, optional): Search inside file contents
/// - `file_types` (array of strings, optional): Filter by extension
/// - `limit` (integer, optional): Max results (default 20)
pub async fn handle(params: Value, scoped_folders: &[String]) -> Result<(Value, Option<u64>), String> {
    let query = params.get("query")
        .and_then(|v| v.as_str())
        .ok_or("search_files requires 'query' parameter")?;

    let search_content = params.get("search_content")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let limit = params.get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(20) as usize;

    let file_types: Option<Vec<String>> = params.get("file_types")
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    // Determine search roots
    let search_roots: Vec<String> = if let Some(path) = params.get("path").and_then(|v| v.as_str()) {
        // Validate the specified path
        security::validate_path(path, scoped_folders)
            .map_err(|e| e.to_string())?;
        vec![path.to_string()]
    } else {
        // Search all scoped folders
        scoped_folders.to_vec()
    };

    let query_lower = query.to_lowercase();
    let mut results = Vec::new();

    for root in &search_roots {
        if results.len() >= limit {
            break;
        }

        let walker = WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok());

        for entry in walker {
            if results.len() >= limit {
                break;
            }

            let entry_path = entry.path();

            // Skip denied paths
            if security::is_denied(entry_path) {
                continue;
            }

            // Skip directories for content/name matching
            if entry.file_type().is_dir() {
                continue;
            }

            let file_name = entry.file_name().to_string_lossy().to_string();

            // Filter by file type if specified
            if let Some(ref types) = file_types {
                let ext = entry_path.extension()
                    .map(|e| e.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                if !types.iter().any(|t| t.to_lowercase() == ext) {
                    continue;
                }
            }

            if search_content {
                // Content search
                match search_file_content(entry_path, &query_lower) {
                    Some(context) => {
                        results.push(json!({
                            "path": entry_path.display().to_string(),
                            "name": file_name,
                            "match_context": context,
                        }));
                    }
                    None => continue,
                }
            } else {
                // Filename search
                if file_name.to_lowercase().contains(&query_lower) {
                    results.push(json!({
                        "path": entry_path.display().to_string(),
                        "name": file_name,
                    }));
                }
            }
        }
    }

    Ok((json!({ "results": results }), None))
}

/// Search a file's content for the query string. Returns the first matching line as context.
fn search_file_content(path: &Path, query: &str) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;

    // Skip large files and binaries
    if metadata.len() > 10 * 1024 * 1024 {
        return None;
    }

    let reader = BufReader::new(file);

    for (line_num, line) in reader.lines().enumerate() {
        let line = match line {
            Ok(l) => l,
            Err(_) => return None, // Likely binary
        };

        if line.to_lowercase().contains(query) {
            let context = if line.len() > 200 {
                format!("L{}: {}…", line_num + 1, &line[..200])
            } else {
                format!("L{}: {}", line_num + 1, line)
            };
            return Some(context);
        }
    }

    None
}
