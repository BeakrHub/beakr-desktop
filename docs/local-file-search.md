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

2. **In-memory metadata cache** (PR2). A `FileIndex` on `AppState` holds each
   file's path + name for the scoped folders, so a filename/path search answers
   from memory instead of re-walking disk. Freshness is per-directory: a
   directory whose mtime is unchanged keeps its cached file list (skipped, no
   I/O); a changed directory is re-read. Adding/removing a direct child bumps
   the directory's mtime, so structural changes are always caught, and we still
   recurse for deep changes. Content search still walks + reads (it must read
   file bodies). `list_files` uses the same parallel walker.

3. **Ranking** (PR3). Results are ranked by fuzzy match quality (`nucleo`, the
   Helix matcher — so `chat inpt` finds `chat-input.tsx`), then frecency (how
   often the file has been read via `read_file`), then recency (newest mtime),
   then name. Match quality dominates; the rest break ties.

4. **Watcher + rescan fallback** (PR4). A `notify` filesystem watcher marks the
   index dirty on change, so the next search re-walks immediately; a 30s timer
   marks it dirty regardless, as the correctness backstop for events watchers
   drop (missed events, per-user watch limits, coalesced/renamed entries). Both
   layers only flip a flag, so bursts coalesce for free and no debouncer is
   needed. `ensure_fresh` re-walks only when the flag is set, so most searches
   skip the walk. This watch-plus-rescan pattern is what Spotlight and Everything
   both use.

Security is unchanged throughout: denied paths are pruned at build time and
scope is re-checked at query time, so the cache can never surface a blocked
path.

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
