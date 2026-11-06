# Plan: Borrowing Everything's Techniques

> **Status:** phases 1-5 and 7 implemented (phase 5: FSEvents replay on macOS, timestamp scan on Linux and Windows); phase 6 crawl-writer fix; open: USN journal replay, platform enumeration (Nov 2026). Background:
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
- Follow-up, done: loading the snapshot read the whole file and then decoded
  it into new buffers, and saving encoded it into one buffer, copied that into
  a second, and compacted (copied) the arena first. `memory_store/snapshot.rs`
  now streams both directions field by field in the same v3 format, reading
  the arena straight into its final buffer and writing the arena without
  garbage instead of compacting it. Peak RSS for a 2.59M-entry, 420 MB
  snapshot: warm start and stop 1,578-1,602 MB → 598-599 MB; cold crawl
  plus first save 1,541 MB → 806 MB.

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

### Follow-up, done: parent-pointer tree

Each entry now stores only its **tail** plus the entry number of its parent,
as Everything does. The tail is `/name` for an entry whose directory is
indexed, `!/inner` for an archive's top-level members (their parent is the
archive entry), or the whole path when the parent is not indexed (watch
roots, records inserted through the API). A parent always has a lower entry
number than its children, so a snapshot can be validated and loaded in one
pass.

- Paths are rebuilt by walking the parent chain, into a thread-local buffer
  where only a borrow is needed. The path index hashes the rebuilt path and
  compares by stripping tails off the end of the probe, so a lookup copies
  nothing on a hit.
- `RecordView` exposes the path through a callback (`with_path`) plus
  `path_is_under` and `path_depth`. The memory store answers those two by
  walking the chain without rebuilding the path: `parent:` compares tails
  root-first against the directory and rejects most entries in their first
  segment, and `depth:` sums the `/` in each tail.
- Removing a directory that still has indexed children marks it dead
  (`F_DEAD`) instead of dropping it, so its children keep resolving and a
  re-add of the same path revives it in place. Dead entries are compacted
  once there are more than 4,096 of them and they exceed a quarter of the
  table, keeping only those that are still ancestors of live entries, and
  are never written to a snapshot unless they are such ancestors.
- `index.bin` is **v4** (tails arena, entries with `parent`). v3 files load
  by re-inserting their entries, shortest path first, so parents come first.

