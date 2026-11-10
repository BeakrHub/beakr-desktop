//! In-memory index of the files in the user's shared (scoped) folders.
//!
//! The index holds each file's name and modification time, grouped by
//! directory, so a filename/path search answers from memory instead of walking
//! the disk. It never holds file contents and lives only in this process: it
//! is rebuilt at every launch and never written to disk or sent anywhere.
//!
//! How it stays current:
//! - A full scan runs in the background at launch, whenever the shared folders
//!   change, and on a slow safety-net timer (`file_watch`). It reuses every
//!   directory whose modification time has not changed.
//! - The filesystem watcher reports changed paths; only those directories are
//!   re-read, usually within a second.
//! - All of that happens on one background thread ([`FileIndex::run_worker`]).
//!   A search never waits on a full scan once the index is built: it answers
//!   from the current snapshot, waiting at most briefly for pending watcher
//!   changes so a file saved a moment ago is found.
//!
//! Before 2026-11-11 a search refreshed the index itself whenever anything had
//! changed, which on 4.7 million files meant a 33-second re-check (and a
//! 4.3 GB peak) before most answers, and a 2.5-minute walk on the first search
//! after launch that disconnected the app.

mod scan;
mod search;
mod store;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};

use store::IndexState;

/// One file a search returned.
#[derive(Clone, Debug)]
pub struct FileMeta {
    pub path: PathBuf,
    pub name: String,
}

/// Whether a search can be answered from the index.
#[derive(Debug, PartialEq, Eq)]
pub enum IndexReadiness {
    /// The index covers the requested folders.
    Ready,
    /// The first scan of these folders is still running.
    StillIndexing,
}

/// Wait after the first queued change before applying it, so a burst (a git
/// checkout, an unzip) becomes one batch instead of hundreds.
const CHANGE_DEBOUNCE: Duration = Duration::from_millis(250);

/// Past this many queued directories, one full scan is cheaper than re-reading
/// each of them.
const MAX_QUEUED_DIRS: usize = 20_000;

/// Work waiting for the background thread.
#[derive(Default)]
struct WorkQueue {
    /// A full scan of these roots is wanted.
    full_scan: Option<Vec<String>>,
    /// Directories the watcher reported as changed.
    changed_dirs: HashSet<PathBuf>,
    /// The thread is re-reading directories taken from `changed_dirs`.
    applying_changes: bool,
    /// The roots of the full scan running now, if one is.
    scanning: Option<Vec<String>>,
    /// The background thread has been started.
    worker_started: bool,
}

impl WorkQueue {
    /// Whether a full scan of `roots` is queued or running.
    fn scan_coming(&self, roots: &[String]) -> bool {
        self.full_scan.as_deref() == Some(roots) || self.scanning.as_deref() == Some(roots)
    }
}

/// Thread-safe index over the scoped folders.
pub struct FileIndex {
    /// The current snapshot. Searches clone the `Arc` and release the lock, so
    /// a scan swapping in a new snapshot never waits for a search, or the
    /// reverse.
    state: RwLock<Arc<IndexState>>,
    /// How many times each path has been read via the `read_file` tool. A
    /// frecency signal that boosts files the agent keeps returning to.
    access: Mutex<HashMap<PathBuf, u32>>,
    work: Mutex<WorkQueue>,
    /// Signalled when work is queued and when a batch finishes.
    work_changed: Condvar,
}

impl Default for FileIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl FileIndex {
    pub fn new() -> Self {
        Self {
            state: RwLock::new(Arc::new(IndexState::default())),
            access: Mutex::new(HashMap::new()),
            work: Mutex::new(WorkQueue::default()),
            work_changed: Condvar::new(),
        }
    }

    fn snapshot(&self) -> Arc<IndexState> {
        Arc::clone(&self.state.read().unwrap())
    }

    /// Record that `path` was read, boosting its future ranking (frecency).
    pub fn record_access(&self, path: &Path) {
        *self
            .access
            .lock()
            .unwrap()
            .entry(path.to_path_buf())
            .or_insert(0) += 1;
    }

