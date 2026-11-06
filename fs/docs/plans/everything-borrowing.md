# Plan: Borrowing Everything's Techniques

> **Status:** phases 1-4 implemented; phase 5 on macOS; phase 6 crawl-writer fix; open: phase 5 on Windows/Linux, platform enumeration, phase 7 (Nov 2026). Background:
> [research/everything-internals.md](../research/everything-internals.md).
> Measurements: RSS and CPU time sampled every 250 ms plus `/usr/bin/time -l`
> peaks, on `~/projects` (2.38M files, 191k dirs), macOS, 8 cores, with 8
> concurrent REST clients cycling 8 queries for 20 s. The harness is not checked
> in.

## Problems this plan fixes

| #   | Problem                                                                                              | Measured cost                                                      | Phase |
| --- | ---------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------ | ----- |
| 1   | Memory-store snapshot drops `meta`, so `last_crawl_time` is lost and every restart runs a full crawl | 52 CPU-s, RSS 130 MB → 2 GB within 10 s of restart                 | 1     |
| 2   | `fuzzy:` copies up to 50,000 full `FileRecord`s per query before scoring                             | RSS 169 → 758 MB under 8 concurrent fuzzy clients, kept after load | 2     |
| 3   | Memory store stores each path 3x plus a watch-root copy, `name`/`extension` as separate strings      | ~850 B/entry, 2.1 GB idle for 2.56M entries                        | 3     |
| 4   | SQLite crawl maintains 5 secondary indexes row by row                                                | 110-136 s initial crawl                                            | 4     |
| 5   | Changes made while the daemon is stopped are missed (both stores)                                    | correctness + restart cost                                         | 5     |
| 6   | Crawl does one `stat` per entry                                                                      | crawl CPU                                                          | 6     |

## Results (phases 1-3)

| Measure                         |    Memory store before | Memory store after | SQLite before | SQLite after |
| ------------------------------- | ---------------------: | -----------------: | ------------: | -----------: |
| RSS after crawl, idle           |               2,090 MB |             741 MB |        205 MB |       280 MB |
| Peak RSS, cold run (`time -l`)  |               3,156 MB |           1,492 MB |             - |            - |
| RSS growth during query load    |                +300 MB |             -54 MB |       +576 MB |       +13 MB |
| Queries completed in 20 s       |                  2,768 |              3,312 |         1,080 |        1,160 |
| Warm restart: CPU in first 10 s | 52.4 s (full re-crawl) |             0.34 s |         3.3 s |        3.5 s |
| Warm restart: steady RSS        |               2,021 MB |             264 MB |        170 MB |       169 MB |
| `index.bin` size                |                 511 MB |             419 MB |             - |            - |

- Phase 1: the warm restart logs "index already built, skipping initial crawl"
  and loads 2.57M entries in about 0.4 s.
- Phase 2: query-load RSS no longer climbs on either backend.
- Phase 3: a warm memory store holds the whole index in 264 MB, about 100 B per
  entry. After a crawl, idle RSS is 741 MB because the allocator keeps the
  crawl's freed memory; that still meets the phase-3 target of roughly a third
  of the old 2.1 GB.
- Follow-up: loading the snapshot peaks at 1.3 GB RSS because the whole file is
  read and then decoded into new buffers. Reading straight into the arena (or
  `mmap`) would remove that peak.

## Phase 1 - Snapshot keeps metadata (Everything §2.7)

Everything's database is a snapshot plus a journal position; restarting reads it
back and continues. Ours dropped the key-value metadata, so the daemon could not
tell that the index had been built.

- `index.bin` format **v3**: v2 header, then a postcard-encoded body holding the
  entries **and** `meta` and `crawl_gen`.
- v1 and v2 files still load (records only; `meta` empty, so the first start
  after upgrading crawls once, then never again).
- Acceptance: restart of a memory-store daemon on an existing snapshot logs
  "index already built, skipping initial crawl" and stays near idle CPU.

## Phase 2 - Fuzzy scores names, not rows (Everything §2.6)

Everything turns a result set into strings only for the rows it shows. Our fuzzy
path fetched whole rows (path, name, extension, tantivy id, archive path) for
every candidate, scored only the name, and threw 99.9% away.

- New `FileStore::fuzzy_search(conds, cap, rank)`:
  - SQLite: `SELECT id, name ... LIMIT cap` (covered by `idx_files_name` when
    there are no other conditions), rank the names, then load the winning rows
    with `WHERE id IN (...)` and restore rank order.
  - Memory: rank borrowed `&str` names under one read lock, materialize only the
    winners.
