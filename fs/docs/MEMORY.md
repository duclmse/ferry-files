# Project Memory Index

- [Overview](00-overview.md) — Three-crate workspace: `common` + `filesearch`
  daemon + `rar2zip` Windows service; resolved decisions Q1–Q6

- [Phase 1 — Workspace & Foundation](01-phases/01-phase1-workspace-foundation.md)
  — Workspace Cargo.toml, `crates/common`, error types, config structs,
  VERSIONS.md template

- [Phase 2 — Data Stores](01-phases/02-phase2-data-stores.md) — filesearch
  SQLite+Tantivy stores (incl. `archive_path`, `parent`, `watch_root`,
  `crawl_generation` columns), rar2zip job queue

- [Phase 3 — Indexer & Watcher](01-phases/03-phase3-indexer-and-watcher.md) —
  Exclusions, content extraction, mark-and-sweep parallel crawler (no full path
  list in memory), streaming archive iterator, watchers, rar2zip intake

- [Phase 4 — Conversion & Transfer](01-phases/04-phase4-conversion-and-transfer.md)
  — Streaming conversion, `select_strategy()` (Disk for multi-destination),
  `stream_convert_and_transfer()` (no zip on disk), metrics, dashboard, alerts

- [Phase 5 — Query Engine](01-phases/05-phase5-query-engine.md) —
  Everything-compatible parser, `fuzzy:`, `archive:`, `noarchive:`, `archiveof:`
  functions, AST, evaluator

- [Phase 6 — APIs & Service Layer](01-phases/06-phase6-apis-and-service.md) —
  REST/gRPC/IPC APIs, inner-file open (temp extract), daemon orchestration,
  Windows Service

- [Phase 7 — Testing & Docs](01-phases/07-phase7-testing-and-docs.md) —
  Integration tests (incl. ZIP inner), benchmarks, READMEs, final release
  checklist

- [Ref: filesearch Modules](02-ref/01-ref-filesearch-modules.md) — All module
  specs: config, SQLite schema, Tantivy, indexer, crawler, archive, APIs, TUI,
  daemon

- [Ref: Query Language](02-ref/02-ref-query-language.md) — Complete
  Everything-compatible syntax, AST, parser, evaluator, `fuzzy:`, archive
  modifiers, acceptance tests

- [Ref: rar2zip Modules](02-ref/03-ref-rar2zip-modules.md) — All module specs:
  config, queue, intake, convert, transfer, metrics, service, CLI

- [Ref: ZIP Inner Streaming](02-ref/04-ref-zip-inner-streaming.md) — Virtual
  paths, config, SQLite changes, archive.rs, crawler integration, query/API/TUI
  changes

---

## Archive Service Phases (8–13)

- [Phase 8 — .zidx Builder](01-phases/08-phase8-zidx-builder.md) — Binary inode
  index (Header + Node table + String pool); `ZidxBuilder` + mmap `ZidxIndex`;
  called from `crawl_archive()` when `archive_service.enabled`

- [Phase 9 — Archive Service API](01-phases/09-phase9-archive-service-api.md) —
  Axum routes `/archive/:id/{index,dir,file,search}`; `ArchiveRegistry` with LRU
  index cache; `ArchiveServiceConfig` added to `Config`

- [Phase 10 — Streaming ZIP Core](01-phases/10-phase10-streaming-zip-core.md) —
  `ByteSource` + `StreamExtractor` traits; `LocalFileSource`, `MmapSource`;
  `ZipStreamExtractor` with backpressure channel; replaces inline inflate in
  Phase 9

- [Phase 11 — Deflate Random Access](01-phases/11-phase11-deflate-random-access.md)
  — `.dzx` sidecar block index (Option B only — preserves original bytes);
  `DzxBuilder` + `DzxIndex`; `read_range()` for HTTP `Range` support

- [Phase 12 — Scheduler & Multi-tenant Isolation](01-phases/12-phase12-scheduler-and-isolation.md)
  — Per-archive semaphore (step 1) → per-client token bucket (step 2) → two-level
  DRR (step 3); `SchedulerConfig`; Prometheus metrics for queue wait and inflight

- [Phase 13 — SIMD Pipeline & Benchmarks](01-phases/13-phase13-simd-and-benchmarks.md)
  — AVX2 match-copy engine; double ring buffer; fast 9-bit Huffman table; criterion
  benchmarks for inflate throughput, range read latency, scheduler dispatch