    /// Whether the current snapshot was built for exactly `roots`.
    pub fn is_built_for(&self, roots: &[String]) -> bool {
        self.state.read().unwrap().roots == roots
    }

    /// Ask the background thread for a full scan of `roots`: at launch, when
    /// the shared folders change, on the safety-net timer, and when the watcher
    /// reports it lost events.
    pub fn request_full_scan(self: &Arc<Self>, roots: &[String]) {
        let mut work = self.work.lock().unwrap();
        work.full_scan = Some(roots.to_vec());
        self.wake_worker(&mut work);
    }

    /// Queue the directories affected by changes at `paths` (each path and its
    /// parent). Paths outside the indexed directories (inside a skipped folder,
    /// or outside the shared folders) are dropped here, so watcher noise from
    /// `node_modules` or `.git` never wakes the background thread.
    pub fn note_changed_paths(self: &Arc<Self>, paths: impl IntoIterator<Item = PathBuf>) {
        let snapshot = self.snapshot();
        let mut relevant = Vec::new();
        for path in paths {
            let path = snapshot.as_indexed(path);
            if let Some(parent) = path.parent() {
                if snapshot.dirs.contains_key(parent) {
                    relevant.push(parent.to_path_buf());
                }
            }
            if snapshot.dirs.contains_key(path.as_path()) {
                relevant.push(path);
            }
        }
        if relevant.is_empty() {
            return;
        }
        let mut work = self.work.lock().unwrap();
        work.changed_dirs.extend(relevant);
        if work.changed_dirs.len() > MAX_QUEUED_DIRS {
            work.changed_dirs.clear();
            work.full_scan = Some(snapshot.roots.clone());
        }
        self.wake_worker(&mut work);
    }

    /// Start the background thread if needed and tell it there is work.
    fn wake_worker(self: &Arc<Self>, work: &mut WorkQueue) {
        if !work.worker_started {
            let weak = Arc::downgrade(self);
            let spawned = std::thread::Builder::new()
                .name("beakr-file-index".into())
                .spawn(move || Self::run_worker(weak));
            match spawned {
                Ok(_) => work.worker_started = true,
                Err(error) => log::error!("File index: could not start its thread: {error}"),
            }
        }
        self.work_changed.notify_all();
    }

    /// The background thread: wait for work, let a burst settle, apply it.
    /// Holds only a weak reference between batches, so it ends once the index
    /// is dropped (tests create many).
    fn run_worker(index: Weak<Self>) {
        loop {
            let Some(this) = index.upgrade() else {
                return;
            };
            let has_work = {
                let work = this.work.lock().unwrap();
                let (work, _) = this
                    .work_changed
                    .wait_timeout_while(work, Duration::from_secs(1), |w| {
                        w.full_scan.is_none() && w.changed_dirs.is_empty()
                    })
                    .unwrap();
                work.full_scan.is_some() || !work.changed_dirs.is_empty()
            };
            if !has_work {
                continue; // re-check that the index still exists
            }
            std::thread::sleep(CHANGE_DEBOUNCE);
            let (full_scan, changed_dirs) = {
                let mut work = this.work.lock().unwrap();
                let changed_dirs = std::mem::take(&mut work.changed_dirs);
                work.applying_changes = !changed_dirs.is_empty();
                work.scanning = work.full_scan.clone();
                (work.full_scan.take(), changed_dirs)
            };
            // Watcher changes first: they take moments, and a search may be
            // waiting for them; a full scan can take minutes and none waits.
            if !changed_dirs.is_empty() {
                this.run_guarded(|| this.apply_changes(&changed_dirs));
                this.work.lock().unwrap().applying_changes = false;
                this.work_changed.notify_all();
            }
            if let Some(roots) = &full_scan {
                this.run_guarded(|| this.refresh(roots));
                this.work.lock().unwrap().scanning = None;
                this.work_changed.notify_all();
            }
        }
    }

