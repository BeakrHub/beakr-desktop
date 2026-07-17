//! Background maintenance that keeps the [`FileIndex`] fresh.
//!
//! Two layers, deliberately:
//! 1. A **filesystem watcher** (`notify`) over the scoped folders marks the
//!    index dirty on any change, so the next search re-walks immediately.
//! 2. A **periodic fallback** marks the index dirty on a timer regardless.
//!    Filesystem watchers are lossy — missed events, per-user watch limits,
//!    coalesced/renamed entries — so the timer is the correctness backstop
//!    (the same watch-plus-rescan pattern Spotlight and Everything use).
//!
//! Both layers only ever flip a flag; the flag naturally coalesces bursts of
//! events, so no event debouncing is needed. The actual (incremental) re-walk
//! happens lazily inside `FileIndex::ensure_fresh` at query time.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use crate::file_index::FileIndex;
use crate::state::AppState;

/// How often the fallback marks the index dirty when no watcher event arrives.
const RESCAN_FALLBACK: Duration = Duration::from_secs(30);

/// Start the periodic fallback and the (folder-set-aware) filesystem watcher.
pub fn spawn(state: AppState) {
    // Periodic rescan fallback.
    {
        let index = state.file_index.clone();
        tauri::async_runtime::spawn(async move {
            let mut ticker = tokio::time::interval(RESCAN_FALLBACK);
            // The immediate first tick is harmless (index starts dirty anyway).
            loop {
                ticker.tick().await;
                index.mark_dirty();
            }
        });
    }

    // Filesystem watcher, rebuilt whenever the scoped folders change.
    tauri::async_runtime::spawn(async move {
        loop {
            let folders = state.scoped_folders.read().await.clone();
            // Keep the watcher alive for as long as these roots are current.
            let _watcher = build_watcher(&folders, state.file_index.clone());
            // A new folder set makes these roots (and the watcher) stale.
            state.folders_changed.notified().await;
            state.file_index.mark_dirty();
        }
    });
}

/// Build a recursive watcher over `folders` that marks `index` dirty on any
/// event. Returns the watcher, which must be held alive to keep watching.
fn build_watcher(folders: &[String], index: Arc<FileIndex>) -> Option<RecommendedWatcher> {
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.is_ok() {
            index.mark_dirty();
        }
    })
    .ok()?;

    for folder in folders {
        // Best effort: a folder that can't be watched (permissions, watch-limit
        // exhaustion on deep trees) is still covered by the periodic fallback.
        let _ = watcher.watch(Path::new(folder), RecursiveMode::Recursive);
    }
    Some(watcher)
}
