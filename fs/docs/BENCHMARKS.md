# Benchmarks

Summary and live-change tables measured 2026-11-22 on `d7f961b` with
`watcher.backend = "native"` (the Windows default since); the sections after
them are from `1c734c7` plus the optimizations listed in
[Findings](#findings), and have not moved by more than noise since. How to rerun everything is in
[DEVELOPMENT.md §8](DEVELOPMENT.md#8-benchmarks).

**Machine:** Intel i7-10700K (8C/16T), 32 GB RAM, Windows 11 Pro 26100, index
and tree both on `C:` (NTFS). Not elevated, so catch-up used the timestamp scan
and the crawl used the directory walk (no USN journal, no file table). Each
store was run twice in a row; the numbers are from the second run, with a
warm file cache.

## Summary

`scripts\bench-windows.ps1 -TargetDir C:\Windows` (defaults: 8 clients, 20 s
load, 15 s idle, 5 iterations); C:\Windows is 348,615 entries.

|                                        |                          Memory store |                      SQLite store |
| -------------------------------------- | ------------------------------------: | --------------------------------: |
| Crawl                                  |           **4.8 s** (73.3k entries/s) |           6.8 s (51.3k entries/s) |
| Peak working set, whole run            |                            **105 MB** |                            185 MB |
| Working set, idle after crawl          |                                 71 MB |                     35–148 MB [^1] |
| Index on disk                          | **6.2 MB** `index.bin` (18.7 B/entry) |   264 MB `index.db` (794 B/entry) |
| Restart catch-up, all 9 checks visible |                             **3.0 s** |                         **3.0 s** |
| Clean stop (Ctrl+C)                    |                                 1.1 s |                             1.1 s |
| 8-client load, 20 s                    |                 211 req/s, p50 2.8 ms |         **157 req/s**, p50 0.8 ms |

[^1]: Windows trims the SQLite daemon's working set during the idle phase,
    from 148 MB to 35 MB; most of it is SQLite's page cache.

The memory store needs about 60% of the RAM and 1/40 of the disk, and its
slowest queries are 10–25× faster. SQLite wins only on the cheapest indexed
lookups (`ext:`, `ext:a | ext:b`).

### How it moved

| Measure                          | 29e870f | 1c734c7 | 6464789 | d7f961b, `native` |
| -------------------------------- | ------: | ------: | ------: | ------: |
| Catch-up, memory / SQLite        | 7.7 / 7.8 s | 7.7 / 7.9 s | **2.9 / 2.9 s** | 3.0 / 3.0 s |
| Memory 8-client load             | – | – | 104 req/s | **211 req/s** |
| SQLite 8-client load             | 48 req/s | 31 req/s | 94 req/s | **157 req/s** |
| SQLite `*.rs`, 50 results asked  | ~0.5 ms | 53 ms   | **1–2 ms**   | – |
| SQLite `*.rs`, 100 results asked (52 exist) | 144–250 ms | 47 ms | 49 ms | 47 ms |
| Crawl, memory / SQLite           | 4.9 / 6.4 s | 4.9 / 6.0 s | 4.4 / 6.5 s | 4.8 / 6.8 s |
| `index.db`                       | 252.6 MB | 258.6 MB | 260.3 MB | 264 MB |

The 6464789 column is before the rebase onto the fuzzy-search work
(`16968e5`); the load gains in the last column are that work plus `notify`'s
watcher no longer running (on `16968e5` with `notify`: 168 / 140 req/s).

## Live changes per watcher backend

`python -I scripts/bench-live.py target/release/fsd.exe --backends notify,native,usn --es es.exe`
from an elevated console, on `d7f961b`. Memory store, a fresh temp folder per
backend; Everything 1.5 (the installed one, which reads the USN journal) is
polled through `es.exe`, which adds about 36 ms per poll. Times are until a
search returns the expected count; "stale" is what was still indexed after
15 s without progress. Bursts are 40 folders × 1,000 files.

| Scenario                         |        `notify` |     `native` |        `usn` | Everything |
| -------------------------------- | --------------: | -----------: | -----------: | ---------: |
| One new file until searchable    |   576 ms median | 520 ms       | 552 ms       | 695 ms     |
| 40 folders created               |           0.5 s | 0.5 s        | 0.5 s        | 0.6 s      |
| Those folders deleted (`rmtree`) | **39,528 stale** | 0.5 s       | 0.6 s        | 0.6 s      |
| 40,000 files in one folder       |           0.9 s | 0.5 s        | 0.5 s        | 0.9 s      |
| Those files deleted one by one   | **39,563 stale** | 0.3 s       | 0.3 s        | 0.6 s      |
| Folder of 1,000 files renamed    |           0.8 s | 0.6 s        | 0.6 s        | 0.6 s      |
| fsd CPU over all scenarios       |          45.8 s | 8.1 s        | **5.3 s**    | –          |
| fsd CPU, 10 s idle               |           16 ms | 0 ms         | 31 ms        | –          |

- `notify` loses nearly every delete in a burst. Its Windows backend reads
  into 16 KiB and handles each event (debouncer bookkeeping included) before
  it reads again, and drops the overflow without a signal. It also costs
  5–9× the CPU of the other two for the same changes.
- `native` (`watcher/live/rdcw.rs`, the default on Windows) never overflowed
  here, even with a 4 KiB buffer: it starts the next read before parsing the
  last one.
- `usn` (`watcher/live/usn.rs`) reads the whole volume's journal, so it spends
  a little CPU on unrelated changes when idle (31 ms per 10 s here), and the
  least while busy.
- Forced loss: fsd suspended while `del /q` removed 20,000 files from one
  folder. `native` (64 KiB buffer) reported the overflow, its recovery scan
  took 0.4 s, and the index was right 0.6 s after resume. `usn` needed no
  recovery: the journal still held every delete, read 0.6 s after resume.

## Release binaries

`cargo build --release`, stripped (`strip = true`), x86_64-pc-windows-msvc.
Sizes in MiB.

| Binary               |  Size |
| -------------------- | ----: |
| `fsd.exe`            | 20.03 |
| `fs-ext-rar2zip.exe` | 11.93 |
| `fs-ext-sftpd.exe`   |  6.47 |
| `fs.exe`             |  1.80 |

## `index.bin` size

| Index                                 |   Entries | `index.bin` | Per entry |            Save |             Load |
| ------------------------------------- | --------: | ----------: | --------: | --------------: | ---------------: |
| Real tree, `C:\Windows` + fixture     |   348,610 |      6.2 MB |    18.7 B | (in 1.1 s stop) | (in 0.5 s start) |
| Synthetic, `benches/memory_search.rs` | 2,000,000 |    28.5 MiB |    14.9 B |          555 ms |           272 ms |

Format version 5 compresses the body as one zstd frame
([index-bin-format.md](reference/index-bin-format.md)); against the
uncompressed version 4 the synthetic file went from 107.2 MiB to 28.5 MiB,
saving from 248 ms to 555 ms and loading from 342 ms to 272 ms (two threads).
Saving only runs at shutdown and every `store.snapshot_interval_secs`.

SQLite's `index.db` for the same real tree is 260 MB, 42× larger: the path is
stored in full per row, plus six secondary indexes. Tantivy was empty in both
runs: `C:\Windows` holds nothing that matches the default
`content.extension_limits`.

## Memory store at 2M entries (criterion)

`cargo bench -p fs-server --bench memory_search` on `29e870f` (the memory
store has not changed since); median of 15 samples. The bench process peaked
at 455 MB working set on `7c162af`; not remeasured.

| Query                                  |             Median |
| -------------------------------------- | -----------------: |
| `readme` (contains, common)            |              92 µs |
| `zzqx` (contains, no match: full scan) |             8.9 ms |
| `*.pdf` (suffix, common)               |              82 µs |
| `*.zzq` (suffix, no match)             |             7.8 ms |
| `*report*zzq*`                         |            17.8 ms |
| `"zzqx.txt"` (whole name)              |             9.1 ms |
| `ext:zzq`                              |             6.0 ms |
| `path:/zzqx` (prefix, no match)        |            11.0 ms |
| `path:/bench/report` (prefix, common)  |              73 µs |
| `path: zzqx` (path contains)           |             9.0 ms |
| `parent:` (shared prefix / one folder) |    7.5 ms / 8.0 ms |
| `depth:` (no match)                    |             9.1 ms |
| `sort:name readme`                     |             8.4 ms |
| `sort:size readme`                     |             2.0 ms |
| `sort:size` (all)                      |              97 µs |
| `sort:dm zzqx`                         |             9.5 ms |
| `sizedupe: count:100`                  |             127 ms |
| Engine vs REST, no match               | 8.51 ms vs 8.72 ms |
| Engine vs REST, 100 hits               |    96 µs vs 277 µs |

A full scan of 2M entries takes 6–18 ms, under the 50 ms target. REST adds
about 0.2 ms for 100 serialized hits.

## End-to-end query latency, `C:\Windows`

`scripts/bench-windows.ps1`, 5 iterations of 100 results each, client-side
median in ms (first run; the post-restart run is within noise of it).

| Query                  | Memory |    SQLite |
| ---------------------- | -----: | --------: |
| `*.rs`                 |    4.4 |      48.9 |
| `*.toml`               |    3.4 |      45.6 |
| `size:>1mb`            |    2.9 |       3.5 |
| `dm:today`             |    3.4 |       3.1 |
| `ext:rs`               |    3.4 |       2.2 |
| `fuzzy:main`           |   82.2 |      94.1 |
| `!ext:rs size:>100kb`  |    2.8 |       3.3 |
| `ext:md \| ext:toml`   |    3.8 |       1.1 |
| `zzqx`                 |    4.3 |      16.0 |
| `readme`               |    6.0 |      16.7 |
| `depth:99`             |   12.3 |     325.2 |
| `dupe: count:10`       |   41.9 |     583.9 |
| `parent:C:\Windows`    |    2.7 |       3.1 |
| `path:C:\Windows\zzqx` |   11.0 |     116.3 |

SQLite `*.rs` and `*.toml` cost ~45 ms whenever fewer matches exist than
were asked for, because every name without an extension (each of the 167k
folders) is checked. With 50 results asked and 52 `.rs` files present it
stops early: `scripts\bench-query.ps1` measures 1–2 ms for `*.rs` (three runs,
20 iterations each, max 12–14 ms) and 44 ms for `*.toml`.

## Process profile, `C:\Windows`

CPU % is of one core (1600 % = all 16 threads).

| Phase            | Memory: secs / CPU % / peak WS | SQLite: secs / CPU % / peak WS |
| ---------------- | -----------------------------: | -----------------------------: |
| Crawl            |          4.3 s / 673 % / 78 MB |         6.4 s / 534 % / 126 MB |
| Idle             |           15.3 s / 0 % / 66 MB |          15.4 s / 5 % / 141 MB |
| 8-client load    |        20.6 s / 1364 % / 93 MB |       20.6 s / 1299 % / 145 MB |
| Restart catch-up |          2.3 s / 880 % / 74 MB |          2.3 s / 963 % / 63 MB |

Catch-up is now cheaper than the crawl: it lists every folder but writes only
what changed.

## Findings

1. **Catch-up opened every folder (fixed).** A folder's times in its
   parent's listing are a copy NTFS does not refresh when the folder's
   contents change, so the timestamp scan asked each folder for its own:
   `symlink_metadata`, then `GetFileAttributesExW` (`1c734c7`). Both open the
   folder, about 155 µs each, which was 2.5 s of the 7.5 s scan of
   C:\Windows's 167k folders; `GetFileAttributesExW` saved nothing. A
   folder's listing begins with its own `.` entry, which carries the current
   times (checked: it changes when a child is added, renamed or deleted; the
   parent's copy does not). `watcher/journal/scan/walk.rs` now lists folders
   with `FindFirstFileExW` and reads `.`; `ignore` and `std::fs::read_dir`
   drop that entry. The walk alone takes 2.7 s against 5.0 s for `ignore`,
   and catch-up went from 7.7 s to 2.9 s with 9/9 checks. Unlike the
   `ignore` walk it does not honour `.ignore` files.
2. **SQLite `*.ext` lost its early exit (fixed).** `1c734c7` wrote `*.ext`
   as `id IN (… UNION ALL …) LIMIT ?`, which builds the whole id list before
   the limit applies, so every call cost the full ~50 ms even when the
   extension index alone filled the limit (the old `OR` form stopped after
   0.5 ms there). It is now two selects in one compound under the `LIMIT`,
   which SQLite runs arm by arm and stops early.
3. **SQLite searches serialised on a global mutex (fixed).** Throughput of
   `*.toml` *fell* as clients were added: 22.8 req/s with 1, 10.4 with 8, 7.4
   with 16. `LOWER(name) GLOB LOWER(?)` allocates per row, and bundled
   SQLite counts every allocation under one global mutex (`MEMSTATUS`).
   `.cargo/config.toml` now builds it with `SQLITE_DEFAULT_MEMSTATUS=0`: 8
   clients run `*.toml` at 130 req/s, and the load mix went from 31 to 94
   req/s. `mmap_size` made no difference. Nothing reads SQLite's memory
   statistics.
4. **Fixed: a directory deleted while `fsd` was stopped could leave its
   contents in the index.** A live watcher event during the timestamp scan
   ran the stale-sibling sweep, which removed the vanished directory's row
   only; the catch-up plan then never saw its children. The sweep now calls
   `remove_path`. Regression test:
   `indexer::crawler::tests::sibling_sweep_removes_a_vanished_directory_with_its_contents`.
5. **Fixed: `bench-query.ps1` and `bench.ps1` read the wrong log.** Both
   waited for readiness lines in `daemon*.out.log`, but the daemon logs to
   stderr, so `bench-query.ps1` always timed out after 600 s and `bench.ps1`
   never noticed a reused index. `bench.ps1` also built a memory index
   (the daemon default), which its hard kill leaves empty; it now takes
   `-Store` and defaults to `sqlite`.
6. **Benchmark harness:** when `bench-windows.ps1` is started from Git Bash
   (or another host that ignores Ctrl+C), the daemon inherits "ignore Ctrl+C",
   the clean stop fails and the script kills it. The memory store then never
   writes `index.bin`. Run it from a normal PowerShell or Windows Terminal
   window.
