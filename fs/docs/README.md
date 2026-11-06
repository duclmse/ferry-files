# Documentation Index

Start with the [overview](overview.md). When a reference doc and a phase or
research doc disagree, the **reference** doc wins.

| Folder                     | What it holds                                         | Authority         |
| -------------------------- | ----------------------------------------------------- | ----------------- |
| [`reference/`](reference/) | Module contracts, schemas, query grammar              | Authoritative     |
| [`phases/`](phases/)       | Phased implementation plan with implementation notes  | Design intent     |
| [`plans/`](plans/)         | Cross-cutting change plans / agent handoffs           | Historical        |
| [`research/`](research/)   | Exploratory design notes that fed the phase docs      | Not a spec        |

## Overview

- [Overview](overview.md) - three-crate workspace (`common`, `filesearch`
  daemon, `rar2zip` Windows service), archive service summary, resolved
  decisions Q1–Q6 and AS1–AS6

## Reference

- [filesearch Modules](reference/filesearch-modules.md) - config, SQLite
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
  1-3 implemented (snapshot metadata, name-only fuzzy, compact memory store);
  SQLite bulk load, OS change-log catch-up, fast enumeration open

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
