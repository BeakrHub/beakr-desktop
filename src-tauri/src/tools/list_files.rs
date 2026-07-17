use std::sync::{Arc, Mutex};

use ignore::{WalkBuilder, WalkState};
use serde_json::{json, Value};

use crate::security;
use crate::unicode;

/// Handle a `list_files` request.
///
/// Params:
/// - `path` (string, required): Directory to list
/// - `recursive` (bool, optional): Recurse into subdirectories
/// - `pattern` (string, optional): Glob pattern to filter files
pub async fn handle(
    params: Value,
    scoped_folders: &[String],
) -> Result<(Value, Option<u64>), String> {
    let path = params
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or("list_files requires 'path' parameter")?;

    let recursive = params
        .get("recursive")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let pattern = params.get("pattern").and_then(|v| v.as_str());

    // Validate path is within scoped folders.
    let canonical = security::validate_path(path, scoped_folders).map_err(|e| e.to_string())?;

    // Compile glob pattern if provided.
    let glob_pattern = match pattern {
        Some(p) => Some(glob::Pattern::new(p).map_err(|e| format!("Invalid glob pattern: {e}"))?),
        None => None,
    };

    // Parallel walk (ripgrep's `ignore` crate). Standard ignore-file/hidden
    // filtering is disabled to preserve the previous walkdir semantics; the win
    // is multi-core traversal plus pruning denied directories (node_modules,
    // .git, …) at the directory level instead of descending and discarding.
    let mut builder = WalkBuilder::new(&canonical);
    builder
        .standard_filters(false)
        .hidden(false)
        .parents(false)
        .follow_links(false)
        .max_depth(if recursive { None } else { Some(1) });

    let root = Arc::new(canonical.clone());
    let glob_pattern = Arc::new(glob_pattern);
    let files = Arc::new(Mutex::new(Vec::<Value>::new()));

    builder.build_parallel().run(|| {
        let root = Arc::clone(&root);
        let glob_pattern = Arc::clone(&glob_pattern);
        let files = Arc::clone(&files);
        Box::new(move |entry| {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => return WalkState::Continue,
            };
            let entry_path = entry.path();

            // Skip the root directory itself.
            if entry_path == root.as_path() {
                return WalkState::Continue;
            }

            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);

            // Silently drop denied entries; prune denied directories.
            if security::is_denied(entry_path) {
                return if is_dir {
                    WalkState::Skip
                } else {
                    WalkState::Continue
                };
            }

            let file_name = entry.file_name().to_string_lossy().to_string();

            // Apply glob filter to output (non-matching directories are still
            // descended so their matching children are listed).
            if let Some(pat) = glob_pattern.as_ref() {
                if !pat.matches(&file_name) {
                    return WalkState::Continue;
                }
            }

            let metadata = match entry.metadata() {
                Ok(m) => m,
                Err(_) => return WalkState::Continue,
            };

            let file_type = if metadata.is_dir() {
                "directory"
            } else if metadata.is_symlink() {
                "symlink"
            } else {
                "file"
            };

            let modified_at = metadata
                .modified()
                .ok()
                .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339());

            files.lock().unwrap().push(json!({
                "name": unicode::normalize_whitespace(&file_name),
                "path": unicode::normalize_whitespace(&entry_path.display().to_string()),
                "size": metadata.len(),
                "type": file_type,
                "modified_at": modified_at,
            }));
            WalkState::Continue
        })
    });

    // The parallel walk yields in nondeterministic order; sort by path so the
    // listing is stable.
    let mut out = std::mem::take(&mut *files.lock().unwrap());
    out.sort_by(|a, b| {
        a.get("path")
            .and_then(|v| v.as_str())
            .cmp(&b.get("path").and_then(|v| v.as_str()))
    });
    Ok((json!({ "files": out }), None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct TempTree {
        root: PathBuf,
    }

    impl TempTree {
        fn new(tag: &str) -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root =
                env::temp_dir().join(format!("beakr_list_test_{tag}_{}_{n}", std::process::id()));
            fs::create_dir_all(&root).unwrap();
            Self { root }
        }
        fn write(&self, rel: &str, contents: &str) {
            let path = self.root.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, contents).unwrap();
        }
        fn scoped(&self) -> Vec<String> {
            vec![self.root.display().to_string()]
        }
        fn path_str(&self) -> String {
            self.root.display().to_string()
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).ok();
        }
    }

    fn names(value: &Value) -> Vec<String> {
        value
            .get("files")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f.get("name").unwrap().as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn lists_direct_entries_and_prunes_denied_dirs() {
        let tree = TempTree::new("direct");
        tree.write("a.txt", "x");
        tree.write("sub/b.txt", "x");
        tree.write("node_modules/pkg/index.js", "x");

        let (value, _) = handle(json!({ "path": tree.path_str() }), &tree.scoped())
            .await
            .unwrap();
        let mut found = names(&value);
        found.sort();
        // Non-recursive: direct file + subdir; nested b.txt not listed;
        // node_modules pruned entirely.
        assert_eq!(
            found,
            vec!["a.txt".to_string(), "sub".to_string()],
            "got {found:?}"
        );
    }

    #[tokio::test]
    async fn recursive_with_glob_filters_and_descends() {
        let tree = TempTree::new("recursive");
        tree.write("top.md", "x");
        tree.write("nested/deep.md", "x");
        tree.write("nested/skip.txt", "x");

        let (value, _) = handle(
            json!({ "path": tree.path_str(), "recursive": true, "pattern": "*.md" }),
            &tree.scoped(),
        )
        .await
        .unwrap();
        let mut found = names(&value);
        found.sort();
        assert_eq!(
            found,
            vec!["deep.md".to_string(), "top.md".to_string()],
            "got {found:?}"
        );
    }
}