    /// Run one update, logging instead of ending the background thread if it
    /// panics, so a bad directory cannot stop all later updates.
    fn run_guarded(&self, update: impl FnOnce()) {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(update)).is_err() {
            log::error!("File index: an update failed; the next change or scan retries it");
        }
    }

    /// Get the index ready to answer a search over `roots`.
    ///
    /// Once built for `roots` it answers at once, waiting at most
    /// `change_wait` for queued watcher changes so a just-saved file is found.
    /// Before that, it makes sure a scan is coming and waits at most
    /// `first_scan_wait` for it.
    pub fn prepare_search(
        self: &Arc<Self>,
        roots: &[String],
        first_scan_wait: Duration,
        change_wait: Duration,
    ) -> IndexReadiness {
        if self.is_built_for(roots) {
            let deadline = Instant::now() + change_wait;
            let work = self.work.lock().unwrap();
            // Only watcher changes are waited for; a full scan pending or
            // running does not delay an index that already answers. Changes
            // queued during a full scan wait for it to end, so they are not
            // waited for either.
            let _settled = self.wait_until(work, deadline, |w| {
                !w.applying_changes && (w.changed_dirs.is_empty() || w.scanning.is_some())
            });
            return IndexReadiness::Ready;
        }
        {
            let mut work = self.work.lock().unwrap();
            if !work.scan_coming(roots) {
                work.full_scan = Some(roots.to_vec());
                self.wake_worker(&mut work);
            }
        }
        let deadline = Instant::now() + first_scan_wait;
        let mut work = self.work.lock().unwrap();
        while !self.is_built_for(roots) {
            let now = Instant::now();
            if now >= deadline {
                return IndexReadiness::StillIndexing;
            }
            work = self
                .work_changed
                .wait_timeout(work, deadline - now)
                .unwrap()
                .0;
        }
        IndexReadiness::Ready
    }

    /// Wait on the queue until `done` holds or `deadline` passes.
    fn wait_until(
        &self,
        mut work: std::sync::MutexGuard<'_, WorkQueue>,
        deadline: Instant,
        done: impl Fn(&WorkQueue) -> bool,
    ) -> bool {
        while !done(&work) {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            work = self
                .work_changed
                .wait_timeout(work, deadline - now)
                .unwrap()
                .0;
        }
        true
    }

    /// Scan `roots` now, on the calling thread, sharing unchanged directories
    /// with the current snapshot. The background thread's full scan; tests
    /// call it directly.
    pub fn refresh(&self, roots: &[String]) {
        let started = Instant::now();
        let old = self.snapshot();
        let next = scan::full_scan(roots, &old);
        drop(old);
        let (files, dirs) = (next.file_count(), next.dirs.len());
        *self.state.write().unwrap() = Arc::new(next);
        log::info!(
            "File index: {files} files in {dirs} folders, scanned in {:.1}s",
            started.elapsed().as_secs_f64()
        );
    }

    /// Re-read the `changed` directories now, on the calling thread. The
    /// background thread's watcher path; tests call it directly.
    pub fn apply_changes(&self, changed: &HashSet<PathBuf>) {
        let current = self.snapshot();
        let changes = scan::rescan_dirs(changed, &current);
        drop(current); // so the write below can update in place, not copy
        let mut guard = self.state.write().unwrap();
        scan::apply(Arc::make_mut(&mut guard), changes);
    }

    /// Ranked filename/path search over the current snapshot. `root_filter`,
    /// when set, restricts to a subtree (the tool's optional `path`). Returns
    /// at most `limit` hits, best first.
    pub fn search_names(
        &self,
        query: &str,
        root_filter: Option<&Path>,
        file_types: Option<&[String]>,
        limit: usize,
    ) -> Vec<FileMeta> {
        let snapshot = self.snapshot();
        let reads = self.access.lock().unwrap().clone();
        search::search(&snapshot, &reads, query, root_filter, file_types, limit)
    }

    /// How many files the current snapshot holds.
    #[cfg(test)]
    pub fn file_count(&self) -> usize {
        self.snapshot().file_count()
    }
}
