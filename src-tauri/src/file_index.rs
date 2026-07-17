//! In-memory metadata cache for the user's approved (scoped) folders.
//!
//! `search_files` used to walk the whole scoped tree from disk on every query.
//! The index holds each file's path and name in memory so a repeat
//! filename/path search answers without touching disk. Freshness is
//! kept per-directory: a directory whose modification time is unchanged since
//! the last snapshot keeps its cached file list (we skip re-`stat`ing every
//! file in it), while a changed directory is re-read. Adding/removing a direct
//! child bumps a directory's mtime, so structural changes are always caught;
//! deep changes are caught because we still recurse into subdirectories.
//!
//! Denied directories (`node_modules`, `.git`, …) and denied files are pruned
//! at build time, and the query path re-checks scope, so the index can never
//! surface a path the tools would otherwise block.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::SystemTime;

use crate::security;
use crate::unicode;

/// One indexed file. Ranking metadata (mtime, etc.) is added in the ranking
/// slice (PR3) where it is actually consumed.
#[derive(Clone)]
pub struct FileMeta {
    pub path: PathBuf,
    pub name: String,
}

/// A full snapshot of the indexed trees, keyed by directory.
#[derive(Clone, Default)]
struct IndexState {
    /// The scoped-folder roots this snapshot was built from. A change forces a
    /// full rebuild rather than an incremental refresh.
    roots: Vec<String>,
    /// Per-directory modification time at snapshot time.
    dir_mtimes: HashMap<PathBuf, SystemTime>,
    /// Per-directory list of (non-denied) subdirectories.
    subdirs: HashMap<PathBuf, Vec<PathBuf>>,
    /// Per-directory list of that directory's direct (non-denied) files.
    files: HashMap<PathBuf, Vec<FileMeta>>,
}

/// Thread-safe metadata cache over the scoped folders.
pub struct FileIndex {
    state: RwLock<IndexState>,
}

impl Default for FileIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl FileIndex {
    pub fn new() -> Self {
        Self {
            state: RwLock::new(IndexState::default()),
        }
    }

    /// Refresh the index for `roots`, reusing unchanged directories.
    ///
    /// The scan runs against a cloned snapshot without holding the lock, then
    /// swaps the result in under a brief write lock, so concurrent searches are
    /// never blocked by the walk itself.
    pub fn refresh(&self, roots: &[String]) {
        let old = {
            let guard = self.state.read().unwrap();
            if guard.roots == roots {
                guard.clone()
            } else {
                IndexState::default() // root set changed -> full rebuild
            }
        };

        let mut next = IndexState {
            roots: roots.to_vec(),
            ..Default::default()
        };
        for root in roots {
            let root_path = PathBuf::from(root);
            // Never index a denied or unreadable root.
            if security::is_denied(&root_path) {
                continue;
            }
            scan_dir(&root_path, &old, &mut next);
        }

        *self.state.write().unwrap() = next;
    }

    /// Filename/path search over the cached metadata.
    ///
    /// `root_filter`, when set, restricts results to a subtree (the tool's
    /// optional `path` parameter). Matching is on the normalized basename so an
    /// ASCII-space query still finds Unicode-whitespace names, mirroring the
    /// previous walk-based behavior. Returns at most `limit` hits.
    pub fn search_names(
        &self,
        query_lower: &str,
        root_filter: Option<&Path>,
        file_types: Option<&[String]>,
        limit: usize,
    ) -> Vec<FileMeta> {
        let guard = self.state.read().unwrap();
        let mut out = Vec::new();
        for files in guard.files.values() {
            for file in files {
                if let Some(root) = root_filter {
                    if !file.path.starts_with(root) {
                        continue;
                    }
                }
                if let Some(types) = file_types {
                    let ext = file
                        .path
                        .extension()
                        .map(|e| e.to_string_lossy().to_lowercase())
                        .unwrap_or_default();
                    if !types.iter().any(|t| t.to_lowercase() == ext) {
                        continue;
                    }
                }
                let normalized = unicode::normalize_whitespace(&file.name);
                if normalized.to_lowercase().contains(query_lower) {
                    out.push(file.clone());
                    if out.len() >= limit {
                        return out;
                    }
                }
            }
        }
        out
    }
}

