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
and installs, starts, searches through and stops the Windows Service.

| Area | State |
| ---- | ----- |
| `fsd` daemon, `fs` client, query engine | Done; Everything-compatible syntax incl. `dupe:<property>` |
| Archive service (`.zidx`, `.dzx`, scheduler, SIMD inflate) | Done |
| Restart catch-up | FSEvents (macOS), USN journal (Windows), timestamp scan (Linux, Windows fallback); 9/9 change checks on Windows |
| NTFS file table crawl (`indexer.use_mft`) | Works on real volumes; off by default, slower than the walk unless the root is most of a volume |
| Windows Service | Installs, starts, stops; runs the extensions |
| `rar2zip` extension | Done; host keys checked (`strict` by default); moved-in and pre-existing archives picked up |
| `sftpd` extension | Done; public-key SFTP server confined to one folder |

Open, none blocking:

- Timing the file table crawl on a whole-volume root (e.g. `C:\`), where it
  should beat the walk.
- One cold `parent:C:\Windows\System32` query took 9.5 s in CI (later ones
  4–17 ms); not investigated.
- `*.ext` globs use the extension index in SQLite only; sorted queries still
  read every match.

### Testing on Windows

From an elevated PowerShell in a checkout (needs Rust and MSVC):

```powershell
cargo build --release --workspace
cargo test --workspace
# Crawl, query load and restart catch-up; add -UseMft for the file table crawl.
.\scripts\bench-windows.ps1 -TargetDir C:\Windows\System32
.\scripts\bench-windows.ps1 -TargetDir C:\Windows\System32 -UseMft
# Windows Service (FsDaemon), with a config whose paths are absolute.
.\target\release\fsd.exe --config C:\path\config.toml install-service
Start-Service FsDaemon; Stop-Service FsDaemon
.\target\release\fsd.exe uninstall-service
```

Each benchmark run prints a summary and exits 1 if a catch-up check fails.
Without elevation, catch-up uses the timestamp scan and the file table crawl
falls back to the walk. Existing Windows indexes do one full crawl after
upgrading, because stored watch roots no longer carry the `\\?\` prefix.

## Overview

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
  implemented; bounded crawl/fuzzy/content search, stream fan-out, postcard IPC
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
