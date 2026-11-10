//! Background maintenance that keeps the [`FileIndex`] current.
//!
//! - A **filesystem watcher** (`notify`) over the shared folders reports which
//!   paths changed; the index re-reads just those directories, usually within
//!   a second. If the watcher reports that it dropped events, the index gets a
//!   full rescan instead.
//! - A **safety-net timer** asks for a full rescan, because watchers are lossy
//!   (missed events, per-user watch limits, coalesced renames). It is slow (10
//!   minutes) while every folder is watched, and fast (1 minute) when one
//!   could not be.
//! - The folder set changing triggers a full rescan of the new set.
//!
//! The work itself runs on the index's own thread; nothing here blocks.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use crate::file_index::FileIndex;
use crate::state::AppState;

/// How often to rescan everything while the watcher covers every folder.
const FULL_RESCAN_WATCHED: Duration = Duration::from_secs(10 * 60);

/// How often to rescan everything when some folder could not be watched, so
/// its changes still show up within about a minute.
const FULL_RESCAN_UNWATCHED: Duration = Duration::from_secs(60);

/// Keep the index current for whatever folders are shared, for the life of
/// the app.
pub fn spawn(state: AppState) {
    tauri::async_runtime::spawn(async move {
        loop {
            let folders = state.scoped_folders.read().await.clone();
            // Build (or rebuild) the index now, in the background, so the first
            // search after launch or after a folder change finds it ready.
            state.file_index.request_full_scan(&folders);
            let (watcher, all_watched) = build_watcher(&folders, state.file_index.clone());
            let every = if all_watched {
                FULL_RESCAN_WATCHED
            } else {
                FULL_RESCAN_UNWATCHED
            };
            let mut safety_net =
                tokio::time::interval_at(tokio::time::Instant::now() + every, every);
            loop {
                tokio::select! {
                    // Waits on the watcher's own channel; sharing folders_changed
                    // with the WS client starved one of them (ENG-1624).
                    _ = state.watch_folders_changed.notified() => break,
                    _ = safety_net.tick() => state.file_index.request_full_scan(&folders),
                }
            }
            drop(watcher); // these roots are stale; the next pass watches the new set
        }
    });
}

/// Build a recursive watcher over `folders` that tells `index` which paths
/// changed. Returns the watcher, which must be held alive to keep watching,
/// and whether every folder is being watched.
fn build_watcher(folders: &[String], index: Arc<FileIndex>) -> (Option<RecommendedWatcher>, bool) {
    let roots = folders.to_vec();
    let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else {
            return;
        };
        if event.need_rescan() {
            index.request_full_scan(&roots);
        } else {
            index.note_changed_paths(event.paths);
        }
    });
    let mut watcher = match watcher {
        Ok(w) => w,
        Err(error) => {
            log::warn!("File index: no filesystem watcher ({error}); rescanning every minute");
            return (None, false);
        }
    };
    let mut all_watched = true;
    for folder in folders {
        // A folder that can't be watched (permissions, watch-limit exhaustion
        // on deep trees) is still covered by the faster safety net.
        if let Err(error) = watcher.watch(Path::new(folder), RecursiveMode::Recursive) {
            log::warn!(
                "File index: cannot watch a shared folder ({error}); rescanning every minute"
            );
            all_watched = false;
        }
    }
    (Some(watcher), all_watched)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_index::IndexReadiness;
    use std::time::Instant;

    /// End to end through the platform watcher: a file created after the index
    /// is built is found by a search within seconds, with no full rescan. On
    /// macOS the temp folder is reached through a symlink (`/var` ->
    /// `/private/var`) and the watcher reports real paths, so this also covers
    /// shared folders like `~/Dropbox` that are symlinks.
    #[test]
    fn a_file_created_after_the_index_is_built_is_found_within_seconds() {
        let root = std::env::temp_dir().join(format!("beakr_watch_e2e_{}", std::process::id()));
        std::fs::create_dir_all(root.join("inbox")).unwrap();
        let roots = vec![root.display().to_string()];
        let index = Arc::new(FileIndex::new());
        index.refresh(&roots);
        let (watcher, all_watched) = build_watcher(&roots, index.clone());
        assert!(watcher.is_some() && all_watched);
        std::thread::sleep(Duration::from_millis(500)); // let the watch stream start

        std::fs::write(root.join("inbox/quarterly_forecast_v7.xlsx"), "x").unwrap();

        let deadline = Instant::now() + Duration::from_secs(15);
        let mut found = false;
        while Instant::now() < deadline && !found {
            let ready = index.prepare_search(&roots, Duration::ZERO, Duration::from_secs(2));
            assert_eq!(ready, IndexReadiness::Ready);
            found = !index
                .search_names("quarterly_forecast_v7", None, None, 5)
                .is_empty();
            if !found {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        drop(watcher);
        std::fs::remove_dir_all(&root).ok();
        assert!(
            found,
            "the watcher never delivered the new file to the index"
        );
    }
}
