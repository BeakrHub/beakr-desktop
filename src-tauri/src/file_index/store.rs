//! The in-memory shape of the index, kept compact because it holds every file
//! in the shared folders (4.7 million on one user's Mac on 2026-11-10).
//!
//! The first layout stored a full `PathBuf` *and* a `String` name per file and
//! deep-copied the whole snapshot on every refresh: 1.4 GB built, 4.3 GB after
//! one refresh. Here a file is its name bytes plus a 16-byte slot, its path is
//! rebuilt from the directory key only for the few files a search returns, and
//! every per-directory list sits behind an `Arc`, so a refresh shares the
//! directories that did not change instead of copying them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Sentinel for a file whose modification time could not be read. Sorts below
/// every real time, so an unknown time never outranks a known one.
pub(super) const UNKNOWN_MODIFIED: i64 = i64::MIN;

/// Seconds since the Unix epoch (negative before it), or the sentinel.
pub(super) fn modified_secs(time: Option<SystemTime>) -> i64 {
    match time {
        Some(t) => match t.duration_since(UNIX_EPOCH) {
            Ok(after) => i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
            Err(before) => -i64::try_from(before.duration().as_secs()).unwrap_or(i64::MAX),
        },
        None => UNKNOWN_MODIFIED,
    }
}

/// The direct files of one directory: every name in one string, each slot
/// recording where its name ends and when the file was modified.
#[derive(Default)]
pub(super) struct DirFiles {
    names: Box<str>,
    slots: Box<[FileSlot]>,
}

struct FileSlot {
    name_end: u32,
    modified: i64,
}

impl DirFiles {
    pub(super) fn from_entries(entries: Vec<(String, i64)>) -> Self {
        let mut names = String::with_capacity(entries.iter().map(|(n, _)| n.len()).sum());
        let mut slots = Vec::with_capacity(entries.len());
        for (name, modified) in entries {
            names.push_str(&name);
            // A directory's names would need to exceed 4 GiB to overflow; stop
            // adding rather than wrap if that ever happens.
            let Ok(name_end) = u32::try_from(names.len()) else {
                names.truncate(slots.last().map_or(0, |s: &FileSlot| s.name_end as usize));
                break;
            };
            slots.push(FileSlot { name_end, modified });
        }
        Self {
            names: names.into_boxed_str(),
            slots: slots.into_boxed_slice(),
        }
    }

    pub(super) fn len(&self) -> usize {
        self.slots.len()
    }

    /// Each file as `(name, modified seconds)`.
    pub(super) fn iter(&self) -> impl Iterator<Item = (&str, i64)> {
        let mut start = 0usize;
        self.slots.iter().map(move |slot| {
            let end = slot.name_end as usize;
            let name = &self.names[start..end];
            start = end;
            (name, slot.modified)
        })
    }
}

/// One indexed directory. Cloning shares its lists.
#[derive(Clone)]
pub(super) struct DirEntry {
    /// The directory's own modification time when it was read. Adding or
    /// removing a direct child changes it, so an equal time means the cached
    /// lists are still right (except on Windows; see `scan`).
    pub(super) mtime: SystemTime,
    pub(super) subdirs: Arc<[Arc<Path>]>,
    pub(super) files: Arc<DirFiles>,
}

/// A snapshot of every indexed directory under `roots`.
#[derive(Clone, Default)]
pub(super) struct IndexState {
    /// The shared folders this snapshot was built from. A different set means
    /// a full rebuild, never a reuse.
    pub(super) roots: Vec<String>,
    /// `(real path, shared path)` for each root reached through a symlink
    /// (`~/Dropbox` is often one). The watcher reports real paths; these map
    /// them back to the paths the index is keyed by.
    pub(super) real_roots: Vec<(PathBuf, PathBuf)>,
    pub(super) dirs: HashMap<Arc<Path>, DirEntry>,
}

impl IndexState {
    pub(super) fn file_count(&self) -> usize {
        self.dirs.values().map(|d| d.files.len()).sum()
    }

    /// `path` as the index names it: a path under a symlinked root's real
    /// location is rewritten under the shared path.
    pub(super) fn as_indexed(&self, path: PathBuf) -> PathBuf {
        for (real, shared) in &self.real_roots {
            if let Ok(rest) = path.strip_prefix(real) {
                return shared.join(rest);
            }
        }
        path
    }

    /// Remove `dir` and everything indexed below it.
    pub(super) fn remove_subtree(&mut self, dir: &Path) {
        let mut stack = vec![Arc::<Path>::from(dir)];
        while let Some(next) = stack.pop() {
            if let Some(entry) = self.dirs.remove(&next) {
                stack.extend(entry.subdirs.iter().cloned());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_files_round_trip_names_and_times() {
        let files = DirFiles::from_entries(vec![
            ("a.txt".into(), 10),
            ("".into(), UNKNOWN_MODIFIED),
            ("résumé – final.pdf".into(), -5),
        ]);
        let got: Vec<(String, i64)> = files.iter().map(|(n, m)| (n.to_string(), m)).collect();
        assert_eq!(
            got,
            vec![
                ("a.txt".to_string(), 10),
                ("".to_string(), UNKNOWN_MODIFIED),
                ("résumé – final.pdf".to_string(), -5)
            ]
        );
    }

    #[test]
    fn modified_seconds_order_unknown_and_pre_epoch_times_correctly() {
        use std::time::Duration;
        let early = UNIX_EPOCH - Duration::from_secs(86_400);
        let later = UNIX_EPOCH + Duration::from_secs(1_762_800_000);
        assert_eq!(modified_secs(Some(early)), -86_400);
        assert_eq!(modified_secs(Some(later)), 1_762_800_000);
        assert!(modified_secs(None) < modified_secs(Some(early)));
    }
}