/// Recursively refresh `dir` into `next`, reusing `old`'s cached entries for
/// any directory whose mtime is unchanged.
fn scan_dir(dir: &Path, old: &IndexState, next: &mut IndexState) {
    let mtime = match std::fs::metadata(dir).and_then(|m| m.modified()) {
        Ok(m) => m,
        Err(_) => return, // unreadable/removed directory: drop it from the index
    };

    let unchanged = old.dir_mtimes.get(dir) == Some(&mtime);

    let subdirs: Vec<PathBuf> = if unchanged {
        // Direct children are unchanged (an add/remove would bump the mtime),
        // so reuse this directory's cached files and subdir list with no I/O.
        if let Some(cached) = old.files.get(dir) {
            next.files.insert(dir.to_path_buf(), cached.clone());
        }
        let cached_subdirs = old.subdirs.get(dir).cloned().unwrap_or_default();
        next.subdirs.insert(dir.to_path_buf(), cached_subdirs.clone());
        cached_subdirs
    } else {
        // Re-read this directory's entries and re-stat its files.
        let mut files = Vec::new();
        let mut dirs = Vec::new();
        if let Ok(read) = std::fs::read_dir(dir) {
            for entry in read.flatten() {
                let path = entry.path();
                // Only follow real files/dirs; symlinks are skipped, matching
                // the tools' follow_links(false).
                let file_type = match entry.file_type() {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                if security::is_denied(&path) {
                    continue; // prune denied dirs and files at build time
                }
                if file_type.is_dir() {
                    dirs.push(path);
                } else if file_type.is_file() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    files.push(FileMeta { path, name });
                }
            }
        }
        next.files.insert(dir.to_path_buf(), files);
        next.subdirs.insert(dir.to_path_buf(), dirs.clone());
        dirs
    };

    next.dir_mtimes.insert(dir.to_path_buf(), mtime);

    for sub in &subdirs {
        scan_dir(sub, old, next);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct TempTree {
        root: PathBuf,
    }

    impl TempTree {
        fn new(tag: &str) -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = env::temp_dir().join(format!("beakr_index_test_{tag}_{}_{n}", std::process::id()));
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
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).ok();
        }
    }

    fn found_names(hits: &[FileMeta]) -> Vec<String> {
        let mut v: Vec<String> = hits.iter().map(|f| f.name.clone()).collect();
        v.sort();
        v
    }

    #[test]
    fn indexes_and_searches_names() {
        let tree = TempTree::new("names");
        tree.write("notes.md", "x");
        tree.write("nested/notebook.md", "x");
        tree.write("todo.txt", "x");

        let index = FileIndex::new();
        index.refresh(&tree.scoped());

        let hits = index.search_names("note", None, None, 20);
        assert_eq!(found_names(&hits), vec!["notebook.md", "notes.md"]);
    }

    #[test]
    fn prunes_denied_directories() {
        let tree = TempTree::new("deny");
        tree.write("node_modules/pkg/index.js", "x");
        tree.write("src/index.js", "x");

        let index = FileIndex::new();
        index.refresh(&tree.scoped());

        let hits = index.search_names("index", None, None, 20);
        let paths: Vec<String> = hits.iter().map(|f| f.path.display().to_string()).collect();
        assert_eq!(hits.len(), 1, "got {paths:?}");
        assert!(paths[0].ends_with("src/index.js"), "got {paths:?}");
    }

    #[test]
    fn file_types_and_limit_respected() {
        let tree = TempTree::new("types");
        tree.write("a.rs", "x");
        tree.write("a.txt", "x");
        let index = FileIndex::new();
        index.refresh(&tree.scoped());

        let rs = index.search_names("a", None, Some(&["rs".to_string()]), 20);
        assert_eq!(found_names(&rs), vec!["a.rs"]);

        let capped = index.search_names("a", None, None, 1);
        assert_eq!(capped.len(), 1);
    }

    #[test]
    fn refresh_reflects_new_and_removed_files() {
        let tree = TempTree::new("refresh");
        tree.write("first.txt", "x");
        let index = FileIndex::new();
        index.refresh(&tree.scoped());
        assert_eq!(index.search_names("first", None, None, 20).len(), 1);

        // Add a file, refresh, and confirm it appears (the dir mtime changed).
        tree.write("second.txt", "x");
        index.refresh(&tree.scoped());
        assert_eq!(index.search_names("second", None, None, 20).len(), 1);

        // Remove the first file and confirm it drops out.
        fs::remove_file(tree.root.join("first.txt")).unwrap();
        index.refresh(&tree.scoped());
        assert_eq!(index.search_names("first", None, None, 20).len(), 0);
    }

    #[test]
    fn root_filter_restricts_to_subtree() {
        let tree = TempTree::new("subtree");
        tree.write("keep/target.md", "x");
        tree.write("other/target.md", "x");
        let index = FileIndex::new();
        index.refresh(&tree.scoped());

        let subtree = tree.root.join("keep");
        let hits = index.search_names("target", Some(&subtree), None, 20);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.starts_with(&subtree));
    }
}
