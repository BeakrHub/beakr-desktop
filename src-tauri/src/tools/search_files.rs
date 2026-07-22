use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ignore::{WalkBuilder, WalkState};
use serde_json::{json, Value};

use crate::file_index::FileIndex;
use crate::security;
use crate::unicode;

/// Handle a `search_files` request.
///
/// Params:
/// - `query` (string, required): Search term
/// - `path` (string, optional): Limit search to this directory
/// - `search_content` (bool, optional): Search inside file contents
/// - `file_types` (array of strings, optional): Filter by extension
/// - `limit` (integer, optional): Max results (default 20)
pub async fn handle(
    params: Value,
    scoped_folders: &[String],
    index: &FileIndex,
) -> Result<(Value, Option<u64>), String> {
    let query = params
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or("search_files requires 'query' parameter")?;

    let search_content = params
        .get("search_content")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;

    let file_types: Option<Vec<String>> = params
        .get("file_types")
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    // An explicit `path` restricts the search to a subtree; validate it against
    // scope up front (applies to both filename and content search).
    let path_param = params.get("path").and_then(|v| v.as_str());
    if let Some(path) = path_param {
        security::validate_path(path, scoped_folders).map_err(|e| e.to_string())?;
    }

    // Filename/path search is served from the in-memory index — no disk walk on
    // repeat queries. The index prunes denied dirs/files at build time and is
    // refreshed incrementally (unchanged directories are reused).
    if !search_content {
        index.ensure_fresh(scoped_folders);
        let root_filter = path_param.map(PathBuf::from);
        let hits = index.search_names(
            query,
            root_filter.as_deref(),
            file_types.as_deref(),
            limit,
        );
        let results: Vec<Value> = hits
            .iter()
            .map(|f| {
                json!({
                    "path": unicode::normalize_whitespace(&f.path.display().to_string()),
                    "name": unicode::normalize_whitespace(&f.name),
                })
            })
            .collect();
        return Ok((json!({ "results": results }), None));
    }

    // Content search must read file bodies, so it still walks the tree.
    let search_roots: Vec<String> = match path_param {
        Some(path) => vec![path.to_string()],
        None => scoped_folders.to_vec(),
    };
    if search_roots.is_empty() {
        return Ok((json!({ "results": [] }), None));
    }

    // Walk all roots in parallel (ripgrep's `ignore` crate). Standard
    // ignore-file/hidden filtering is disabled so results match the previous
    // single-threaded walkdir semantics exactly; the speedup is twofold:
    // multi-core traversal, and pruning denied directories (node_modules,
    // .git, .venv, …) at the directory level so we never descend into them —
    // the old code walked every file inside them only to discard each one.
    let mut builder = WalkBuilder::new(&search_roots[0]);
    for root in &search_roots[1..] {
        builder.add(root);
    }
    builder
        .standard_filters(false)
        .hidden(false)
        .parents(false)
        .follow_links(false);

    let query_lower = Arc::new(query.to_lowercase());
    let file_types = Arc::new(file_types);
    let results = Arc::new(Mutex::new(Vec::<Value>::new()));
    let count = Arc::new(AtomicUsize::new(0));

    builder.build_parallel().run(|| {
        let query_lower = Arc::clone(&query_lower);
        let file_types = Arc::clone(&file_types);
        let results = Arc::clone(&results);
        let count = Arc::clone(&count);
        Box::new(move |entry| {
            // Enough already found — stop dispatching new work.
            if count.load(Ordering::Relaxed) >= limit {
                return WalkState::Quit;
            }
            let entry = match entry {
                Ok(e) => e,
                Err(_) => return WalkState::Continue,
            };
            let entry_path = entry.path();
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);

            if is_dir {
                // Prune denied directories entirely (never descend).
                if security::is_denied(entry_path) {
                    return WalkState::Skip;
                }
                return WalkState::Continue;
            }

            // Skip denied files (e.g. *.key, .env) that aren't in a denied dir.
            if security::is_denied(entry_path) {
                return WalkState::Continue;
            }

            let file_name = entry.file_name().to_string_lossy().to_string();

            // Filter by file type if specified.
            if let Some(types) = file_types.as_ref() {
                let ext = entry_path
                    .extension()
                    .map(|e| e.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                if !types.iter().any(|t| t.to_lowercase() == ext) {
                    return WalkState::Continue;
                }
            }

            // Only content search reaches the walker; filename/path search is
            // served from the index above.
            let hit = search_file_content(entry_path, query_lower.as_str()).map(|context| {
                json!({
                    "path": unicode::normalize_whitespace(&entry_path.display().to_string()),
                    "name": unicode::normalize_whitespace(&file_name),
                    "match_context": context,
                })
            });

            if let Some(hit) = hit {
                let mut guard = results.lock().unwrap();
                if guard.len() < limit {
                    guard.push(hit);
                    if count.fetch_add(1, Ordering::Relaxed) + 1 >= limit {
                        return WalkState::Quit;
                    }
                }
            }
            WalkState::Continue
        })
    });

    let mut out = std::mem::take(&mut *results.lock().unwrap());
    out.truncate(limit);
    Ok((json!({ "results": out }), None))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// A freshly-created, uniquely-named temp directory, removed on drop so a
    /// panicking test still cleans up. Matches the repo convention of building
    /// fixtures under `std::env::temp_dir()` (no `tempfile` dev-dep).
    struct TempTree {
        root: PathBuf,
    }

    impl TempTree {
        fn new(tag: &str) -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = env::temp_dir().join(format!(
                "beakr_search_test_{tag}_{}_{n}",
                std::process::id()
            ));
            fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        /// Write `contents` to `rel` under the tree, creating parent dirs.
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
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).ok();
        }
    }

    /// Pull the `results` array out of a handler response.
    fn results(value: &Value) -> &Vec<Value> {
        value.get("results").unwrap().as_array().unwrap()
    }

    fn names(value: &Value) -> Vec<String> {
        results(value)
            .iter()
            .map(|r| r.get("name").unwrap().as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn filename_search_finds_matching_file() {
        let tree = TempTree::new("filename");
        tree.write("notes.md", "content");
        tree.write("todo.txt", "content");
        tree.write("nested/notebook.md", "content");

        let (value, _) = handle(json!({ "query": "note" }), &tree.scoped(), &FileIndex::new())
            .await
            .unwrap();
        let found = names(&value);
        assert!(found.contains(&"notes.md".to_string()), "got {found:?}");
        assert!(found.contains(&"notebook.md".to_string()), "got {found:?}");
        assert!(!found.contains(&"todo.txt".to_string()), "got {found:?}");
    }

    #[tokio::test]
    async fn content_search_returns_line_context() {
        let tree = TempTree::new("content");
        tree.write("a.txt", "first line\nthe needle is here\nlast line");

        let (value, _) = handle(
            json!({ "query": "needle", "search_content": true }),
            &tree.scoped(),
            &FileIndex::new(),
        )
        .await
        .unwrap();
        let arr = results(&value);
        assert_eq!(arr.len(), 1);
        let ctx = arr[0].get("match_context").unwrap().as_str().unwrap();
        assert!(ctx.contains("needle"), "context was {ctx:?}");
        assert!(ctx.starts_with("L2:"), "expected line 2, got {ctx:?}");
    }

    #[tokio::test]
    async fn file_types_filter_restricts_extension() {
        let tree = TempTree::new("filetypes");
        tree.write("app.rs", "content");
        tree.write("app.txt", "content");

        let (value, _) = handle(
            json!({ "query": "app", "file_types": ["rs"] }),
            &tree.scoped(),
            &FileIndex::new(),
        )
        .await
        .unwrap();
        let found = names(&value);
        assert_eq!(found, vec!["app.rs".to_string()], "got {found:?}");
    }

    #[tokio::test]
    async fn deny_listed_file_is_excluded() {
        let tree = TempTree::new("deny");
        tree.write(".env", "SECRET=needle123");
        tree.write("config.txt", "value=needle123");

        let (value, _) = handle(
            json!({ "query": "needle123", "search_content": true }),
            &tree.scoped(),
            &FileIndex::new(),
        )
        .await
        .unwrap();
        let found = names(&value);
        assert!(found.contains(&"config.txt".to_string()), "got {found:?}");
        assert!(!found.contains(&".env".to_string()), "deny-listed .env leaked: {found:?}");
    }

    #[tokio::test]
    async fn path_outside_scope_errors() {
        let tree = TempTree::new("scope");
        tree.write("a.txt", "content");
        // temp_dir is the parent of the scoped root, so it is out of scope.
        let outside = env::temp_dir().display().to_string();

        let result = handle(
            json!({ "query": "a", "path": outside }),
            &tree.scoped(),
            &FileIndex::new(),
        )
        .await;
        assert!(result.is_err(), "expected out-of-scope error, got {result:?}");
    }

    #[tokio::test]
    async fn denied_directory_is_pruned_not_descended() {
        // node_modules is a denied directory: its contents must never appear,
        // and (post-swap) the walker skips the whole subtree instead of
        // walking every file to discard it.
        let tree = TempTree::new("prune");
        tree.write("node_modules/pkg/index.js", "needle_x");
        tree.write("src/app.js", "needle_x");

        let (value, _) = handle(
            json!({ "query": "needle_x", "search_content": true }),
            &tree.scoped(),
            &FileIndex::new(),
        )
        .await
        .unwrap();
        let found = names(&value);
        assert_eq!(found, vec!["app.js".to_string()], "got {found:?}");
    }

    #[tokio::test]
    async fn limit_caps_result_count() {
        let tree = TempTree::new("limit");
        for i in 0..6 {
            tree.write(&format!("match{i}.txt"), "content");
        }

        let (value, _) = handle(
            json!({ "query": "match", "limit": 2 }),
            &tree.scoped(),
            &FileIndex::new(),
        )
        .await
        .unwrap();
        let arr = results(&value);
        assert!(arr.len() <= 2, "limit not respected: {} results", arr.len());
        assert!(!arr.is_empty());
    }
}