Measured on `~/projects` (2.31M files + 178K dirs, memory store, same
`run.sh` load as the table above; footprint is `vmmap` "Physical
footprint", which, unlike RSS, includes compressed pages):

| Measure                          | Flat arena | Parent tree |
| -------------------------------- | ---------: | ----------: |
| Footprint after crawl, idle      |     527 MB |      249 MB |
| Peak footprint, cold crawl       |     753 MB |      419 MB |
| Footprint, warm restart steady   |     515 MB |      291 MB |
| `index.bin` size                 |     401 MB |      169 MB |
| Warm restart until ready         |     0.40 s |      0.54 s |
| Queries completed in 20 s        |      3,304 |       3,320 |

Queries that must rebuild or walk the path of every entry got slower; the
rest did not change. Warm REST calls, `limit=100`, five runs each:

| Query                      | Flat arena | Parent tree |
| -------------------------- | ---------: | ----------: |
| `zzqx` (name, no match)    |  112-129 ms |   114-135 ms |
| `readme`                   |     2 ms   |      2-3 ms |
| `path:/zzqx` (full scan)   | 156-164 ms |  350-361 ms |
| `parent:/zzqx` (full scan) |   14-15 ms |    76-78 ms |
| `depth:99` (full scan)     |   86-88 ms |  178-183 ms |
| `parent:` an indexed dir   |     5 ms   |        7 ms |
| `dupe:name count:10`       |    1.0-1.1 s |    1.0-1.1 s |

That is the trade Everything makes as well: half the memory, and path
predicates over the whole table cost a chain walk per entry.

## Phase 4 - SQLite bulk load (Everything §2.7) - done

Everything never maintains a transactional index while indexing. For SQLite
we approximate that during the _initial_ crawl only
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
- If the daemon stops mid-crawl the rebuild is skipped;
  `SqliteStore::with_indexes` recreates the configured indexes on the next
  start (phase 7). Reindex requests
  are rejected until the rebuild is done.

Measured on the same tree, alternating runs, on a loaded machine (load
average 5-23):

| Run               |             Crawl |       Index rebuild |           Crawl CPU |    Queries in 20 s |
| ----------------- | ----------------: | ------------------: | ------------------: | -----------------: |
| without bulk load |      131 s, 110 s |                   - |        166 s, 154 s |       1,128, 1,136 |
| with bulk load    | 66 s, 68 s, 101 s | 6.1 s, 9.4 s, 7.7 s | 123 s, 114 s, 290 s | 1,248, 256*, 1,208 |

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

## Phase 5 - Catch up from the OS change log (Everything §2.2) - done

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
  The position captured before the crawl is kept and stored within seconds
  of the crawl finishing: everything before it was crawled and everything
  after it is in the log. (At first only positions captured after a crawl
  were stored, so a restart within two minutes of any crawl crawled again.)
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

### Linux and Windows: timestamp scan

inotify keeps no history, and reading the USN journal is not built yet, so
these platforms use `watcher/journal/scan.rs`. The stored position is the
wall-clock second it was captured, plus each root's device id (Unix) so a
different filesystem mounted on a root forces a crawl. A warm start walks the
roots with the crawler's exclusions and flags entries whose times are within
2 s of the position or newer:

- mtime, plus ctime on Unix (only the kernel sets it, so an extracted or
  copied file with an old mtime is caught) or creation time on Windows (a
  copy or extraction sets it to now);
- each changed directory's children are compared with the index: indexed
  ones that are gone are deleted (with everything under them), unindexed ones
  are added. This catches entries moved in with all their times old, which
  NTFS does on rename;
- a changed directory that is not in the index is walked in full, since its
  contents can keep old times;
- more than 200,000 events still means a reconcile crawl.

Missed: changes that leave every checked time older than the position (a
clock set backwards, or a tool that rewrites times).

Cost: one `stat` per entry and no index writes for unchanged files. On the
2.6M-entry `~/projects` tree the walk takes 16.5 s (release, warm cache, no
exclusions, macOS: the same walk phase 6 measured), against 32-37 s for an
initial crawl and longer for a reconcile crawl, which also upserts every row. Unit tests run on macOS (new, edited, deleted, moved-in,
unindexed-with-old-times and excluded entries); Linux and Windows builds are
clippy-clean via cross-compilation, but neither has been run on its own OS
here.

Still open: replaying the NTFS USN journal (`FSCTL_READ_USN_JOURNAL`, journal
ID + next USN per volume) would make the Windows catch-up cost proportional
to the changes instead of the tree.

## Phase 6 - Faster enumeration (Everything §2.1) - writer fixed; platform enumeration deferred

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

| Variant (2 runs each, alternating) |   Crawl | + index rebuild |
| ---------------------------------- | ------: | --------------: |
| baseline                           | 52-57 s |         60-65 s |
| larger channel only                | 42-45 s |         50-53 s |
| less frequent checkpoint only      |    38 s |         46-47 s |
| both (committed)                   | 32-37 s |         40-49 s |

Peak RSS did not change (481-540 MB across all runs).

Platform enumeration was not built (Nov 2026):

- macOS `getattrlistbulk` could save at most ~9 s of the 16 s walk. The walk
  now overlaps the writer, and warm starts replay FSEvents instead of walking,
  so only an initial or reconcile crawl would gain, by less than that.
- Linux and Windows warm starts do walk (phase 5 timestamp scan, ~16 s for
  2.6M entries), so faster enumeration would pay there: `statx` with a minimal
  mask on Linux, `FSCTL_ENUM_USN_DATA` over the MFT on Windows (service mode;
  USN journal replay would remove the walk altogether). Neither can be
  measured on this machine, so both stay open until they can be.

## Phase 7 - Optional extras (Everything §2.4) - done

SQLite secondary indexes are configurable in the same way as Everything's
"fast sort" toggles:

```toml
[store]
indexes = ["name", "extension", "mtime", "size", "attrib"]   # the default
```

- The optional indexes are no longer in `SCHEMA`, which runs on every start.
  `SqliteStore::with_indexes` builds the listed ones that are missing and
  drops the unlisted ones, so a change takes effect on the next start. A
  dropped index's pages stay in the file until a `VACUUM`.
- `idx_files_parent` (directory listing, the watcher's stale-sibling sweep)
  and the partial `idx_files_archive` are always kept.
- Without `name`, name searches take the plain-scan path they already used
  during a bulk load, because the partitioned search names the index in
  `INDEXED BY`.
- The bulk load records only the indexes that exist, so it never rebuilds an
  unlisted one. Opening the store also clears a replay left by a crawl that
  never finished, because the configured indexes are rebuilt by then.

Making columns optional was not worth it: the large ones (`path`, `parent`,
`watch_root`) are all used, and the integer columns are 1-9 bytes each.

Cost per index, measured on 2.59M rows (`~/projects`, sqlite mode, load
average 3-17 during the runs). Size comes from `dbstat`. Build is the time to
create the index on the full table at startup. The table itself is 884 MB,
and `idx_files_parent` is 281 MB.

| Index       |   Size | Build      |
| ----------- | -----: | ---------- |
| `name`      | 116 MB | 4.4-10.6 s |
| `extension` |  27 MB | 1.7-6.8 s  |
| `mtime`     |  33 MB | 3.9-6.1 s  |
| `size`      |  29 MB | 3.4-7.4 s  |
| `attrib`    |  23 MB | 2.5-6.3 s  |

The initial crawl is not affected, because the bulk load (Phase 4) drops
these indexes anyway. They do slow every later write. A full rescan of the
existing database (every row upserted, two alternating runs each) took:

| `indexes`                              | Rescan     |
| -------------------------------------- | ---------- |
| `[]`                                   | 45-50 s    |
| `["name"]`                             | 87-102 s   |
| `["extension","mtime","size","attrib"]` | 90-100 s   |
| all five                               | 131-136 s  |

In return, queries on an indexed column take milliseconds, and without the
index they scan the whole table. Times are warm REST calls with
`limit=100`, from the second run on; results were identical:

| Query             | All indexes | None       |
| ----------------- | ----------- | ---------- |
| `zzqx` (no match) | 70 ms       | 0.9-6.1 s  |
| `*.pdf`           | 72-76 ms    | 0.27-1.7 s |
| `ext:pdf`         | 1 ms        | 0.23 s     |
| `size:>500mb`     | 0.6-0.8 ms  | 0.4-1.6 s  |
| `readme size:>1mb` | 124 ms     | 0.57-1.2 s |
| `readme` (common) | 70 ms (6-11 ms after the fix below) | 7 ms |

The last row was a side finding, now fixed. The partitioned name search
splits the index into up to eight name ranges scanned in parallel, and
waited for all of them. The matches for `%readme%` almost all sort into the
range holding names that start with "readme", so the other ranges scanned
to their end and the query cost as much as one with no match. The ranges now
share a row count, and once it reaches `LIMIT` the ones still scanning are
stopped with `sqlite3_interrupt` (`search.rs`, `EarlyStop`). Same database,
two runs of three warm calls each:

| Query    | Before   | After     |
| -------- | -------- | --------- |
| `readme` | 73-78 ms | 6-11 ms   |
| `zzqx`   | 74-134 ms | 78-120 ms (must scan everything) |
| `*.pdf`  | 77-121 ms | 59-129 ms |

`*.pdf` is unchanged: its matches are spread thinly over every range, so
`LIMIT` is reached only near the end. Answering it from `idx_files_extension`
like `ext:pdf` (1 ms) would need the glob rewritten into an extension test
that matches exactly the same names, which is left open.

Use the default unless writes matter more than queries, for example a large
tree under constant churn. Remove `attrib` first, since no common query uses
it, then `size` and `mtime` if nobody sorts or filters by them. Keep `name`:
name search is the main use.
