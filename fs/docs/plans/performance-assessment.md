# Performance Assessment: Events and Crawl (Nov 2026)

> **Status:** implemented on branch `perf/events-and-crawl`. Each item was
> measured before it was changed; two were measured and left alone. Numbers
> are from an 8-core M-series Mac (release build, APFS, warm cache) unless
> marked as Windows CI.

## Summary

| # | Item | Before | After | Done |
| - | ---- | ------ | ----- | ---- |
| 1 | Watcher events: transactions and order | 71 us/row, one transaction per event, 16 events at once in any order | 16 us/row in batches; events in arrival order | Yes |
| 1a | ...end to end, 10,000 created files in one folder (macOS) | 12.4 ms/event | 0.13 ms/event | Yes |
| 1b | Subtree lookup on every delete and rename | 99 ms per event at 1M rows (table scan) | 6-8 us row lookup; subtree query < 0.05 ms | Yes |
| 2 | Exclusion check per walked entry | 0.83 us (5 `glob` patterns) | 0.13 us (`globset`) | Yes |
| 2a | Config copy per event | 0.57 us | — | No: negligible |
| 2b | Exclusion rules built per event | 1.4 us | cached until the patterns change | Yes, because `globset` costs more to build |
| 3 | Idle full-text commit | 12.4 ms of index writes and a sync every 5 s | none when nothing changed | Yes |
| 4 | File table crawl for large roots | whole-drive roots only | also roots whose last crawl found >= 200,000 entries; CI times the table first, on a cold cache | Yes (threshold to confirm) |
| 5 | Walk threads (regression from wiring `crawl_threads`) | half the cores: 1 thread on the 2-core runner, 1.10 s for 83k entries | one per core, at most 12: 0.25 s | Yes |
| 5a | Global gitignore probes per folder | 246 ms for 83k entries | 210 ms (-15%) | Yes |
| 6 | Index for the stale-row cleanup | 0.6-2.4 s per 1M rows | — | No: would slow every re-crawl |
| 7 | rar2zip in-memory test queue | 16 separate databases | one connection | Yes |

## 1. Watcher event pipeline

**Before.** Each event became a task, up to 16 at once (`MAX_EVENT_TASKS`),
and each wrote in a transaction of its own. Two events for one path could
finish in either order: a modify and a delete, or a delete and a re-create.
`indexer.event_batch_size` was parsed and never read.

**Measured.** 10,000 single-row upserts took 713 ms (71 us each); the same
rows in transactions of 100 took 164 ms (16 us each). End to end, 10,000
created files in one folder took 12.4 ms per event in batches of one and
0.13 ms per event in batches of 100. Most of that per-event cost on macOS is
the stale-sibling sweep, which runs on every event there and checks every
indexed file in the folder on disk (10,000 here). Batching runs the sweep
once per folder per batch. On Linux and Windows the sweep was already
limited to once per 30 s per folder, so the gain there is the write side.

