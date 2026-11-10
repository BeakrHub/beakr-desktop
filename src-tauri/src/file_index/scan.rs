//! Reading the shared folders from disk into an [`IndexState`].
//!
//! Two operations:
//! - [`full_scan`] walks every root on several threads, reusing the previous
//!   snapshot's lists for any directory whose modification time is unchanged.
//! - [`rescan_dirs`] re-reads only the directories the filesystem watcher
//!   reported, scanning newly created subfolders and dropping deleted ones.
//!
//! Denied directories and files (`security::is_denied`) and build/cache noise
//! (`search_filter::is_excluded_dir`) are never indexed.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::SystemTime;

use super::store::{modified_secs, DirEntry, DirFiles, IndexState};
use crate::{search_filter, security};

/// Threads for a full scan. Directory reads are mostly waiting on the disk, so
/// a few threads overlap that waiting; more would compete with whatever the
/// user is doing for a background job.
fn scan_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get() / 2)
        .unwrap_or(2)
        .clamp(1, 6)
}

/// The modification time of `dir`, or `None` if it should not be read: gone,
/// unreadable, or a symlink that is not itself a shared folder. A folder
/// listed as a real directory can be replaced by a symlink later; following
/// it would index names from outside the shared folders, so a non-root
/// symlink is treated as removed. A shared folder that is a symlink
/// (`~/Dropbox` on current macOS) is followed, as the user chose it.
fn directory_mtime(dir: &Path, is_root: bool) -> Option<SystemTime> {
    if !is_root {
        let link = std::fs::symlink_metadata(dir).ok()?;
        if !link.file_type().is_dir() {
            return None;
        }
    }
    std::fs::metadata(dir).and_then(|m| m.modified()).ok()
}

/// Read one directory: its modification time, the subdirectories worth
/// indexing, and its files. `None` when it should not be read (see
/// [`directory_mtime`]), which drops it from the index.
fn read_directory(dir: &Path, is_root: bool) -> Option<DirEntry> {
    let mtime = directory_mtime(dir, is_root)?;
    let mut files = Vec::new();
    let mut subdirs = Vec::new();
    if let Ok(read) = std::fs::read_dir(dir) {
        for entry in read.flatten() {
            // Symlinks are neither followed nor listed, matching the tools'
            // follow_links(false).
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if security::is_denied(&path) {
                continue;
            }
            if file_type.is_dir() {
                if !search_filter::is_excluded_dir(&path) {
                    subdirs.push(Arc::<Path>::from(path));
                }
            } else if file_type.is_file() {
                let modified = entry.metadata().ok().and_then(|m| m.modified().ok());
                files.push((
                    entry.file_name().to_string_lossy().into_owned(),
                    modified_secs(modified),
                ));
            }
        }
    }
    Some(DirEntry {
        mtime,
        subdirs: subdirs.into(),
        files: Arc::new(DirFiles::from_entries(files)),
    })
}

/// The cached entry for `dir` if the directory has not changed since `old` was
/// taken, read from disk otherwise. `None` if it can no longer be read.
fn reuse_or_read(dir: &Path, is_root: bool, old: &IndexState) -> Option<DirEntry> {
    let mtime = directory_mtime(dir, is_root)?;
    // Windows directory mtimes can stay the same across rapid direct-child
    // additions and removals (ENG-1960), so there a cached list is never
    // trusted and every directory is re-read.
    if !cfg!(windows) {
        if let Some(cached) = old.dirs.get(dir) {
            if cached.mtime == mtime {
                return Some(cached.clone());
            }
        }
    }
    read_directory(dir, is_root)
}

/// Shared work list for the scan threads: directories still to visit, and how
/// many threads are mid-directory (and so may still add more).
struct WorkList {
    pending: Vec<Arc<Path>>,
    active: usize,
}

/// Walk `starts` and everything below them, returning one entry per readable
/// directory. Entries of directories unchanged since `old` are shared, not
/// re-read. `starts_are_roots` lets a start that is a symlink be followed
/// (only shared folders themselves may be).
fn walk(
    starts: Vec<Arc<Path>>,
    starts_are_roots: bool,
    old: &IndexState,
) -> Vec<(Arc<Path>, DirEntry)> {
    let roots: HashSet<Arc<Path>> = if starts_are_roots {
        starts.iter().cloned().collect()
    } else {
        HashSet::new()
    };
    let threads = scan_threads();
    let work = Mutex::new(WorkList {
        pending: starts,
        active: 0,
    });
    let more = Condvar::new();

    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut found = Vec::new();
                    loop {
                        let dir = {
                            let mut list = work.lock().unwrap();
                            loop {
                                if let Some(dir) = list.pending.pop() {
                                    list.active += 1;
                                    break Some(dir);
                                }
                                if list.active == 0 {
                                    break None;
                                }
                                list = more.wait(list).unwrap();
                            }
                        };
                        let Some(dir) = dir else {
                            more.notify_all(); // let the other threads see the end
                            return found;
                        };
                        let entry = reuse_or_read(&dir, roots.contains(&dir), old);
                        let mut list = work.lock().unwrap();
                        if let Some(entry) = &entry {
                            list.pending.extend(entry.subdirs.iter().cloned());
                        }
                        list.active -= 1;
                        drop(list);
                        more.notify_all();
                        if let Some(entry) = entry {
                            found.push((dir, entry));
                        }
                    }
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().expect("index scan thread panicked"))
            .collect()
    })
}