- `fuzzy_score` becomes `fuzzy_rank(&[&str], ...) -> Vec<usize>` so both stores
  share the scorer.
- `max_fuzzy_candidates` keeps its meaning (rows considered) and default.
- Acceptance: RSS growth under 8 concurrent `fuzzy:main` clients drops from ~590
  MB to tens of MB; results identical to before on the same candidates.

## Phase 3 - Compact memory store (Everything §2.3)

Everything stores a name and a parent pointer per entry. A full parent-pointer
tree would change every path-based predicate (`path:`, `parent:`, depth, prefix
filters, `!/` archive paths) to walk parents, so this phase takes the part of
the idea that removes the duplication without changing predicate semantics:

- **One copy of every path**, in a shared byte arena. An entry holds
  `(offset, len)` into it.
- **`name` is a slice of the path** (`name_off`), **`archive_path` is a prefix**
  (`archive_len`) for the usual layout `outer.zip!/inner`. Records whose name or
  archive path are not slices of their path (never produced by the crawler, but
  allowed by the API) go to a small side table, so behaviour is unchanged.
- **Extensions are interned** (`u32` id into a table of distinct extensions).
- **Path index is a `hashbrown::HashTable<u32>`** holding only entry numbers;
  the key is hashed from the arena, so the path is not copied again.
- **Crawl generation is a `u32` in the entry**; the per-entry watch-root string
  is dropped (stale removal already filters by path prefix).
- **Rare fields** (`id`, `tantivy_id`) live in the side table.
- Deleted paths leave garbage in the arena; it is compacted when garbage exceeds
  half of the live bytes.
- Matching runs on borrowed entries through a `RecordView` trait implemented by
  both `FileRecord` and the memory store's entry view, so
  `SqlCondition::matches` and meta filters never allocate a `FileRecord`. Rows
  are materialized only for returned results (Everything §2.6).
- The v3 snapshot stores the compact arrays directly, so it also shrinks and
  loads without rebuilding strings.
- Acceptance: idle RSS on the 2.37M-file tree at most ~1/3 of the 2.1 GB
  baseline; all memory-store and evaluator tests pass on both backends.

A true parent-pointer tree (one name per directory level) is left as a
follow-up: it would cut another ~60 B/entry, but needs path reconstruction in
every path predicate.

## Phase 4 - SQLite bulk load (Everything §2.7) - done

Everything never maintains a transactional index while indexing. For SQLite
we approximate that during the *initial* crawl only
(`store/sqlite_store/bulk.rs`, called from `daemon::spawn_crawl`):

- When a crawl starts on an empty `files` table, the `name`, `extension`,
  `mtime`, `size` and `attrib` indexes are dropped and their `CREATE INDEX`
  statements are saved in `index_meta`. After the crawl they are rebuilt from
  the saved statements and the WAL is checkpointed.
- `idx_files_parent` stays: the watcher's stale-sibling sweep (`children_of`)
  runs during the crawl and would scan the whole table per event without it.
  The small partial `idx_files_archive` stays too.
- Queries keep working during the crawl: the name-index scan uses
  `INDEXED BY idx_files_name`, so while it is dropped name queries fall back
  to a plain table scan.
- If the daemon stops mid-crawl the rebuild is skipped; `SqliteStore::new`
  recreates missing indexes from `SCHEMA` on the next start. Reindex requests
  are rejected until the rebuild is done.

Measured on the same tree, alternating runs, on a loaded machine (load
average 5-23):

| Run | Crawl | Index rebuild | Crawl CPU | Queries in 20 s |
|-----|------:|--------------:|----------:|----------------:|
| without bulk load | 131 s, 110 s | - | 166 s, 154 s | 1,128, 1,136 |
| with bulk load | 66 s, 68 s, 101 s | 6.1 s, 9.4 s, 7.7 s | 123 s, 114 s, 290 s | 1,248, 256*, 1,208 |

\* That run saw 344 watcher events during the crawl (others: 0-58) and kept
using ~4 cores after the query clients stopped; the next run with the same
build did not repeat it. Treated as outside interference, not confirmed.

Tried and rejected:

- `cache_size=-16384` on every pooled connection: the crawl got no faster
  (140 s) and RSS grew ~300 MB under query load as 16 connections filled
  their caches.
