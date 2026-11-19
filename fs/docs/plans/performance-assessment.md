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
