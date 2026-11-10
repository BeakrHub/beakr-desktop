//! Ranked fuzzy filename/path search over an [`IndexState`].
//!
//! Matching is fuzzy (subsequence, whitespace-split, case-insensitive) via
//! `nucleo`, so `chat inpt` finds `chat-input.tsx`, and it runs on the path
//! relative to the shared folder, so `swr hooks` finds `lib/hooks/use-swr.ts`.
//! Results rank by match quality, then frecency (how often the agent has read
//! the file), then recency, then name.
//!
//! Over millions of files the first version took 1.4 to 4.9 seconds a query: it
//! allocated two strings per file and kept every match. This one builds each
//! directory's relative prefix once, reuses one buffer per thread, splits the
//! directories across threads, and keeps only each thread's best `limit`.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf, MAIN_SEPARATOR};
use std::sync::Arc;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use super::store::{DirEntry, IndexState};
use super::FileMeta;
use crate::unicode;

/// Below this many files a single thread is faster than starting more.
const FILES_PER_THREAD_MIN: usize = 100_000;

/// One match, borrowing from the snapshot until the final cut.
struct Hit<'a> {
    score: u32,
    reads: u32,
    modified: i64,
    name: &'a str,
    dir: &'a Arc<Path>,
}

/// Best first: match quality, then reads, then newest, then name.
fn rank(a: &Hit, b: &Hit) -> Ordering {
    b.score
        .cmp(&a.score)
        .then(b.reads.cmp(&a.reads))
        .then(b.modified.cmp(&a.modified))
        .then(a.name.cmp(b.name))
}

/// Cut `hits` to its best `limit`. Called whenever a thread's list grows well
/// past `limit`, so a query matching millions of files never holds them all.
fn trim(hits: &mut Vec<Hit>, limit: usize) {
    hits.sort_unstable_by(rank);
    hits.truncate(limit);
}

/// The scoped root containing `dir`, as the prefix fuzzy matching sees.
fn relative_prefix(dir: &Path, roots: &[String]) -> String {
    for root in roots {
        if let Ok(rest) = dir.strip_prefix(root) {
            let rest = rest.to_string_lossy();
            return if rest.is_ascii() {
                rest.into_owned()
            } else {
                unicode::normalize_whitespace(&rest)
            };
        }
    }
    String::new()
}

/// Whether a file `name` in `dir` falls under `root` (the tool's optional
/// `path` argument, normally a folder, possibly a single file).
fn under_root(dir: &Path, name: &str, root: &Path) -> bool {
    dir.starts_with(root) || (root.parent() == Some(dir) && root.file_name() == Some(name.as_ref()))
}

/// Whether `name`'s extension is one of `types` (compared case-insensitively).
fn has_type(name: &str, types: &[String]) -> bool {
    let ext = Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy())
        .unwrap_or_default();
    types.iter().any(|t| t.eq_ignore_ascii_case(&ext))
}

struct Query<'q> {
    pattern: &'q Pattern,
    roots: &'q [String],
    root_filter: Option<&'q Path>,
    file_types: Option<&'q [String]>,
    /// Read counts grouped by directory, so a match only looks up its name.
    reads_by_dir: &'q HashMap<&'q Path, Vec<(&'q str, u32)>>,
    limit: usize,
}

