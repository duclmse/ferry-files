# Project Memory Index

- [Overview](00-overview.md) — Three-crate workspace: `common` + `filesearch` daemon + `rar2zip` Windows service; resolved decisions Q1–Q6
- [Phase 1 — Workspace & Foundation](01-phases/01-phase1-workspace-foundation.md) — Workspace Cargo.toml, `crates/common`, error types, config structs, VERSIONS.md template
- [Phase 2 — Data Stores](01-phases/02-phase2-data-stores.md) — filesearch SQLite+Tantivy stores (incl. `archive_path` column), rar2zip job queue with `get_expired_groups()`
- [Phase 3 — Indexer & Watcher](01-phases/03-phase3-indexer-and-watcher.md) — Exclusions, content extraction, parallel crawler, `archive.rs` ZIP inner crawler, watchers, rar2zip intake
- [Phase 4 — Conversion & Transfer](01-phases/04-phase4-conversion-and-transfer.md) — Streaming conversion, `stream_convert_and_transfer()` (no zip on disk), metrics, dashboard, alerts
- [Phase 5 — Query Engine](01-phases/05-phase5-query-engine.md) — Everything-compatible parser, `fuzzy:`, `archive:`, `noarchive:`, `archiveof:` functions, AST, evaluator
- [Phase 6 — APIs & Service Layer](01-phases/06-phase6-apis-and-service.md) — REST/gRPC/IPC APIs, inner-file open (temp extract), daemon orchestration, Windows Service
- [Phase 7 — Testing & Docs](01-phases/07-phase7-testing-and-docs.md) — Integration tests (incl. ZIP inner), benchmarks, READMEs, final release checklist
- [Ref: filesearch Modules](02-ref/01-ref-filesearch-modules.md) — All module specs: config, SQLite schema, Tantivy, indexer, crawler, archive, APIs, TUI, daemon
- [Ref: Query Language](02-ref/02-ref-query-language.md) — Complete Everything-compatible syntax, AST, parser, evaluator, `fuzzy:`, archive modifiers, acceptance tests
- [Ref: rar2zip Modules](02-ref/03-ref-rar2zip-modules.md) — All module specs: config, queue, intake, convert, transfer, metrics, service, CLI
- [Ref: ZIP Inner Streaming](02-ref/04-ref-zip-inner-streaming.md) — Virtual paths, config, SQLite changes, archive.rs, crawler integration, query/API/TUI changes
