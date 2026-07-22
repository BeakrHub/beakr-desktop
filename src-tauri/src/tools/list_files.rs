use std::sync::{Arc, Mutex};

use ignore::{WalkBuilder, WalkState};
use serde_json::{json, Value};

use crate::security;
use crate::unicode;

/// Listing order. `Name` preserves the original stable alphabetical contract;
/// `Modified`/`Size` exist so "most recent file" / "biggest file" questions
/// get a correct answer instead of the model fishing timestamps out of an
/// alphabetical wall (ENG-1668).
#[derive(Clone, Copy, PartialEq)]
enum SortBy {
    Name,
    Modified,
    Size,
}

/// Handle a `list_files` request.
///
/// Params:
/// - `path` (string, required): Directory to list
/// - `recursive` (bool, optional): Recurse into subdirectories
/// - `pattern` (string, optional): Glob pattern to filter files
/// - `sort_by` (string, optional): "name" (default) | "modified" | "size"
/// - `order` (string, optional): "asc" | "desc" (default: asc for name, desc otherwise)
/// - `max_results` (number, optional): Cap the listing, applied AFTER sorting
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

    let sort_by = match params.get("sort_by").and_then(|v| v.as_str()) {
        None | Some("name") => SortBy::Name,
        Some("modified") => SortBy::Modified,
        Some("size") => SortBy::Size,
        Some(other) => return Err(format!("Invalid sort_by: {other}")),
    };
    // "Newest first" / "biggest first" is what recency and size questions
    // mean; alphabetical stays ascending.
    let descending = match params.get("order").and_then(|v| v.as_str()) {
        Some("asc") => false,
        Some("desc") => true,
        None => !matches!(sort_by, SortBy::Name),
        Some(other) => return Err(format!("Invalid order: {other}")),
    };
    let max_results = params
        .get("max_results")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);

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

            // Prune denied (sensitive) and noise (build/cache) directories;
            // silently drop denied files.
            if is_dir
                && (security::is_denied(entry_path)
                    || crate::search_filter::is_excluded_dir(entry_path))
            {
                return WalkState::Skip;
            }
            if security::is_denied(entry_path) {
                return WalkState::Continue;
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

    // The parallel walk yields in nondeterministic order; sort for a stable
    // listing. RFC3339 UTC timestamps compare correctly as strings; entries
    // without a timestamp sort as oldest. Path is the tiebreak so equal keys
    // stay deterministic.
    let mut out = std::mem::take(&mut *files.lock().unwrap());
    out.sort_by(|a, b| {
        let path_cmp = a
            .get("path")
            .and_then(|v| v.as_str())
            .cmp(&b.get("path").and_then(|v| v.as_str()));
        let key_cmp = match sort_by {
            SortBy::Name => path_cmp,
            SortBy::Modified => a
                .get("modified_at")
                .and_then(|v| v.as_str())
                .cmp(&b.get("modified_at").and_then(|v| v.as_str())),
            SortBy::Size => a
                .get("size")
                .and_then(|v| v.as_u64())
                .cmp(&b.get("size").and_then(|v| v.as_u64())),
        };
        let ordered = if descending { key_cmp.reverse() } else { key_cmp };
        ordered.then(path_cmp)
    });

    // Cap AFTER sorting, so "newest N" is the newest N. The engine has always
    // sent max_results; the pre-index implementation honored it and this
    // rewrite silently dropped it (ENG-1668) - for a big folder that meant
    // unbounded payloads and, once anything truncated downstream, the newest
    // files vanishing alphabetically.
    let total = out.len();
    if let Some(cap) = max_results {
        out.truncate(cap);
    }
    let truncated = out.len() < total;
    Ok((json!({ "files": out, "total_found": total, "truncated": truncated }), None))
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

    fn backdate(tree: &TempTree, rel: &str, hours_ago: u64) {
        use std::time::{Duration, SystemTime};
        let f = fs::File::options()
            .write(true)
            .open(tree.root.join(rel))
            .unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(hours_ago * 3600))
            .unwrap();
    }

    // Regression (ENG-1668): "most recently downloaded file" questions were
    // wrong because the listing was alphabetical-only and the cap was ignored.
    // The newest file must be FIRST under sort_by=modified, and the cap must
    // apply after sorting so "newest N" is actually the newest N.
    #[tokio::test]
    async fn sort_by_modified_puts_newest_first_and_caps_after_sort() {
        let tree = TempTree::new("mtime");
        tree.write("aaa_old.txt", "x");
        tree.write("mmm_newest.txt", "x");
        tree.write("zzz_middle.txt", "x");
        backdate(&tree, "aaa_old.txt", 3);
        backdate(&tree, "zzz_middle.txt", 2);

        let (value, _) = handle(
            json!({ "path": tree.path_str(), "sort_by": "modified", "max_results": 2 }),
            &tree.scoped(),
        )
        .await
        .unwrap();

        // Newest first (desc is the default for modified), capped to 2, and
        // the alphabetically-first-but-oldest file is the one dropped.
        assert_eq!(
            names(&value),
            vec!["mmm_newest.txt".to_string(), "zzz_middle.txt".to_string()]
        );
        assert_eq!(value.get("total_found").unwrap(), 3);
        assert_eq!(value.get("truncated").unwrap(), true);
    }

    #[tokio::test]
    async fn max_results_caps_the_default_alphabetical_listing() {
        let tree = TempTree::new("cap");
        tree.write("a.txt", "x");
        tree.write("b.txt", "x");
        tree.write("c.txt", "x");

        let (value, _) = handle(
            json!({ "path": tree.path_str(), "max_results": 2 }),
            &tree.scoped(),
        )
        .await
        .unwrap();

        assert_eq!(names(&value), vec!["a.txt".to_string(), "b.txt".to_string()]);
        assert_eq!(value.get("truncated").unwrap(), true);
    }

    #[tokio::test]
    async fn invalid_sort_by_is_a_clear_error() {
        let tree = TempTree::new("badsort");
        let err = handle(
            json!({ "path": tree.path_str(), "sort_by": "frecency" }),
            &tree.scoped(),
        )
        .await
        .unwrap_err();
        assert!(err.contains("Invalid sort_by"), "got {err}");
    }
}
