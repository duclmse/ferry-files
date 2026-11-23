# Documentation Index

Start with the [overview](overview.md). When a reference doc and a phase or
research doc disagree, the **reference** doc wins.

| Folder                     | What it holds                                         | Authority         |
| -------------------------- | ----------------------------------------------------- | ----------------- |
| [`reference/`](reference/) | Module contracts, schemas, query grammar              | Authoritative     |
| [`phases/`](phases/)       | Phased implementation plan with implementation notes  | Design intent     |
| [`plans/`](plans/)         | Cross-cutting change plans / agent handoffs           | Historical        |
| [`research/`](research/)   | Exploratory design notes that fed the phase docs      | Not a spec        |

## Status (Nov 2026)

All phases (1–13) and both plans are implemented, and every checklist item
in `phases/` is ticked. CI (`.github/workflows/ci.yml`) builds, lints and
tests the workspace on Windows Server 2025, Ubuntu and macOS on every push,
and on Windows also runs the benchmark with and without the file table crawl
and installs, starts, searches through and stops the Windows Service. On
Ubuntu and macOS it does the same with a systemd system unit and a
LaunchDaemon, searched over IPC by an unprivileged user.

| Area | State |
| ---- | ----- |
| `fsd` daemon, `fs` client, query engine | Done; Everything-compatible syntax incl. `dupe:<property>`; `*.ext` answered from the extension index, sorted or not |
| Archive service (`.zidx`, `.dzx`, scheduler, SIMD inflate) | Done |
| Restart catch-up | FSEvents (macOS), USN journal (Windows), timestamp scan (Linux, Windows fallback); 9/9 change checks on Windows |
| NTFS file table crawl (`indexer.use_mft`) | Works on real volumes; 33 s against 650 s for the walk over all of `C:\` (1.43M entries); slower for a small root; `auto` (default) uses it for whole-drive roots |
| Services | Windows Service, systemd unit, launchd job (`install-service`, `--user` or `--system`); runs the extensions. User units and LaunchAgents are not tested in CI |
| `rar2zip` extension | Done; host keys checked (`strict` by default); moved-in and pre-existing archives picked up |
| `sftpd` extension | Done; public-key SFTP server confined to one folder |

Nothing is open. The last round (Nov 2026) closed section 7 of the
performance plan:

- Four settings that were parsed but never read now work:
  `indexer.crawl_threads`, `indexer.content_threads` (content and archive
  work moved off the walker threads onto a bounded worker pool),
  `exclusions.skip_network_content` and `exclusions.max_content_size_bytes`.
- SQLite writes go through one write connection. Full-text adds and deletes
  no longer serialise on one lock. Per-row archive upserts reuse their
  prepared statement.
- rar2zip skips re-hashing a file it already queued unchanged, and hashes
  off the async runtime.
- `benches/reconcile.rs`: a 1M-row re-crawl went from 29.2 s to 5.9 s once
  unchanged rows stopped rewriting every secondary index; peak RSS ~100 MiB.
- `indexer.use_mft` defaults to `"auto"`: the file table crawl for
  whole-drive roots, the walk elsewhere.
- `store.mode` defaults to `"auto"`, the memory store (an existing SQLite
  index is kept). Over all of C: against voidtools Everything on the same
  runner: restart 1.1 s against 0.6 s, substring search faster, `parent:`
  61 ms against ~45, 15 MB on disk against 53, about 35 MB more memory. See
  the [assessment](plans/performance-assessment.md#closing-the-gaps).
- SQLite restarts took 40-100 s at 1.4M rows because a migration rebuilt an
  index on every open; they now take 0.5 s.

The same pass fixed watcher bugs. A renamed folder lost its contents, a
moved-in folder was indexed empty, and a folder moved out left its rows.
Content indexed from watcher events was not searchable until the next crawl,
because the commit loop never reloaded the reader. Details are in the
[performance plan](plans/performance-memory-update.md#section-7-status-nov-2026).

### Testing on Windows

From an elevated PowerShell in a checkout (needs Rust and MSVC):

```powershell
cargo build --release --workspace
cargo test --workspace
# Crawl, query load and restart catch-up; add -UseMft for the file table crawl.
.\scripts\bench-windows.ps1 -TargetDir C:\Windows\System32
.\scripts\bench-windows.ps1 -TargetDir C:\Windows\System32 -UseMft
# Windows Service (FsDaemon): installs and starts it, then stops and removes it.
.\target\release\fsd.exe --config C:\path\config.toml install-service
.\target\release\fsd.exe uninstall-service
```

Each benchmark run prints a summary and exits 1 if a catch-up check fails.
Without elevation, catch-up uses the timestamp scan and the file table crawl
falls back to the walk. Existing Windows indexes do one full crawl after
upgrading, because stored watch roots no longer carry the `\\?\` prefix.

## Overview

- [Development guide](DEVELOPMENT.md) - toolchain, build, run, test, lint,
  change checklists, benchmarks, helper scripts
- [Benchmarks](BENCHMARKS.md) - crawl, memory, query latency, `index.bin`
  and binary sizes (Nov 2026)
- [Overview](overview.md) - workspace crates (`common`, `fs-protocol`,
  `fs-server` daemon, `fs-cli` client, `rar2zip` and `sftpd` extensions),
  archive service summary, resolved decisions Q1–Q6 and AS1–AS6

## Reference

- [fs Modules](reference/fs-server-modules.md) - config, SQLite
  schema, Tantivy, indexer, crawler, archive, APIs, TUI, daemon
- [Query Language](reference/query-language.md) - Everything-compatible
  syntax, AST, parser, evaluator, `fuzzy:`, archive modifiers, acceptance tests
- [rar2zip Modules](reference/rar2zip-modules.md) - config, queue, intake,
  convert, transfer, metrics, service, CLI
- [ZIP Inner Streaming](reference/zip-inner-streaming.md) - `!/` virtual
  paths, `archive_path` column, `archive.rs`, crawler/query/API/TUI changes

## Phases

### Core (1–7)

1. [Workspace & Foundation](phases/01-workspace-foundation.md) - workspace
   Cargo.toml, `crates/common`, error types, config structs, VERSIONS.md
2. [Data Stores](phases/02-data-stores.md) - SQLite + Tantivy stores (incl.
   `archive_path`, `parent`, `watch_root`, `crawl_generation`), rar2zip job queue
3. [Indexer & Watcher](phases/03-indexer-and-watcher.md) - exclusions, content
   extraction, mark-and-sweep parallel crawler, archive iterator, watchers,
   rar2zip intake
4. [Conversion & Transfer](phases/04-conversion-and-transfer.md) - streaming
   conversion, `select_strategy()`, `stream_convert_and_transfer()`, metrics,
   dashboard, alerts
5. [Query Engine](phases/05-query-engine.md) - parser, `fuzzy:`, `archive:`,
   `noarchive:`, `archiveof:`, AST, evaluator
6. [APIs & Service Layer](phases/06-apis-and-service.md) - REST/gRPC/IPC,
   inner-file open, daemon orchestration, Windows Service
7. [Testing & Docs](phases/07-testing-and-docs.md) - integration tests,
   benchmarks, READMEs, release checklist

### Archive service (8–13)

8. [.zidx Builder](phases/08-zidx-builder.md) - binary inode index,
   `ZidxBuilder` + mmap `ZidxIndex`
9. [Archive Service API](phases/09-archive-service-api.md) -
   `/archive/:id/{index,dir,file,search}`, `ArchiveRegistry`, LRU cache
10. [Streaming ZIP Core](phases/10-streaming-zip-core.md) - `ByteSource` /
    `StreamExtractor`, backpressure channel
11. [Deflate Random Access](phases/11-deflate-random-access.md) - `.dzx`
    sidecar (Option B), `read_range()` for HTTP `Range`
12. [Scheduler & Isolation](phases/12-scheduler-and-isolation.md) - semaphore
    → token bucket → two-level DRR, Prometheus metrics
13. [SIMD & Benchmarks](phases/13-simd-and-benchmarks.md) - AVX2 match-copy,
    ring buffer, fast Huffman table, criterion suite

## Plans

- [Performance & Memory Update](plans/performance-memory-update.md) -
  implemented, section 7 included; bounded crawl/fuzzy/content search,
  stream fan-out, postcard IPC, content worker pool, single SQLite writer
- [Performance Assessment: Events and Crawl](plans/performance-assessment.md) -
  measured before and after: ordered batched watcher events, indexed subtree
  queries, `globset` exclusions, idle commits skipped, walk threads,
  `use_mft = "auto"` for large roots
- [Borrowing Everything's Techniques](plans/everything-borrowing.md) - phases
  1-5 and 7 implemented (snapshot metadata, name-only fuzzy, compact memory
  store with a parent-pointer tree, SQLite bulk load, change catch-up on
  restart including USN journal replay, optional SQLite indexes, `*.ext`
  answered from the extension index); platform enumeration measured and not
  built; NTFS file table crawl built behind `indexer.use_mft` and run on a
  real volume in CI (slower than the walk unless the root is most of a volume)

## Research notes

| Note                                                                 | Fed into              |
| -------------------------------------------------------------------- | --------------------- |
| [In-memory index](research/in-memory-index.md)                       | `store/memory_store`  |
| [Everything internals](research/everything-internals.md)             | Everything plan       |
| [Shutdown persistence](research/shutdown-persistence.md)             | daemon shutdown       |
| [Archive service](research/archive-service.md)                       | Phase 9               |
| [Backend design](research/backend-design.md)                         | Phases 8-9            |
| [.zidx builder](research/zidx.md)                                    | Phase 8               |
| [.zidx vs .dzx](research/zidx-vs-dzx.md)                             | Phases 8, 11          |
| [Streaming ZIP](research/streaming-zip.md)                           | Phase 10              |
| [Deflate random access](research/deflate-random-access.md)           | Phase 11              |
| [Global scheduler](research/global-scheduler.md)                     | Phase 12              |
| [System-level optimizations](research/system-level-optimizations.md) | Phase 12              |
| [SIMD & benchmarks](research/simd-and-benchmarks.md)                 | Phase 13              |
| [Browser ZIP explorer](research/browser-zip-explorer.md)             | Phases 8-9 (client)   |
