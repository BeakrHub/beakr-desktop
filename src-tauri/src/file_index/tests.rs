//! Tests for the file index: building, searching, readiness and live updates.

use super::store::{DirEntry, DirFiles, IndexState};
use super::*;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;
use std::time::{Duration, SystemTime};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TempTree {
    root: PathBuf,
}

impl TempTree {
    fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root =
            env::temp_dir().join(format!("beakr_index_test_{tag}_{}_{n}", std::process::id()));
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
    assert!(
        hits[0].path.ends_with(Path::new("src").join("index.js")),
        "got {paths:?}"
    );
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

    // ENG-1960: Windows may preserve the directory mtime across these
    // rapid mutations; a dirty refresh still has to reconcile the tree.
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
fn matches_folder_names_in_the_path() {
    let tree = TempTree::new("pathmatch");
    tree.write("lib/hooks/use-thing-swr.ts", "x");
    tree.write("lib/utils/unrelated.ts", "x");
    let index = FileIndex::new();
    index.refresh(&tree.scoped());

    // "hooks" is only in the folder and "swr" only in the filename — a
    // basename-only match would miss this; path-aware matching finds it.
    let hits: Vec<String> = index
        .search_names("swr hooks", None, None, 20)
        .iter()
        .map(|f| f.name.clone())
        .collect();
    assert!(
        hits.contains(&"use-thing-swr.ts".to_string()),
        "path-aware match failed: {hits:?}"
    );
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
    index.refresh(&tree.scoped());

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
    idx1.refresh(&keep.scoped());
    assert_eq!(idx1.search_names("house plan", None, None, 20).len(), 1);

    // package.json beside `dist`: it is a build tree, so prune it.
    let proj = TempTree::new("proj");
    proj.write("package.json", "{}");
    proj.write("dist/bundle.min.js", "x");
    let idx2 = FileIndex::new();
    idx2.refresh(&proj.scoped());
    assert_eq!(idx2.search_names("bundle", None, None, 20).len(), 0);
}

// Benchmark, not a correctness test. Run with:
//   cargo test --lib bench_index_vs_naive_walk -- --ignored --nocapture
#[test]
#[ignore = "benchmark; run with --ignored --nocapture"]
fn bench_index_vs_naive_walk() {
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
    index.refresh(&scoped);
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

const FIRST_SCAN_WAIT: Duration = Duration::from_secs(10);
const CHANGE_WAIT: Duration = Duration::from_secs(5);

#[test]
fn first_search_builds_the_index_in_the_background_and_answers() {
    let tree = TempTree::new("prepare_first");
    tree.write("notes/harvey_trace_package.zip", "x");
    let index = Arc::new(FileIndex::new());

    let readiness = index.prepare_search(&tree.scoped(), FIRST_SCAN_WAIT, CHANGE_WAIT);

    assert_eq!(readiness, IndexReadiness::Ready);
    assert_eq!(index.search_names("harvey trace", None, None, 20).len(), 1);
}

#[test]
fn a_search_during_an_unfinished_first_scan_answers_within_its_bound() {
    // Regression (2026-11-10): the first search after launch walked every
    // shared folder inside the request for 2.5 minutes, past the engine's
    // 30-second wait, so the agent got no answer at all. While a first scan
    // is still running, a search must come back within its bound.
    let tree = TempTree::new("prepare_busy");
    tree.write("a.txt", "x");
    let index = Arc::new(FileIndex::new());
    {
        let mut work = index.work.lock().unwrap();
        work.scanning = Some(tree.scoped()); // a slow first scan is running
    }

    let started = Instant::now();
    let readiness = index.prepare_search(&tree.scoped(), Duration::from_millis(100), CHANGE_WAIT);

    assert_eq!(readiness, IndexReadiness::StillIndexing);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn a_built_index_answers_at_once_while_a_full_rescan_runs() {
    // Regression (2026-11-10, 0.6.1): once built, a search still re-checked
    // every folder whenever anything had changed (33 seconds over 4.7 million
    // files) and waited up to 20 seconds for it. A rescan must never hold up
    // a search over an index that already answers.
    let tree = TempTree::new("rescan_running");
    tree.write("kept.txt", "x");
    let index = Arc::new(FileIndex::new());
    index.refresh(&tree.scoped());
    {
        let mut work = index.work.lock().unwrap();
        work.scanning = Some(tree.scoped()); // a slow safety-net rescan is running
        work.full_scan = Some(tree.scoped()); // and another is queued
    }

    let started = Instant::now();
    let readiness = index.prepare_search(&tree.scoped(), FIRST_SCAN_WAIT, CHANGE_WAIT);

    assert_eq!(readiness, IndexReadiness::Ready);
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "waited {:?} on a rescan",
        started.elapsed()
    );
    assert_eq!(index.search_names("kept", None, None, 20).len(), 1);
}

#[test]
fn changes_queued_behind_a_running_rescan_do_not_delay_a_search() {
    // They cannot be applied until the rescan ends, so waiting for them would
    // only add the full change wait to every search during a rescan.
    let tree = TempTree::new("changes_behind_scan");
    tree.write("kept.txt", "x");
    let index = Arc::new(FileIndex::new());
    index.refresh(&tree.scoped());
    {
        let mut work = index.work.lock().unwrap();
        work.scanning = Some(tree.scoped());
        work.changed_dirs.insert(tree.root.clone());
    }

    let started = Instant::now();
    index.prepare_search(&tree.scoped(), FIRST_SCAN_WAIT, CHANGE_WAIT);

    assert!(started.elapsed() < Duration::from_millis(500));
}

#[test]
fn a_file_the_watcher_reports_is_found_by_the_next_search() {
    let tree = TempTree::new("watch_new_file");
    tree.write("docs/old.txt", "x");
    let index = Arc::new(FileIndex::new());
    assert_eq!(
        index.prepare_search(&tree.scoped(), FIRST_SCAN_WAIT, CHANGE_WAIT),
        IndexReadiness::Ready
    );

    tree.write("docs/brand_new_report.txt", "x");
    index.note_changed_paths([tree.root.join("docs/brand_new_report.txt")]);

    assert_eq!(
        index.prepare_search(&tree.scoped(), FIRST_SCAN_WAIT, CHANGE_WAIT),
        IndexReadiness::Ready
    );
    assert_eq!(
        index.search_names("brand new report", None, None, 20).len(),
        1
    );
}

#[test]
fn watcher_events_inside_skipped_folders_queue_nothing() {
    let tree = TempTree::new("watch_noise");
    tree.write("src/main.rs", "x");
    tree.write("node_modules/pkg/index.js", "x");
    let index = Arc::new(FileIndex::new());
    index.refresh(&tree.scoped());

    index.note_changed_paths([
        tree.root.join("node_modules/pkg/index.js"),
        tree.root.join("node_modules/pkg"),
        PathBuf::from("/somewhere/else/entirely.txt"),
    ]);

    let work = index.work.lock().unwrap();
    assert!(work.changed_dirs.is_empty() && work.full_scan.is_none());
    assert!(!work.worker_started, "noise must not even start the thread");
}

#[cfg(unix)]
#[test]
fn watcher_paths_under_a_symlinked_shared_folder_are_mapped_back() {
    // Regression (2026-11-10): ~/Dropbox is a symlink to
    // ~/Library/CloudStorage/Dropbox on current macOS, and the watcher reports
    // the real path. Unmapped, every change there was dropped as "not indexed"
    // and waited for the safety-net rescan.
    let tree = TempTree::new("symlinked_root");
    tree.write("real/docs/a.txt", "x");
    let link = tree.root.join("shared_link");
    std::os::unix::fs::symlink(tree.root.join("real"), &link).unwrap();
    let roots = vec![link.display().to_string()];
    let index = Arc::new(FileIndex::new());
    index.refresh(&roots);

    let real_docs = fs::canonicalize(tree.root.join("real/docs")).unwrap();
    index.note_changed_paths([real_docs.join("b.txt")]);

    let work = index.work.lock().unwrap();
    assert!(
        work.changed_dirs.contains(&link.join("docs")),
        "queued {:?}",
        work.changed_dirs
    );
}

#[cfg(unix)]
#[test]
fn an_indexed_folder_replaced_by_a_symlink_is_not_followed() {
    // A folder indexed as a real directory can later become a symlink to
    // somewhere outside the shared folders. Re-reading it by path would list
    // outside names in search results, so it must drop out instead.
    let shared = TempTree::new("swap_shared");
    let outside = TempTree::new("swap_outside");
    shared.write("projects/plan.md", "x");
    outside.write("secret_outside_scope.txt", "x");
    let index = Arc::new(FileIndex::new());
    index.refresh(&shared.scoped());

    fs::remove_dir_all(shared.root.join("projects")).unwrap();
    std::os::unix::fs::symlink(&outside.root, shared.root.join("projects")).unwrap();

    // Both update paths: the watcher's targeted re-read, and a full rescan.
    let changed: HashSet<PathBuf> = [shared.root.join("projects"), shared.root.clone()]
        .into_iter()
        .collect();
    index.apply_changes(&changed);
    assert!(index
        .search_names("secret outside", None, None, 20)
        .is_empty());
    index.refresh(&shared.scoped());
    assert!(index
        .search_names("secret outside", None, None, 20)
        .is_empty());
}

#[cfg(unix)]
#[test]
fn a_folder_whose_parent_became_a_symlink_is_not_followed_by_a_watcher_update() {
    // Second half of the symlink check: only the parent was swapped, so the
    // re-read folder itself is a real directory, just outside the shared
    // folders now.
    let shared = TempTree::new("parent_swap_shared");
    let outside = TempTree::new("parent_swap_outside");
    shared.write("a/b/plan.md", "x");
    outside.write("b/secret_outside_scope.txt", "x");
    let index = Arc::new(FileIndex::new());
    index.refresh(&shared.scoped());

    fs::remove_dir_all(shared.root.join("a")).unwrap();
    std::os::unix::fs::symlink(&outside.root, shared.root.join("a")).unwrap();
    let changed: HashSet<PathBuf> = [shared.root.join("a/b")].into_iter().collect();
    index.apply_changes(&changed);

    assert!(index
        .search_names("secret outside", None, None, 20)
        .is_empty());
    assert!(!index
        .snapshot()
        .dirs
        .contains_key(shared.root.join("a/b").as_path()));
}

#[test]
fn applying_changes_adds_new_folders_and_drops_deleted_ones() {
    let tree = TempTree::new("apply_changes");
    tree.write("keep/a.txt", "x");
    tree.write("gone/b.txt", "x");
    tree.write("gone/deeper/c.txt", "x");
    let index = FileIndex::new();
    index.refresh(&tree.scoped());

    fs::remove_dir_all(tree.root.join("gone")).unwrap();
    tree.write("fresh/nested/d_new_file.txt", "x");
    fs::remove_file(tree.root.join("keep/a.txt")).unwrap();
    let changed: HashSet<PathBuf> = [
        tree.root.clone(),
        tree.root.join("gone"),
        tree.root.join("keep"),
    ]
    .into_iter()
    .collect();
    index.apply_changes(&changed);

    assert_eq!(index.search_names("d_new_file", None, None, 20).len(), 1);
    assert_eq!(index.search_names("c.txt", None, None, 20).len(), 0);
    assert_eq!(index.search_names("a.txt", None, None, 20).len(), 0);
    let state = index.snapshot();
    assert!(!state
        .dirs
        .contains_key(tree.root.join("gone/deeper").as_path()));
    assert!(state
        .dirs
        .contains_key(tree.root.join("fresh/nested").as_path()));
}

#[test]
fn a_rescan_shares_unchanged_folders_instead_of_copying_them() {
    // Regression (2026-11-10): every refresh deep-copied the whole index
    // before walking, so memory went from 1.4 GB to 4.3 GB on 4.7 million
    // files. An unchanged folder's list must be shared with the new snapshot.
    if cfg!(windows) {
        return; // Windows re-reads every folder on a rescan (ENG-1960)
    }
    let tree = TempTree::new("share");
    tree.write("stable/a.txt", "x");
    let index = FileIndex::new();
    index.refresh(&tree.scoped());
    let before = index.snapshot();

    index.refresh(&tree.scoped());
    let after = index.snapshot();

    let dir = tree.root.join("stable");
    assert!(Arc::ptr_eq(
        &before.dirs[dir.as_path()].files,
        &after.dirs[dir.as_path()].files
    ));
}

#[test]
fn root_filter_naming_a_single_file_returns_only_that_file() {
    let tree = TempTree::new("file_filter");
    tree.write("reports/q3.pdf", "x");
    tree.write("reports/q4.pdf", "x");
    let index = FileIndex::new();
    index.refresh(&tree.scoped());

    let only = tree.root.join("reports/q3.pdf");
    let hits = index.search_names("pdf", Some(&only), None, 20);
    assert_eq!(found_names(&hits), vec!["q3.pdf"]);
}

/// An in-memory snapshot of `dirs` folders with `per_dir` files each, for
/// search tests too large to write to disk.
fn synthetic_state(dirs: usize, per_dir: usize) -> IndexState {
    let root = PathBuf::from("/synthetic");
    let mut state = IndexState {
        roots: vec![root.display().to_string()],
        ..Default::default()
    };
    assert!(state.real_roots.is_empty() && state.canonical_roots.is_empty());
    for d in 0..dirs {
        let files = (0..per_dir)
            .map(|f| (format!("file_{d}_{f}.txt"), (d * per_dir + f) as i64))
            .collect();
        state.dirs.insert(
            Arc::from(root.join(format!("dir{d}"))),
            DirEntry {
                mtime: SystemTime::UNIX_EPOCH,
                subdirs: Arc::from(Vec::new()),
                files: Arc::new(DirFiles::from_entries(files)),
            },
        );
    }
    state
}

#[test]
fn a_search_split_across_threads_ranks_like_one_thread() {
    // 300k files is past the threshold for several search threads; the top
    // results must not depend on how the folders were split.
    let state = synthetic_state(3_000, 100);
    let reads = HashMap::from([(PathBuf::from("/synthetic/dir7/file_7_3.txt"), 2u32)]);

    let threaded = search::search(&state, &reads, "file_7", None, None, 25);
    let single = {
        let dirs: Vec<_> = state.dirs.iter().collect();
        let pattern = nucleo_matcher::pattern::Pattern::parse(
            "file_7",
            nucleo_matcher::pattern::CaseMatching::Ignore,
            nucleo_matcher::pattern::Normalization::Smart,
        );
        search::search_with_threads(&state, &reads, &pattern, &dirs, None, None, 25, 1)
    };

    let names = |hits: &[FileMeta]| hits.iter().map(|h| h.path.clone()).collect::<Vec<_>>();
    assert_eq!(threaded.len(), 25);
    assert_eq!(names(&threaded), names(&single));
    assert_eq!(
        threaded[0].path,
        PathBuf::from("/synthetic/dir7/file_7_3.txt")
    );
}

/// Resident memory of this process in MiB, read from `ps` (benchmarks only).
fn resident_mib() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .unwrap_or(0)
        / 1024
}

// Benchmark over real folders, not a correctness test. Run with:
//   BEAKR_INDEX_BENCH_ROOTS=~/Projects:~/Downloads \
//     cargo test --release --lib bench_index_real_folders -- --ignored --nocapture
#[test]
#[ignore = "benchmark over real folders; set BEAKR_INDEX_BENCH_ROOTS"]
fn bench_index_real_folders() {
    let Ok(raw) = env::var("BEAKR_INDEX_BENCH_ROOTS") else {
        println!("BEAKR_INDEX_BENCH_ROOTS not set; skipping");
        return;
    };
    let roots: Vec<String> = raw.split(':').map(str::to_string).collect();
    let index = FileIndex::new();
    let base = resident_mib();

    let t = Instant::now();
    index.refresh(&roots);
    let build = t.elapsed();
    let after_build = resident_mib();

    let t = Instant::now();
    index.refresh(&roots);
    let refresh = t.elapsed();
    let after_refresh = resident_mib();

    let queries = [
        "harvey_low_effort_trace_package",
        "readme",
        "chat inpt",
        "zzqqxx",
    ];
    let mut per_query = Vec::new();
    for q in queries {
        let t = Instant::now();
        let hits = index.search_names(q, None, None, 20).len();
        per_query.push(format!("{q:?}: {:?} ({hits} hits)", t.elapsed()));
    }

    println!("\n=== FileIndex over {roots:?} ===");
    println!("files indexed        : {}", index.file_count());
    println!("cold build           : {build:?}");
    println!("refresh, no changes  : {refresh:?}");
    println!("memory (RSS)         : base {base} MiB, built {after_build} MiB, after refresh {after_refresh} MiB");
    for line in per_query {
        println!("search {line}");
    }
}