/// Build a fresh snapshot of `roots`, sharing every directory unchanged since
/// `old` (when `old` covers the same roots).
pub(super) fn full_scan(roots: &[String], old: &IndexState) -> IndexState {
    let empty = IndexState::default();
    let reusable = if old.roots == roots { old } else { &empty };
    let starts: Vec<Arc<Path>> = roots
        .iter()
        .map(|root| Arc::<Path>::from(PathBuf::from(root)))
        .filter(|root| !security::is_denied(root)) // never index a denied root
        .collect();
    let dirs: HashMap<Arc<Path>, DirEntry> = walk(starts, true, reusable).into_iter().collect();
    let resolved: Vec<(PathBuf, PathBuf)> = roots
        .iter()
        .filter_map(|root| {
            let shared = PathBuf::from(root);
            Some((std::fs::canonicalize(&shared).ok()?, shared))
        })
        .collect();
    IndexState {
        roots: roots.to_vec(),
        canonical_roots: resolved.iter().map(|(real, _)| real.clone()).collect(),
        real_roots: resolved
            .into_iter()
            .filter(|(real, shared)| real != shared)
            .collect(),
        dirs,
    }
}

/// What re-reading the changed directories found, computed without holding
/// the index lock and applied afterwards with [`apply`].
pub(super) struct Changes {
    /// Directories that no longer exist; their whole subtree goes.
    removed: Vec<Arc<Path>>,
    /// Directories re-read, plus every directory of any newly created subfolder.
    updated: Vec<(Arc<Path>, DirEntry)>,
    /// Subfolders that disappeared from a re-read directory.
    dropped_subdirs: Vec<Arc<Path>>,
}

/// Re-read the `changed` directories that `current` indexes. A path the index
/// does not hold (a file, a new folder, anything inside a skipped folder) is
/// ignored here: its parent is in `changed` too and picks it up.
pub(super) fn rescan_dirs(changed: &HashSet<PathBuf>, current: &IndexState) -> Changes {
    let mut changes = Changes {
        removed: Vec::new(),
        updated: Vec::new(),
        dropped_subdirs: Vec::new(),
    };
    let mut new_subtrees = Vec::new();
    for dir in changed {
        let Some((key, old_entry)) = current.dirs.get_key_value(dir.as_path()) else {
            continue;
        };
        let is_root = current
            .roots
            .iter()
            .any(|root| Path::new(root) == dir.as_path());
        // A full scan walks down from the roots and stops at any symlink, but
        // this re-reads one folder by path, so a parent replaced by a symlink
        // would be followed. Read it only if it still resolves inside a root.
        let inside = is_root
            || std::fs::canonicalize(dir)
                .map(|real| {
                    current
                        .canonical_roots
                        .iter()
                        .any(|root| real.starts_with(root))
                })
                .unwrap_or(false);
        let read = if inside {
            read_directory(dir, is_root)
        } else {
            None
        };
        match read {
            None => changes.removed.push(Arc::clone(key)),
            Some(entry) => {
                let before: HashSet<&Arc<Path>> = old_entry.subdirs.iter().collect();
                let after: HashSet<&Arc<Path>> = entry.subdirs.iter().collect();
                changes
                    .dropped_subdirs
                    .extend(before.difference(&after).map(|d| Arc::clone(d)));
                new_subtrees.extend(
                    after
                        .difference(&before)
                        .filter(|d| !current.dirs.contains_key(d.as_ref()))
                        .map(|d| Arc::clone(d)),
                );
                changes.updated.push((Arc::clone(key), entry));
            }
        }
    }
    if !new_subtrees.is_empty() {
        changes
            .updated
            .extend(walk(new_subtrees, false, &IndexState::default()));
    }
    changes
}

/// Apply [`Changes`] to the live snapshot.
pub(super) fn apply(state: &mut IndexState, changes: Changes) {
    let gone: Vec<&Arc<Path>> = changes
        .removed
        .iter()
        .chain(&changes.dropped_subdirs)
        .collect();
    for dir in &gone {
        state.remove_subtree(dir);
    }
    for (dir, entry) in changes.updated {
        // Never re-add a folder below one this same batch removed.
        if !gone.iter().any(|g| dir.starts_with(g.as_ref())) {
            state.dirs.insert(dir, entry);
        }
    }
}
