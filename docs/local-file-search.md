# Local file search — design note (ENG-1150)

How the desktop agent finds the user's local files quickly. Built across the
ENG-1150 stack (five PRs). This note records the architecture, the measured
improvement, and the deliberate non-goals.

## The problem

`search_files` (the agent's core file-finding loop) used to walk the whole
scoped folder tree from disk, single-threaded, **on every query**, and for
content searches read every file line by line. It descended into large
excluded directories (`node_modules`, `.git`, `.venv`, …) only to discard each
file, and returned matches in arbitrary order. Every lookup paid the full walk.

## Architecture

Four layers, built bottom-up. For a **bounded set of user-approved folders**
(not the whole disk), the evidence — ripgrep, fd, VS Code — favors a fast
pruned walk plus a lightweight metadata cache over a heavyweight search index.

1. **Parallel, pruned walk** (PR1). Traversal uses BurntSushi's `ignore` crate
   (ripgrep's engine): multi-core, and denied directories are pruned at the
   directory level (`WalkState::Skip`) so we never descend into them.

2. **In-memory metadata cache** (PR2, reworked in 0.6.2). A `FileIndex`
   (`src-tauri/src/file_index/`) on `AppState` holds each file's name and
   modification time, grouped by directory, so a filename/path search answers
   from memory instead of re-walking disk. It holds no file contents and lives
   only in the app process: rebuilt at launch, never written to disk or sent
   anywhere. A file costs its name bytes plus a 16-byte slot; its full path is
   rebuilt from the directory only for the files a search returns. Each
   directory's lists sit behind an `Arc`, so a rescan shares unchanged
   directories with the previous snapshot instead of copying them, and a
   directory whose mtime is unchanged is not re-read (except on Windows, where
   mtimes can miss rapid changes, ENG-1960). Content search still walks + reads
   (it must read file bodies). `list_files` uses the same parallel walker.

3. **Ranking** (PR3). Results are ranked by fuzzy match quality (`nucleo`, the
   Helix matcher — so `chat inpt` finds `chat-input.tsx`), then frecency (how
   often the file has been read via `read_file`), then recency (newest mtime),
   then name. Match quality dominates; the rest break ties.

4. **Watcher + rescan safety net** (PR4, reworked in 0.6.2). All index work
   runs on one background thread (`beakr-file-index`); a search never waits
   for a full scan once the index is built.
   - A `notify` watcher reports changed paths. The index queues each path's
     directory (dropping anything it does not index, such as `node_modules`
     churn), waits 250 ms for a burst to settle, and re-reads only those
     directories, scanning new subfolders and dropping deleted ones. A search
     waits at most 2 s for queued changes, so a just-saved file is found.
   - A full rescan runs at launch, when the shared folders change, when the
     watcher reports dropped events, and on a safety-net timer: every 10
     minutes while every folder is watched, every minute if one could not be.
     It uses several threads and reuses unchanged directories.
   - Until the first scan of the current folders finishes, a search waits at
     most 20 s (under the engine's 30 s tool wait) and then answers
     `indexing: true` with a note the agent relays.

   Before 0.6.2, the watcher and a 30 s timer only set a dirty flag, and the
   next search re-checked every directory before answering. With 4.7 million
   files that was a 33 s re-check before most answers, and the first search
   after launch walked everything inline (2.5 minutes) and dropped the
   connection (fixed in 0.6.1 by moving the walk off the request).

   Each completed full scan logs `File index: N files in M folders, scanned in
   Xs` to the app log.

Security is unchanged throughout: denied paths are pruned at build time and
scope is re-checked at query time, so the cache can never surface a blocked
path.

### Noise exclusion (`search_filter`)

Separate from the security deny-list (which blocks *sensitive* files), a
`search_filter` list prunes build / dependency / cache **noise** so results
aren't buried in generated files (e.g. a query for `chat-input.tsx` used to
also surface `.next/…/components_chat_chat-input_tsx_*.js`). Because the
agent's job is finding the user's files, recall is the priority:

- **Always excluded** — unambiguous generated dirs, almost all hidden dotfiles
  (`.next`, `.turbo`, `.svelte-kit`, `.parcel-cache`, `.pytest_cache`,
  `.mypy_cache`, `.gradle`, `.cache`, `bower_components`, `*.egg-info`, …).
  Near-zero collision with real content.
- **Excluded only inside a build tree** — ambiguous generic names (`dist`,
  `build`, `out`, `target`, `coverage`, `vendor`) are pruned only when a sibling
  manifest (`package.json`, `Cargo.toml`, `go.mod`, …) proves it. So a user's
  real `~/Documents/build/` stays searchable; a project's `dist/` does not.
- **`.gitignore` is deliberately NOT a hard filter.** For a find-my-files agent
  it routinely hides exactly what the user is looking for (a gitignored
  `notes/`, `data/`, local config) — the opposite of the code-search tradeoff
  ripgrep makes.

## Measured improvement

Benchmark (`bench_index_vs_naive_walk`, run with
`cargo test --lib bench_index_vs_naive_walk -- --ignored --nocapture`) on a
synthetic 3,000-file tree (1,000 source files indexed + 2,000 `node_modules`
files pruned), 5 queries × 20 rounds:

| | per query |
|---|---|
| Naive walk-per-query (previous behavior) | ~4.35 ms |
| Cached index query | ~1.48 ms |
| Index build (one-time, cold) | ~6.2 ms |

**~3× faster per query** in a debug build. This *understates* the real gain:
the cached path is CPU-bound (fuzzy ranking), which debug builds penalize
heavily, while the naive path is I/O-bound; in a release build and on larger or
colder trees the walk cost dominates and the index avoids it entirely, so the
gap widens. The one-time build is amortized by the dirty flag — steady-state
queries pay only the cached cost, and never re-walk unless something changed.

### Real folders (0.6.2)

`bench_index_real_folders` (set `BEAKR_INDEX_BENCH_ROOTS` to colon-separated
folders; run with `cargo test --release --lib bench_index_real_folders --
--ignored --nocapture`) on one developer's Mac, 2026-11-10: Projects,
Downloads, Desktop and Dropbox, 4.7 million files in about 500,000 folders,
warm disk cache for both rows.

| | 0.6.1 | 0.6.2 |
|---|---|---|
| Full scan from nothing | 116 s | 27 s |
| Full rescan, nothing changed | 31 s | 6.5 s |
| Memory after the scan | 2.0 GB | 0.44 GB |
| Memory after a rescan | 4.2 GB | 0.54 GB |
| One search | 1.4 to 5.0 s | 0.07 to 0.47 s |

In 0.6.1 a search after any change also waited (up to 20 s) for the rescan;
in 0.6.2 it waits only for the changed folders the watcher reported.

## Non-goals (deliberately deferred)

- **No content-search index** (trigram / Spotlight / `tantivy` style). For a
  bounded folder set it's real maintenance debt for little gain; content search
  stays fast on the parallel pruned walk. Revisit only if profiling shows
  repeated content queries over a large corpus are too slow.
- **No persistent (SQLite) cache.** The in-memory index rebuilds cheaply on
  launch for bounded folders; persistence is warranted only if cold-start
  re-walk becomes a problem.

## Research basis

Index-free parallel walk + metadata cache is the ripgrep / fd / VS Code
approach for bounded, changing corpora; a full index (Spotlight, Everything,
Zoekt) pays off for whole-disk / many-repo / static corpora. Watch-plus-rescan
(watch for freshness, rescan for correctness) is the Spotlight/Everything
pattern. Fuzzy ranking with word-boundary bonuses follows fzf/`nucleo`;
frecency follows zoxide.

Key sources: ripgrep internals (burntsushi.net/ripgrep), Russ Cox's trigram
index (swtch.com/~rsc/regexp/regexp4.html), Zoekt design doc, `notify` docs +
Watchman recrawl notes, zoxide frecency algorithm, `nucleo`.
