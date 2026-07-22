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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::SystemTime;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::security;
use crate::unicode;

/// One indexed file.
#[derive(Clone)]
pub struct FileMeta {
    pub path: PathBuf,
    pub name: String,
    /// Modification time, used as a recency signal when ranking results.
    pub modified: Option<SystemTime>,
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
    /// How many times each path has been read via the `read_file` tool. A
    /// frecency signal that boosts files the agent keeps returning to.
    access: Mutex<HashMap<PathBuf, u32>>,
    /// Set when the on-disk trees may have changed since the last refresh (by
    /// the filesystem watcher or the periodic fallback). `ensure_fresh` only
    /// re-walks when this is set, so most searches skip the walk entirely.
    dirty: AtomicBool,
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
            access: Mutex::new(HashMap::new()),
            dirty: AtomicBool::new(true), // first search builds the index
        }
    }

    /// Record that `path` was read, boosting its future ranking (frecency).
    pub fn record_access(&self, path: &Path) {
        *self.access.lock().unwrap().entry(path.to_path_buf()).or_insert(0) += 1;
    }

    /// Flag that the scoped trees may have changed; the next `ensure_fresh`
    /// will re-walk. Called by the filesystem watcher and the periodic fallback.
    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
    }

    /// Refresh the index only if it has been marked dirty since the last walk.
    /// A file change between the flag-clear and the walk simply re-sets the
    /// flag, so the following search reconciles it — no update is lost.
    pub fn ensure_fresh(&self, roots: &[String]) {
        if self.dirty.swap(false, Ordering::AcqRel) {
            self.refresh(roots);
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

    /// Ranked filename/path search over the cached metadata.
    ///
    /// Matching is fuzzy (subsequence, whitespace-split, case-insensitive) via
    /// `nucleo`, so `chat inpt` finds `chat-input.tsx`. Results are ranked by
    /// match quality, then frecency (how often the file has been read), then
    /// recency (newest first), then name for a stable order. `root_filter`, when
    /// set, restricts to a subtree (the tool's optional `path` parameter).
    /// Returns at most `limit` hits.
    pub fn search_names(
        &self,
        query: &str,
        root_filter: Option<&Path>,
        file_types: Option<&[String]>,
        limit: usize,
    ) -> Vec<FileMeta> {
        let guard = self.state.read().unwrap();
        let access = self.access.lock().unwrap();

        let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
        let mut matcher = Matcher::new(Config::DEFAULT);
        let mut buf = Vec::new();

        // (match score, access count, mtime, file) for every fuzzy match.
        let mut scored: Vec<(u32, u32, Option<SystemTime>, &FileMeta)> = Vec::new();
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
                let haystack = Utf32Str::new(&normalized, &mut buf);
                if let Some(score) = pattern.score(haystack, &mut matcher) {
                    let count = access.get(&file.path).copied().unwrap_or(0);
                    scored.push((score, count, file.modified, file));
                }
            }
        }
        drop(access);

        scored.sort_by(|a, b| {
            b.0.cmp(&a.0) // match quality (higher first)
                .then(b.1.cmp(&a.1)) // frecency (more-read first)
                .then(b.2.cmp(&a.2)) // recency (newest first)
                .then(a.3.name.cmp(&b.3.name)) // stable tiebreak
        });

        scored
            .into_iter()
            .take(limit)
            .map(|(_, _, _, file)| file.clone())
            .collect()
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
                    // Skip build/cache noise directories (recall-safe: generic
                    // names only when a sibling manifest proves a build tree).
                    if crate::search_filter::is_excluded_dir(&path) {
                        continue;
                    }
                    dirs.push(path);
                } else if file_type.is_file() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let modified = entry.metadata().ok().and_then(|m| m.modified().ok());
                    files.push(FileMeta {
                        path,
                        name,
                        modified,
                    });
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

    #[test]
    fn ranks_closer_match_first() {
        let tree = TempTree::new("rank");
        tree.write("cfg.txt", "x"); // "cfg" is a contiguous prefix
        tree.write("config_grid_helper.txt", "x"); // c..f..g scattered
        let index = FileIndex::new();
        index.refresh(&tree.scoped());

        let hits = index.search_names("cfg", None, None, 20);
        assert_eq!(hits[0].name, "cfg.txt", "closer match should rank first");
    }

    #[test]
    fn frecency_boosts_a_read_file() {
        let tree = TempTree::new("frecency");
        tree.write("a/config.txt", "x");
        tree.write("b/config.txt", "x"); // identical basename -> identical match score
        let index = FileIndex::new();
        index.refresh(&tree.scoped());

        // With no reads, order is only broken by the stable tiebreak; after
        // reading b's copy it must rank first on frecency.
        let b = tree.root.join("b/config.txt");
        index.record_access(&b);
        let hits = index.search_names("config", None, None, 20);
        assert_eq!(hits[0].path, b, "the read file should rank first");
    }

    #[test]
    fn ensure_fresh_only_rewalks_when_dirty() {
        let tree = TempTree::new("dirty");
        tree.write("first.txt", "x");
        let index = FileIndex::new(); // starts dirty
        index.ensure_fresh(&tree.scoped());
        assert_eq!(index.search_names("first", None, None, 20).len(), 1);

        // Change on disk but do NOT mark dirty: ensure_fresh is a no-op, so the
        // new file is not yet visible.
        tree.write("second.txt", "x");
        index.ensure_fresh(&tree.scoped());
        assert_eq!(index.search_names("second", None, None, 20).len(), 0);

        // Marking dirty (what the watcher / fallback do) triggers the rescan.
        index.mark_dirty();
        index.ensure_fresh(&tree.scoped());
        assert_eq!(index.search_names("second", None, None, 20).len(), 1);
    }

    #[test]
    fn fuzzy_matches_subsequence() {
        let tree = TempTree::new("fuzzy");
        tree.write("chat-input.tsx", "x");
        tree.write("unrelated.rs", "x");
        let index = FileIndex::new();
        index.refresh(&tree.scoped());

        // "chatinpt" is not a substring of "chat-input.tsx" (the dash breaks it)
        // but is a subsequence — the point of fuzzy matching.
        let hits = index.search_names("chatinpt", None, None, 20);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "chat-input.tsx");
    }

    #[test]
    fn excludes_build_noise_keeps_real_source() {
        let tree = TempTree::new("noise");
        // The exact junk the repro surfaced: a real source file plus a mangled
        // .next build chunk that fuzzy-matches the same query.
        tree.write("components/chat/chat-input.tsx", "x");
        tree.write(
            ".next/dev/static/chunks/components_chat_chat-input_tsx_0b3o._.js",
            "x",
        );
        let index = FileIndex::new();
        index.ensure_fresh(&tree.scoped());

        let hits: Vec<String> = index
            .search_names("chat input", None, None, 20)
            .iter()
            .map(|f| f.name.clone())
            .collect();
        assert!(hits.contains(&"chat-input.tsx".to_string()), "got {hits:?}");
        assert!(
            hits.iter().all(|n| !n.contains("_tsx_")),
            "build junk leaked into results: {hits:?}"
        );
    }

    #[test]
    fn generic_build_dir_pruned_only_with_manifest() {
        // No manifest beside `build`: a user's real folder stays searchable.
        let keep = TempTree::new("keep");
        keep.write("build/house-plan.md", "x");
        let idx1 = FileIndex::new();
        idx1.ensure_fresh(&keep.scoped());
        assert_eq!(idx1.search_names("house plan", None, None, 20).len(), 1);

        // package.json beside `dist`: it is a build tree, so prune it.
        let proj = TempTree::new("proj");
        proj.write("package.json", "{}");
        proj.write("dist/bundle.min.js", "x");
        let idx2 = FileIndex::new();
        idx2.ensure_fresh(&proj.scoped());
        assert_eq!(idx2.search_names("bundle", None, None, 20).len(), 0);
    }

    // Benchmark, not a correctness test. Run with:
    //   cargo test --lib bench_index_vs_naive_walk -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark; run with --ignored --nocapture"]
    fn bench_index_vs_naive_walk() {
        use std::time::Instant;

        let tree = TempTree::new("bench");
        // A realistic-ish tree: source files plus a big node_modules of junk
        // (which the old code walked on every query and the index prunes).
        for d in 0..20 {
            for f in 0..50 {
                tree.write(&format!("src/mod{d}/file{f}.rs"), "content");
            }
        }
        for d in 0..40 {
            for f in 0..50 {
                tree.write(&format!("node_modules/pkg{d}/file{f}.js"), "junk");
            }
        }
        let indexed_files = 20 * 50; // node_modules is pruned from the index
        let scoped = tree.scoped();
        let queries = ["file3", "mod1", "file49", "file0", "mod19"];
        let rounds = 20u32;
        let n = rounds * queries.len() as u32;

        // Baseline: a naive walkdir per query over the whole tree (node_modules
        // included), matching the pre-index behavior.
        let t0 = Instant::now();
        for _ in 0..rounds {
            for q in &queries {
                let mut hits = 0usize;
                for entry in walkdir::WalkDir::new(&scoped[0]).into_iter().flatten() {
                    if entry.file_type().is_file() {
                        let name = entry.file_name().to_string_lossy().to_lowercase();
                        if name.contains(q) {
                            hits += 1;
                        }
                    }
                }
                std::hint::black_box(hits);
            }
        }
        let naive = t0.elapsed();

        // Indexed: build once (cold), then cached queries.
        let index = FileIndex::new();
        let tb = Instant::now();
        index.ensure_fresh(&scoped);
        let build = tb.elapsed();
        let tq = Instant::now();
        for _ in 0..rounds {
            for q in &queries {
                std::hint::black_box(index.search_names(q, None, None, 20).len());
            }
        }
        let cached = tq.elapsed();

        let per_naive = naive / n;
        let per_cached = cached / n;
        let speedup = per_naive.as_secs_f64() / per_cached.as_secs_f64().max(f64::MIN_POSITIVE);
        println!("\n=== FileIndex benchmark ===");
        println!("indexed files       : {indexed_files} (+2000 node_modules pruned)");
        println!("naive walk / query  : {per_naive:?}  (total {naive:?})");
        println!("index build (cold)  : {build:?}");
        println!("cached query        : {per_cached:?}  (total {cached:?})");
        println!("per-query speedup   : {speedup:.0}x");
    }
}