- `synchronous=OFF` for the crawl: under WAL with `synchronous=NORMAL` SQLite
  only syncs at checkpoints, so there is little to gain, and it would need
  per-connection state in `batch_upsert_crawl`.

Benchmark hygiene: a run on a 98 %-full disk produced 2-3x slower, very
noisy numbers. Delete each run's data directory before the next.

## Phase 5 - Catch up from the OS change log (Everything §2.2) - macOS done

Before: on a warm start the daemon skipped the crawl, and a comment claimed
the watcher would deliver a "startup backlog". It does not - `notify`'s
FSEvents stream starts at "now" - so changes made while the daemon was
stopped were never indexed, in either store, until a manual Reindex.

Now (`watcher/journal.rs`, wired in `daemon.rs` step 17):

- A `journal_position` meta entry stores the FSEvents event id plus, per
  watch root, the FSEvents UUID of its volume. It is stored every 60 s,
  lagging one interval behind so every earlier event has been through the
  debouncer; this makes it crash-safe without a shutdown hook. While a crawl
  runs no position is stored, so an interrupted recovery crawl is redone.
- On a warm start the history since that id is replayed
  (`FSEventStreamCreate` with `sinceWhen`, file-level events, until
  `HistoryDone`). Each changed path is checked on disk and becomes a
  `Modified`/`Deleted` (or archive) event through the normal event pipeline;
  a removed directory also deletes its indexed children, and a
  `MustScanSubDirs` below a root rescans just that subtree.
- A reconcile crawl runs instead when the roots or a volume UUID changed,
  events were dropped or ids wrapped, a root itself must be rescanned, more
  than 200,000 paths changed, or no position is stored yet (first start
  after upgrading).
- Replay paths are mapped back from resolved form (`/private/tmp/...`) to
  the configured root.

Verified end to end on both stores: delete, rename, `rm -rf` of a directory,
a new nested directory and a content change made while stopped all appear
after restart (replay of 11-14 changes: 0.3 s). Unit tests cover the
planner on every platform and a real FSEvents replay on macOS.

Still open:

| Platform | Source | Fallback today |
| -------- | ------ | -------------- |
| Windows | NTFS USN journal (`FSCTL_READ_USN_JOURNAL`): journal ID + next USN per volume | changes while stopped missed; no Windows toolchain here to build or test |
| Linux | none persistent; rescan that skips directories whose `mtime` is unchanged (as `updatedb` does) - misses in-place content edits | changes while stopped missed |

## Phase 6 - Faster enumeration (Everything §2.1) - macOS measured; writer fixed

Measured before building platform enumeration. On macOS the bare `ignore`
parallel walk of the 2.58M-entry tree takes 16 s with `metadata()` and 7.5 s
without it, while the SQLite crawl took 52-57 s. So enumeration was not the
bottleneck. A `sample` profile mid-crawl showed:

- walker threads ~66 % blocked sending to the writer channel, ~23 % in `lstat`;
- the single writer thread ~33 % in `fsync`, ~17 % in `pwrite`, ~25 %
  waiting for input.

Two causes, both fixed:

- The channel held 2,048 records but the writer commits 10,000 at a time,
  so walkers stalled during every commit and the writer then waited for
  them. The channel now holds four batches (`crawler.rs`).
- SQLite's default WAL auto-checkpoint (1000 pages) ran after almost every
  batch, two fsyncs each. During a bulk load `batch_upsert_crawl` raises it
  to 100,000 pages and restores the default after each batch; the WAL peaks
  at ~435 MB and `end_bulk_load` truncates it.

| Variant (2 runs each, alternating) | Crawl | + index rebuild |
|---|---:|---:|
| baseline | 52-57 s | 60-65 s |
| larger channel only | 42-45 s | 50-53 s |
| less frequent checkpoint only | 38 s | 46-47 s |
| both (committed) | 32-37 s | 40-49 s |

Peak RSS did not change (481-540 MB across all runs).

Still open, and now lower priority:

- macOS `getattrlistbulk` could save at most ~9 s of the 16 s walk, which now
  overlaps the writer.
- Windows `FSCTL_ENUM_USN_DATA` over the MFT (service mode) is where
  enumeration matters most and is untested here.
- Linux: `statx` with a minimal mask.

## Phase 7 - Optional extras (Everything §2.4) - open

Make SQLite secondary indexes and stored-but-rarely-used columns configurable
(`[store] indexes = ["name", "extension", "mtime", ...]`), mirroring
Everything's "fast sort" toggles, and document the per-index cost.