**Changed.** One consumer takes events in arrival order, up to
`event_batch_size` at a time. Runs of created, modified and deleted events
are collapsed to the last change per path and written in one transaction
(`FileStore::apply_changes`). Folder work (walking an unseen folder,
removing a deleted folder's subtree), renames and archive events run where
they fall in the order, after the changes before them are written. Content
for changed files is read on the same blocking thread, bounded by the
limit. The async `extract_content` is gone.

**Trade-off.** A large folder walk now delays the events after it, where
before 15 other tasks went on meanwhile. Every write already went through
the one SQLite write connection, so the throughput lost is small, and the
order is now right.

**Found while measuring (1b).** Last round's folder fixes asked for the
subtree of every deleted or renamed path. That query used
`path LIKE ?1 || '/%'`, which is case-insensitive without
`case_sensitive_like`, so SQLite scanned the table: 99 ms per event at 1M
rows. Two changes:

- The path is looked up first (6-8 us), and only an indexed folder's
  subtree is queried.
- The subtree predicate is now two key ranges (`dir/` up to `dir0`, `dir\`
  up to `dir]`) that the unique index on `path` answers, in under 0.05 ms.
  This also stops `_` and `%` in folder names acting as wildcards.

## 2. Repeated work per event and per entry

**Measured.** Copying the config costs 0.57 us and building the exclusion
rules 1.4 us. Neither matters per event. The exclusion *check*, which runs
for every entry of every walk, cost 0.83 us. It matched five
`glob::Pattern`s in turn against the path string.

**Changed.** `ExclusionEngine` is one `globset::GlobSet`: 0.13 us per path,
about 1 s of CPU saved over a 1.4M-entry walk. Two behaviours of the old
matcher are kept and tested: `*` crosses `/`, and on Windows `\` in a
pattern matches a separator (patterns are normalised to `/`). A set costs
more to build, so watcher events take `ExclusionEngine::shared`, which
rebuilds only when the patterns change. The config copy is left as it is.
The Linux `/proc/mounts` read happens only when `skip_network_content` is
set, and is left as it is too.

## 3. Idle commits

**Measured.** A commit and reader reload with nothing pending took 12.4 ms,
and still rewrote the index metadata and synced it every 5 s.

**Changed.** `TantivyStore` records whether anything was added or deleted
since the last commit. `commit_if_changed` skips the commit otherwise, and
the commit loop uses it.

## 4. File table crawl for large roots

**Changed.** `use_mft = "auto"` now also reads the file table for a root
whose last crawl recorded at least 200,000 entries (`root_entries:<root>` in
`index_meta`, written after every crawl). On the 2-core runner the table
cost 9-10 s for all of C: whatever the root, and the walk managed
2,200-10,000 entries a second. That puts the break-even between about
22,000 and 100,000 entries; 200,000 leaves a margin.

**Still to measure.** The whole-volume CI job now runs the file table first,
on a cold cache. Before, it ran second, after the walk had read every
directory, so its 20x may partly be cache. If the cold number holds,
lower the threshold toward the break-even; if not, raise it.

## 5. Windows walk speed

**Found.** Wiring up `crawl_threads` last round made the walk use its
default, half the cores. Before, the `ignore` crate chose one thread per
core, at most 12. On the 2-core runner that meant one walker thread. Over
83k entries here: 1.10 s on one thread, 0.28 s on four, 0.25 s on eight.
The default is now one per core, at most 12. `content_threads` keeps half
the cores. That default also computed `max(1)` before halving, so it gave 0
on a one-core machine, which validation rejects. Fixed.

**Changed.** All four walkers (crawl, folder events, catch-up scan, journal
replay) share `indexer::walk_builder`, which turns off `git_global` and
`git_exclude`. They defaulted to on even with `git_ignore` off. That cost a
`.git` probe per folder (210 ms against 246 ms over 83k entries), and let a
user's global gitignore hide matching files inside repositories.

**Still to measure.** The Windows walk managed about 2,200 entries a second
over all of C:. A profile on Windows is needed to see whether anything else
costs per entry (Defender scanning on open, the `.ignore` probe per folder).

## 6. Index for the stale-row cleanup: not done

The cleanup takes 0.6-2.4 s per 1M rows. An index on
`(watch_root, crawl_generation)` could shorten it, but the re-crawl's fast
path works because no index covers `crawl_generation`: it rewrites that
column on every row. Indexing it would add an index rewrite per row to
every re-crawl (5.9 s per 1M rows now), costing more than the cleanup could
save.

## 7. rar2zip test queue

Each connection to `:memory:` is a database of its own. The queue's pool
held 16, and the tests passed only because the pool kept handing back the
connection that had the schema. An in-memory queue now has one connection,
as `fsd`'s store does.

## Windows CI results and the regression they found

The branch's first CI run showed that the whole-volume job (`C:\` as a root,
`continue-on-error`) had failed 3 of 9 catch-up checks since `ef1070b`, on
`main` too: a deleted file, a renamed file's old name and a file in a
deleted folder stayed indexed. The job's green mark hid it. Two causes:

- **8.3 short names.** `ReadDirectoryChangesW` named the fixture folder by
  its alias (`C:\Users\RUNNER~1\...`, as `%TEMP%` spells it). `ef1070b`
  walks a folder the index has not seen, so the alias indexed a second
  copy of every file under it. The journal replay after a restart reports
  long names, so its deletes left the copies. Event paths are now resolved
  to long form on Windows. An unseen folder is walked only when its parent
  is indexed or is a watch root, which an alias's parent never is.
- **Excluded paths in events.** Watcher events never applied the exclusion
  rules. A data folder inside a watched tree fed its own SQLite and Tantivy
  writes back in as events, and from `ef1070b` each such delete also ran a
  full-table subtree query. Excluded paths now drop out of events.

Run `44598156897` (2-core `windows-latest`), after both fixes:

| Crawl | Root | Time | Rate | Catch-up |
| ----- | ---- | ---: | ---: | -------- |
| walk | System32 (23.6k entries) | 2.2 s | 10,600/s (was ~6,000) | 9/9 |
| file table | System32 | 11.1 s | 2,100/s | 9/9 |
| file table, first and cold | all of C: (1.44M entries) | 28-72 s over three runs | 20,000-52,000/s | 9/9 in 1.2 s |
| walk, after the file table | all of C: | 455-649 s over three runs | 2,200-3,200/s | 9/9 in 1.2 s |

A cold file table read is still 7-20x faster than the walk over a whole
volume, so the `auto` rule and its 200,000-entry threshold stand. The
whole-volume walk rate varies by up to 40% between runs on the same image,
so it does not confirm or rule out the per-entry savings; System32's
1.8x does.

**Found and fixed: the slow store open.** Opening the 1.4M-row store on
restart took 39-100 s before catch-up started. A migration that re-points
`idx_files_parent` dropped and rebuilt that index over every row on every
start. It now runs only when the index is on another column, and the open
logs a time per step. Run `44656692722`: the store opens in under 0.1 s, and
the daemon answers 0.5 s after launch with every change visible at 1.4-2.1 s.

## Head-to-head with voidtools Everything

The `windows-everything` CI job runs `scripts/bench-everything.ps1`
(Everything 1.4.1.1032 portable, queries through `es.exe`) and then
`bench-windows.ps1` over all of C: from the file table, with each store,
one after the other on the same 2-core runner. Everything runs first, so it
reads the file table cold. Runs `44655688723` and `44656692722` (restart
row: after the fix above):

| | Everything | fsd, memory store | fsd, SQLite |
| --- | ---: | ---: | ---: |
| First index build (1.43M entries) | 17.3 s | 10.7 s | 30.2 s |
| Restart until searchable | 0.6 s | 0.5 s | 0.5 s |
| All 9 offline changes visible | 1.2 s | 0.7-1.5 s | 1.4 s |
| Working set, steady | 133-141 MB | 161-170 MB | 25-70 MB plus OS cache |
| Private bytes, steady | 126-133 MB | 213-287 MB | 17-63 MB |
| Peak during the first index | ~231 MB | 837 MB | 879 MB |
| On disk | 53 MB | 78 MB | 933 MB |

Query times in ms: Everything's are es.exe medians less its 47.6 ms start,
fsd's are server times, so both are approximate.

| Query | Everything | fsd memory | fsd SQLite |
| --- | ---: | ---: | ---: |
| `*.rs`, `ext:rs` | 13-16 | 7-10 | 0-2 |
| `zzqx` (substring, no hits) | 92-190 | 30-46 | 155-837 |
| `readme` | 70-160 | 2-3 | 66-115 |
| `parent:C:\` | 41 | 198-467 | 1,625 |
| `depth:99` | 47-73 | 140-153 | 1,472 |
| `dupe: count:10` | 400 | 420 | 3,400 |

## Closing the gaps

Changed after the head-to-head (commit `144c961`), measured in run
`44662344370` on the same job:

- `store.mode` defaults to `"auto"`: the memory store, unless the data dir
  holds only an `index.db`, which is kept so an upgrade does not re-crawl.
- The file table crawl sends each row as the walk reaches it; it collected
  the whole volume first. The table keeps names in one buffer instead of two
  allocations per record, and a drive's table is freed after its last root.
- `parent:` and `depth:` reuse the scan's per-folder path; they walked each
  entry's parent chain, and on a volume every path shares the prefix.
- The memory store grows its records and names by an eighth. Doubling left up
  to half of both allocated and unused, which Windows counts as private
  bytes, and the first file added after a restart doubled the exactly sized
  buffers loaded from the snapshot (277 MB for 143 MB of data, measured
  locally; CI showed 286 MB private).
- `index.bin` version 5 compresses the body with zstd level 1.

| | Everything | fsd memory, before | fsd memory, after |
| --- | ---: | ---: | ---: |
| First index build | 17.3 s, 143.8 s on this run | 10.7 s | 13.5 s |
| Restart until searchable | 0.6 s | 0.5-1.2 s | 1.1 s |
| All 9 changes visible | 1.1-1.2 s | 0.7-1.5 s | 1.4 s |
| Peak during the first index | ~231 MB | 837 MB | 301 MB sampled, 399 MB peak counter |
| Working set, steady | 134-141 MB | 161-170 MB | 161-167 MB |
| Private bytes, steady | 127-134 MB | 213-287 MB | 168-171 MB |
| Store's own heap (logged) | | | 156 MB after a crawl, 144 MB after a load |
| On disk | 53 MB | 78 MB | 15 MB |

| Query (ms) | Everything | fsd memory, before | fsd memory, after |
| --- | ---: | ---: | ---: |
| `zzqx` | 61-255 | 30-46 | 23-38 |
| `parent:C:\` | 42-51 | 198-467 | 61-62 |
| `depth:99` | 68-85 | 140-153 | 49-60 |
| `dupe: count:10` | 346-623 | 419-425 | 375-376 |

Everything's times are es.exe medians less its process start, so they are
approximate. Its first build took 143.8 s on this run against 17.3 s on the
last, on the same image; the cold file table read on a fresh runner varies.

Streaming also took the SQLite store's crawl peak from 879 MB to 250 MB.

**Still behind Everything:**

- The peak counter reached 399 MB at the end of the crawl, about 100 MB above
  every sample. The likely cause is the store's last reallocations (old and
  new buffer both live) while the file table is still held. The crawl now
  counts the entries and name bytes under a root from the file table and
  reserves the store once before its first crawl (6673b25). Not yet measured
  on a runner: the CI run for it was refused for account billing.
- Steady memory is about 35 MB above Everything's, close to what
  `fsd` holds besides the index (runtime, full-text writer, threads).

**`parent:` after the 61 ms above.** It planned to "under this folder" plus
"at this depth", a test of every path, and the memory store compared case
exactly, so `parent:c:\windows` found nothing there while SQLite and
Everything ignore case. It is now one condition, `ParentIs`, on the text
before an entry's last separator, ignoring ASCII case in both stores:

- SQLite: `parent = ? COLLATE NOCASE`, with `idx_files_parent` rebuilt once
  with `NOCASE` so that is an index lookup. The exact-case lookups the crawl
  makes (`children_of`, `has_child`) add the NOCASE comparison to reach the
  same index. 500k rows, one folder: 59.8 ms to 0.058 ms.
- Memory store: an entry whose tail is `/name` is in its parent entry, so the
  answer is the parent's, worked out once per parent by comparing tails from
  the end, without building a path. 2M entries: `parent:/bench` 14.8 to
  3.8 ms, one folder 6.8 to 3.7 ms; what is left is the scan itself. Not yet
  measured on a runner against Everything's ~45 ms for `parent:C:\`.

**Restart, 1.1 s against 0.6 s.** Most of the gap was the measurement. On
that run the daemon logged its first line, finished loading `index.bin`
0.58 s later and was listening 0.03 s after that, but the script's first
`/health` answer came 1.14 s after launch: the rest is PowerShell's polling
and first HTTP request. On a first start with nothing to load, `/health`
came at 1.1 s too, 0.25 s after the daemon's first line. Everything's 0.58 s
was polled with es.exe every 250 ms. So the load itself was about on par.

The load is three steps of similar size, one after another (2M entries,
local): decompress 85 ms, decode 85 ms, rebuild the path index 100-125 ms.
Decompression now runs on a second thread while the main one decodes, and
the index rebuild hashes paths in parallel and inserts them in order. 2M
entries: 270 ms to 171 ms, or 205-215 ms with two rayon threads, as on a
runner. bench-windows.ps1 now also reports the times from the daemon's
log, makes one HTTP request before the first start, and bench-everything.ps1
polls every 50 ms, so the next run compares like with like.

## Desktop run: C:\Windows, not elevated

`bench-windows.ps1 -TargetDir C:\Windows`, once per store, on an i7-10700K
(8 cores) with Windows 11 from a normal prompt. Without elevation the crawl
is the walk and catch-up is the timestamp scan, not the USN journal replay.
C:\Windows is 348,605 files and folders.

| | Memory store | SQLite |
| --- | ---: | ---: |
| Cold crawl | 8.6 s | 9.2 s |
| Peak working set | 97.5 MB | 185 MB |
| Index on disk | `index.bin` 26.5 MB (79.6 B/entry) | `index.db` 259 MB (778 B/entry) |
| Catch-up after restart | 14.2 s, 9/9 checks | 15.5 s, 9/9 checks |

`index.bin` at 2M synthetic entries: 107 MiB (56 B/entry), saved in 248 ms
and loaded in 342 ms. Searches that scan all 2M entries take 6-18 ms; the
bench process peaked at 455 MB.

- The walk did about 40,000 entries/s, against 2,200-3,200/s for all of C:
  on the 2-core runner and 10,600/s for System32 there.
- 56 B/entry matches the runner's whole-volume `index.bin` (78 MB for 1.43M
  entries). C:\Windows costs more per entry because its names are long
  (WinSxS) and the memory store shares parent paths but not names.
- The 2M load took 342 ms here, against 171 ms on an 8-core Mac. That is
  still under 0.4 s, but it is the slowest load measured so far.
- **Catch-up was slower than a cold crawl.** With both stores it took
  1.6-1.7x the crawl. The timestamp scan walks every entry once, like the
  crawl, and writes nothing for unchanged ones. Then it compares each folder
  whose mtime changed with the index, and that comparison was the cost:
  - The memory store's `children_of` rebuilt every entry's path to find a
    folder's children, about 10 ms per changed folder at 100k entries in a
    release build, growing with the index. It now compares parent numbers.
  - Each indexed child of a changed folder was stat'ed to see whether it
    still exists. On Windows a stat opens the file. The folder listing the
    scan already reads now answers that; only children missing from it are
    stat'ed.
  - Replaying the changes then swept the same folders again: a stat per
    indexed child, at most once per folder every 30 s. The scan now marks
    the folders it compared, and the parents of what it replays, as swept.
    A folder whose mtime did not change has had nothing added, removed or
    renamed in it.

  Release build on macOS, 100k entries, with a walk alone of about 150 ms:

  | Changed folders | Memory store | SQLite |
  | ---: | ---: | ---: |
  | 50 | 693 to 212 ms | 282 to 198 ms |
  | 200 | 2,219 to 305 ms | 575 to 330 ms |

  A stat costs much more on Windows than on macOS, so the SQLite row
  understates the gain there. The Windows timings are not measured yet.

### Rerun after those changes

Same machine and tree (348,619 entries), on a build with the compressed
`index.bin` (`144c961`), which the first run predated.

| | Memory store | SQLite |
| --- | ---: | ---: |
| Crawl (warm file cache) | 4.9 s | 6.4 s |
| Peak working set | 102 MB | 186 MB |
| Index on disk | `index.bin` 6.2 MB | `index.db` 253 MB |
| Catch-up after restart | 7.7 s | 7.8 s |
| 8-client load | 93-111 req/s | 48 req/s |

`index.bin` at 2M entries: 28.5 MiB, saved in 555 ms and loaded in 272 ms.
The first memory-store crawl took 8.6 s, as before; the table uses the
rerun, so crawl and catch-up are compared with a warm cache.

- **The per-folder fixes made no measurable difference here**, and the
  explanation above was wrong for this tree: only 11 folders changed.
  Catch-up is still about 1.6x the crawl, the same ratio as the first run
  (7.7 / 4.9 s, 14.2 / 8.6 s), and nearly all of it goes to the 348k
  entries that did not change. The scan's walk is the crawl's walk, plus
  one `symlink_metadata` per directory, because the parent's listing has a
  stale mtime for a directory. On Windows that opens a handle, and C:\Windows
  is mostly folders. The scan now reads a directory's times with
  `GetFileAttributesExW`, which needs no handle, and falls back to
  `symlink_metadata` when that fails. Not measured on Windows yet.
- **SQLite `*.rs` took 144-250 ms against 2 ms for `ext:rs`.** `*.ext` also
  has to match names without an extension (folders named `x.rs`, `.rs`), so
  it ran as `extension = ? OR extension IS NULL`, and C:\Windows has few
  `.rs` files: SQLite read every row without an extension, mostly folders,
  and never reached the limit to stop early. It is now two index reads: the
  extension index, and a partial index `idx_files_noext_name` on the names
  of rows without an extension, which the name check reads without fetching
  rows. It is built with `idx_files_extension`, once, on the first start.
  On a 350k-row synthetic table with a third of rows folders: 84 to 23 ms.
  Not measured on Windows yet.

## Scale: 2M and 10M synthetic entries

`benches/store_scale.rs`, one store and one phase per process, on an 8-core
Mac with 16 GB, release build. The tree is `memory_search`'s: folders of 40
files, mixed-case names, ten extensions. Names are shorter than a real
volume's, so real indexes need more memory per entry than this. `build` is
the crawl's write path and a save; `open` is a restart, then each query run
five times. Peak memory is `/usr/bin/time -l`'s maximum resident set.

| | Memory 2M | SQLite 2M | Memory 10M | SQLite 10M |
| --- | ---: | ---: | ---: | ---: |
| Build and save | 2.1 s | 16.1 s | 10.6 s | 163 s (67 s rows, 95 s indexes) |
| On disk | 28.4 MiB | 685 MiB | 142 MiB (15 B/entry) | 3,451 MiB (362 B/entry) |
| Open | 0.21 s | 0 s | 1.0 s | 0 s |
| Resident after open | 244 MiB | 12 MiB | 1,102 MiB | 10 MiB |
| Peak while querying | 716 MiB | 114 MiB | 3,041 MiB, 1,257 MiB after the dupe fix | 128 MiB |

Median query time at 10M, in ms:

| Query | Memory | SQLite |
| --- | ---: | ---: |
| `zzqx` (no match) | 28 | 249 (max 572) |
| `readme` | 0.1 | 9.2 |
| `*.zzq` | 23 | 30 |
| `*.pdf` | 0.1 | 147, 0.1 after the fix below |
| `ext:zzq` | 20 | 0.0 |
| `"zzqx.txt"` | 29 | 215 |
| `path:/bench/report` | 0.1 | 0.1 |
| `parent:/bench/report/report` | 3.6 | 0.1 |
| `size:>999kb` (no match) | 11 | 0.1 |
| `sort:size readme` | 1.9 | 0.7 |
| `sizedupe: count:100` | 483, 136 after the fix | 165,975 |
| `fuzzy:reprot` | 79, 100 hits | 99, 0 hits |

- **Memory.** The memory store holds the whole index: 1.1 GB at 10M here,
  more with real names. SQLite stays near 100 MB at any size and is the
  store for a machine that cannot spare that. It pays for it everywhere
  else: 15x the build time, 24x the disk, substring and whole-name searches
  of 200-250 ms against the 50 ms target, and `sizedupe:` that takes
  almost three minutes.
- **`sizedupe:` doubled the memory store's peak.** Its sort buffer held a
  48-byte `(i128 key, u32)` per entry and grew by doubling, so for a moment
  it held both buffers: 1.4 GB on top of the index, most of which the
  allocator kept. Keys are now `i64` or `&str` (16 or 24 bytes), and the
  buffer is reserved once. Peak memory went from 2.53 to 1.26 GB and the query
  from 591 to 136 ms.
- **SQLite `*.pdf` got slow with the `*.ext` change above.** As `id IN (...)`
  SQLite collected every match before `LIMIT` applied. It is now a
  compound select that stops at the limit, which brought it back to 0.1 ms.
- **`fuzzy:` scored only the first `max_fuzzy_candidates` (50,000) entries
  matching the other conditions**, 0.5% of a 10M index. Which ones depended
  on the store: SQLite read them in name order through `idx_files_name`, and
  here those were all `Draft_*`, so it found nothing, while the memory store
  found 100. Now every name goes through a cheap prefilter first, and the cap
  counts only names that pass it. The prefilter rejects a name only if
  neither score could reach the threshold:
  - nucleo needs the term as a subsequence;
  - the typo score needs all but `k` of the term's letters within some
    `len + 1` bytes.

  A randomized test checks that it never drops a name the ranker keeps.

  | At 10M | Memory | SQLite |
  | --- | ---: | ---: |
  | `fuzzy:reprot` | 99 ms, 100 hits | 193 ms, 100 hits (was 0) |
  | `fuzzy:mian` | 48 ms | 61 ms |
  | `fuzzy:zzqx` (no match: every name tested) | 127 ms | 1,215 ms |

  The same test found that a term with a capital letter could panic inside
  nucleo, which wants a lowercase needle (`fuzzy:Ad` against `AA_bab`); the
  term is now lowercased first. SQLite's no-match case reads every name
  from disk on one connection; the name-range split that substring searches
  use would cut it if it matters.
- The `sort:size readme` maximum (419 ms memory, 146 ms SQLite) is the first
  run of the process; the median is the steady cost.

## Name filters (memory store)

Every 4096 entries have a Bloom filter of their names' 3-byte windows,
lowercased (`memory_store/name_filter.rs`). A name condition's literal text
(the term, or a wildcard pattern's literal runs) is tested against each
block's filter first, and only blocks that may hold all of it are read.
Filters are built in the background after the index loads and after a full
crawl (0.45 s at 10M), are dropped per block when a name changes, and are
rebuilt on next use. 10 bits per window, ~0.8% false positives.

Block size, from `store_scale trigram` over 9.1M real names from this Mac
(a flat-array scan, so only the ratios carry over):

| entries per filter | filters | `zzqx.txt` | `mian` | `invoice.pdf` |
|---|---:|---:|---:|---:|
| 32768 (the scan block) | 4.6 MiB | 22 → 2.3 ms | 35 → 20 ms | 32 → 18 ms |
| 4096 | 13.8 MiB | 26 → 1.6 ms | 40 → 11 ms | 33 → 7 ms |
| 1024 | 28 MiB | 38 → 6.9 ms | 58 → 12 ms | 60 → 8.6 ms |

Through the full query path at 10M entries (`store_scale open memory`;
"real" builds the tree with those 9.1M names, `build ... <names.txt>`),
median ms without → with filters:

| query | synthetic | real names | hits (real) |
|---|---:|---:|---:|
| `zzqx` | 38.5 → 0.4 | 43.5 → 1.3 | 2 |
| `qwertyuiop` | 46.7 → 0.4 | 52.9 → 0.8 | 0 |
| `"zzqx.txt"` | 32.7 → 0.3 | 41.0 → 0.5 | 0 |
| `*.zzq` | 20.8 → 0.6 | 20.6 → 0.5 | 0 |
| `mian` | 60.3 → 0.5 | 49.4 → 16.3 | 18 |
| `invoice.pdf` | 49.4 → 0.5 | 53.4 → 13.7 | 14 |
| `main.rs` | 47.9 → 0.5 | 15.8 → 4.9 | 100 |
| `readme` | 0.1 → 0.1 | 0.6 → 0.6 | 100 |

- Synthetic names reuse 20 words, so every block holds the same few
  thousand windows and absent terms skip everything; real names are the
  numbers to expect.
- Filters take 16 MB of a 1,031 MB heap at 10M entries.
- Not helped: `ext:`, `size:` and other non-name conditions (11-22 ms when
  nothing matches), `fuzzy:`, `regex:`, terms under 3 bytes, and `path:`.
- `memory_search` (criterion, 2M synthetic) against the commit before:
  `contains_no_match` 7.0 ms → 0.14, `suffix_no_match` 5.5 → 0.14,
  `two_wildcards_no_match` 25.5 → 0.16, `whole_filename_no_match` 9.5 →
  0.13, `sort_date_no_match` 7.2 → 0.16, REST and IPC no-match 7.2 / 6.7 →
  0.31 / 0.22. Nothing else moved beyond noise; save and load are
  unchanged, and a 10M crawl-path build writes in 8.6 s (8.5 before).
- Without the warm-up the first search builds every filter it reaches:
  300-500 ms once at 10M. Compaction drops all filters, so the first search
  after one pays that again.

## Fuzzy at 10M

`fuzzy:` reads every name through `FuzzyPrefilter` and scores up to
`max_fuzzy_candidates` (50,000) survivors with nucleo plus the typo
similarity. Split by stage, real names: `reprot` 75 ms scan + 110 ms
scoring (cap reached), `zzqx` 160 ms scan (16k candidates) + 24 ms, `mian`
9 ms scan + 50 ms scoring (cap reached).

- Scan: the prefilter zeroed a 512-byte table per name before looking at
  it. A 64-bit letter mask now rejects names first: within `k` edits a name
  lacks at most `k` of the term's letters entirely, and a subsequence lacks
  none.
- Scoring: the typo similarity aligned the term against each window of
  length `n-1`, `n` and `n+1` separately, each in a freshly allocated
  matrix, after collecting the lowercased name into a `Vec<char>`. It now
  aligns once per start position (the last row gives all three windows),
  in per-thread buffers, on bytes for ASCII names.

Median ms, before → after:

| query | memory, real names | memory, synthetic | SQLite, synthetic |
|---|---:|---:|---:|
| `fuzzy:reprot` | 174 → 60 | 102 → 40 | 193 → 95 |
| `fuzzy:zzqx` | 188 → 70 | 130 → 36 | 1215 → 729 |
| `fuzzy:mian` | 56 → 25 | 48 → 19 | 61 → 39 |

Results are unchanged: `typo_similarity_agrees_with_aligning_each_window`
compares the new score with the old on random terms and names.

SQLite's no-match case is now its row stream from disk. The name filters
cannot skip blocks here, since a typo need not share any 3-byte window with
the term.
