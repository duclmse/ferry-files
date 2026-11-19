# Plan: Borrowing Everything's Techniques

> **Status:** phases 1-5 and 7 implemented (phase 5: FSEvents replay on macOS, USN journal replay on Windows, timestamp scan on Linux and as the Windows fallback); phase 3 follow-up: parent-pointer tree; phase 6 crawl-writer fix, platform enumeration measured and not built, NTFS file table crawl built behind `indexer.use_mft` (default `auto`: whole-drive roots only) and run on real volumes in CI: 20x faster than the walk over all of `C:\`, slower for a root that is a small part of a volume (Nov 2026). Background:
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

inotify keeps no history, so Linux uses `watcher/journal/scan.rs`, as does
Windows whenever the USN journal cannot be replayed (next section). The stored position is the
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
initial crawl and longer for a reconcile crawl, which also upserts every
row. Unit tests run on macOS (new, edited, deleted, moved-in,
unindexed-with-old-times and excluded entries). Since Nov 2026 CI runs the
tests on Linux and Windows too, and the Windows benchmark's catch-up checks
exercise the scan as the fallback.

### Windows: USN journal replay (follow-up, done; run on Windows in CI)

`watcher/journal/usn.rs` replays the NTFS change journal, so the Windows
catch-up costs time in proportion to the changes instead of the tree, like
Everything's.

- The position gains `usn`: per root, the journal ID and next USN of its
  volume (`FSCTL_QUERY_USN_JOURNAL`). `event_id` keeps the wall-clock time,
  so the timestamp scan still has a position to scan from. Positions stored
  before this change have no `usn` and use the scan once.
- On a warm start the records since the stored USN are read with
  `FSCTL_READ_USN_JOURNAL`, once per volume however many roots it holds.
  A record names the file and its parent directory's file reference number;
  the parent's current path comes from `OpenFileById` and
  `GetFinalPathNameByHandleW` (cached per parent), and is mapped back onto
  the configured root ignoring ASCII case.
- The result is the same `path -> flags` map the FSEvents replay produces,
  so the same planner checks each path on disk. A directory renamed into
  place gets `MustScanSubDirs`, because its contents moved without records
  of their own. A record whose parent no longer exists is skipped: the
  removal of that parent, or of the nearest ancestor that still resolves, is
  in the journal too, and deleting a directory deletes everything indexed
  below it. The planner now treats a `MustScanSubDirs` path that is gone as
  deleted, on macOS as well.
- Replay falls back to the timestamp scan, not to a crawl, when the volume
  cannot be opened (it needs administrator rights; the service runs as
  LocalSystem), the journal was recreated or no longer reaches the stored
  USN, the volume is not NTFS (ReFS writes v3 records), or more than 200,000
  paths changed. A missing root still goes to the scan, which crawls.

The record parser, the fold into paths, the root mapping and the volume
grouping are unit-tested on every platform. On Windows (Nov 2026, CI and
`scripts/bench-windows.ps1`) the replay passes all 9 catch-up checks, both
after a walk and after a file table crawl. Running it there found that
canonicalized roots kept the `\\?\` prefix, so no record mapped onto a
root and replay never ran; roots are now stored without it.

## Phase 6 - Faster enumeration (Everything §2.1) - writer fixed; platform enumeration measured, not built

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

Platform enumeration was not built (Nov 2026). The macOS part was measured
with a prototype; the other two turned out to have little to gain:

- **macOS `getattrlistbulk`.** A prototype walker (one `getattrlistbulk`
  call per directory buffer, returning name, type, mtime, ctime, flags and
  size, directories shared between threads) against the `ignore` walk with
  `metadata()` that the crawler uses, on the 2.5M-entry `~/projects` tree,
  warm cache, load average 6-10:

  | Threads | `ignore` + `lstat`     | `getattrlistbulk`  |
  | ------: | ---------------------: | -----------------: |
  |       2 |                   45 s |               25 s |
  |       4 |                   26 s |               20 s |
  |       8 | 18.4-18.9 s (87 s CPU) | 20-21 s (44 s CPU) |
  |      16 |                      - | 18.6 s (48 s CPU)  |

  One thread on a 50K-entry subtree: 0.44 s against 0.26 s.

  Per directory it is about twice as fast and it halves the CPU, but
  `getattrlistbulk` stops scaling past about two busy cores (APFS
  serializes it), so it never gets below the ~18.5 s the parallel `lstat`
  walk already reaches. The crawl would use ~40 s less CPU, out of 174 s,
  and finish no sooner. On macOS a crawl runs only on the first start or
  after a lost FSEvents history, so this was not worth replacing the
  `ignore` walker. A replacement would also have to reproduce its handling
  of `.ignore` and git exclude files: the prototype found 30K entries more
  than the `ignore` walk.
- **Linux `statx`.** std's `symlink_metadata` already calls `statx` on
  Linux, so a smaller mask would save only filling unused fields, not the
  system call per entry. Linux has no bulk equivalent (`getdents64` returns
  names and types only).
- **Windows.** `ignore` keeps the metadata that `FindNextFileW` returned with
  the directory listing, so the Windows walk makes no per-entry call to
  begin with. What remains is Everything's MFT read (`FSCTL_ENUM_USN_DATA`
  or the raw `$MFT`), which enumerates a volume without visiting
  directories but needs administrator rights, and needs the raw `$MFT` for
  sizes and times. Since the USN replay (phase 5), a Windows warm start
  walks only when the replay fails, so this would only shorten initial
  crawls. Built afterwards as a raw `$MFT` reader (`indexer/mft.rs`), since
  `FSCTL_ENUM_USN_DATA` gives names and parents but no sizes or dates. It
  runs per `indexer.use_mft` (`auto` by default, see below), falls back to
  the walk on any failure,
  does not honour `.ignore` files, and rejects an `$MFT` split across
  records.

  CI runs it on the Windows Server 2025 runner (NTFS C:, 2 cores, 8 GB),
  elevated, with `C:\Windows\System32` as the root, next to the walk:

  | Enumeration | Entries indexed | Crawl (after `/health`) |
  | ----------- | --------------: | ----------------------: |
  | walk        |          23,587 |                  4.0 s  |
  | file table  |          23,603 |                  9.3 s  |

  The file table read covers the whole volume however small the root is,
  so it only pays for a root that is most of a volume. It found 16 more
  entries than the walk, ones the walk could not list.

  Over the whole of `C:\` on the same runner, content indexing off
  (`bench-windows.ps1 -NoContent`), both crawls finished with 9/9 catch-up
  checks passing:

  | Enumeration | Entries indexed | Crawl (after `/health`) |    Rate |
  | ----------- | --------------: | ----------------------: | ------: |
  | walk        |       1,430,354 |                  650 s  | 2,200/s |
  | file table  |       1,430,461 |                   33 s  | 43,000/s |

  The file table run came second, after the walk had read every directory,
  so some of its 20x may be a warm cache; the raw `$MFT` read itself is
  sequential.

  Since these results `use_mft` defaults to `"auto"`: the table is read for
  a root that is a whole volume (`C:\`), and reused for any later root on a
  drive already read in the same crawl; other roots walk. `true` and
  `false` force either way.

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
`LIMIT` is reached only near the end.

Follow-up, done: a plain `*.ext` glob is narrowed to `idx_files_extension`
in the planner (`plan.rs`, `glob_extension`), which adds
`ExtOrNone(ext)`: `extension = ext OR extension IS NULL`. That is exact,
not a guess: every writer stores the lowercased `Path::extension`, and
folders and names such as `.pdf` have none, so any name the glob matches
has that extension or no extension. SQLite answers it with a multi-index OR
on the extension index, so it also serves sorted queries, OR branches and
no-match globs. The SQLite search takes the plain query plan when the
condition is present, since the partitioned name scan would read every
name. Synthetic 2M-row database (`populate_for_bench` plus 200 `.pdf` names
spread over every name range; no folders, so few rows lack an extension),
warm, `limit=100`:

| Query           | Name index only | + extension index |
| --------------- | --------------: | ----------------: |
| `*.pdf`         |          818 ms |           0.07 ms |
| `*.md`          |          1.1 ms |           0.09 ms |
| `*.zzz`         |          881 ms |          0.013 ms |
| `*.pdf` sorted  |          332 ms |           0.15 ms |
| `*.md` sorted   |          339 ms |            164 ms |

`*.md` sorted still orders all 250,000 matches. In a real tree a no-match
glob reads the rows without an extension (folders, 7% of `~/projects`, plus
extensionless files) instead of every row.

Use the default unless writes matter more than queries, for example a large
tree under constant churn. Remove `attrib` first, since no common query uses
it, then `size` and `mtime` if nobody sorts or filters by them. Keep `name`:
name search is the main use.