/// Search one slice of the directories on the current thread.
fn search_dirs<'a>(dirs: &[(&'a Arc<Path>, &'a DirEntry)], query: &Query) -> Vec<Hit<'a>> {
    // match_paths tunes scoring for path-shaped text (a bonus after separators
    // and for the last segment), so a filename match beats a folder-only one.
    let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
    let mut chars = Vec::new();
    let mut haystack = String::new();
    let mut hits: Vec<Hit<'a>> = Vec::new();
    for &(dir, entry) in dirs {
        let filter = query.root_filter.filter(|root| !dir.starts_with(root));
        if let Some(root) = filter {
            if root.parent() != Some(dir.as_ref()) {
                continue; // nothing in this directory can be under the filter
            }
        }
        let prefix = relative_prefix(dir, query.roots);
        let reads = query.reads_by_dir.get(dir.as_ref());
        for (name, modified) in entry.files.iter() {
            if let Some(root) = filter {
                if !under_root(dir, name, root) {
                    continue;
                }
            }
            if let Some(types) = query.file_types {
                if !has_type(name, types) {
                    continue;
                }
            }
            haystack.clear();
            if !prefix.is_empty() {
                haystack.push_str(&prefix);
                haystack.push(MAIN_SEPARATOR);
            }
            if name.is_ascii() {
                haystack.push_str(name);
            } else {
                haystack.push_str(&unicode::normalize_whitespace(name));
            }
            let Some(score) = query
                .pattern
                .score(Utf32Str::new(&haystack, &mut chars), &mut matcher)
            else {
                continue;
            };
            let reads = reads
                .and_then(|names| names.iter().find(|(n, _)| *n == name))
                .map_or(0, |(_, count)| *count);
            hits.push(Hit {
                score,
                reads,
                modified,
                name,
                dir,
            });
            if hits.len() >= query.limit.max(64) * 4 {
                trim(&mut hits, query.limit);
            }
        }
    }
    trim(&mut hits, query.limit);
    hits
}

/// Split `dirs` into about `parts` slices holding similar numbers of files.
fn balanced_slices<'s, 'a>(
    dirs: &'s [(&'a Arc<Path>, &'a DirEntry)],
    parts: usize,
) -> Vec<&'s [(&'a Arc<Path>, &'a DirEntry)]> {
    let total: usize = dirs.iter().map(|(_, e)| e.files.len()).sum();
    let target = total.div_ceil(parts.max(1)).max(1);
    let mut slices = Vec::with_capacity(parts);
    let mut start = 0;
    let mut filled = 0;
    for (i, (_, entry)) in dirs.iter().enumerate() {
        filled += entry.files.len();
        if filled >= target {
            slices.push(&dirs[start..=i]);
            start = i + 1;
            filled = 0;
        }
    }
    if start < dirs.len() {
        slices.push(&dirs[start..]);
    }
    slices
}

/// Ranked search over `state`. Returns at most `limit` files, best first.
pub(super) fn search(
    state: &IndexState,
    reads: &HashMap<PathBuf, u32>,
    query: &str,
    root_filter: Option<&Path>,
    file_types: Option<&[String]>,
    limit: usize,
) -> Vec<FileMeta> {
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let dirs: Vec<(&Arc<Path>, &DirEntry)> = state.dirs.iter().collect();
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(8)
        .min(state.file_count() / FILES_PER_THREAD_MIN)
        .max(1);
    search_with_threads(
        state,
        reads,
        &pattern,
        &dirs,
        root_filter,
        file_types,
        limit,
        threads,
    )
}

/// [`search`] over `dirs` with a fixed number of threads.
#[allow(clippy::too_many_arguments)]
pub(super) fn search_with_threads(
    state: &IndexState,
    reads: &HashMap<PathBuf, u32>,
    pattern: &Pattern,
    dirs: &[(&Arc<Path>, &DirEntry)],
    root_filter: Option<&Path>,
    file_types: Option<&[String]>,
    limit: usize,
    threads: usize,
) -> Vec<FileMeta> {
    if limit == 0 {
        return Vec::new();
    }
    let mut reads_by_dir: HashMap<&Path, Vec<(&str, u32)>> = HashMap::new();
    for (path, count) in reads {
        if let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str()))
        {
            reads_by_dir.entry(dir).or_default().push((name, *count));
        }
    }
    let query = Query {
        pattern,
        roots: &state.roots,
        root_filter,
        file_types,
        reads_by_dir: &reads_by_dir,
        limit,
    };

    let mut hits = if threads <= 1 {
        search_dirs(dirs, &query)
    } else {
        std::thread::scope(|scope| {
            let workers: Vec<_> = balanced_slices(dirs, threads)
                .into_iter()
                .map(|slice| scope.spawn(|| search_dirs(slice, &query)))
                .collect();
            workers
                .into_iter()
                .flat_map(|w| w.join().expect("index search thread panicked"))
                .collect()
        })
    };
    trim(&mut hits, limit);
    hits.into_iter()
        .map(|hit| FileMeta {
            path: hit.dir.join(hit.name),
            name: hit.name.to_string(),
        })
        .collect()
}
