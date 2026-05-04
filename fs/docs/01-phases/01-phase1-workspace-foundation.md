# Phase 1 — Workspace & Foundation

**Goal:** Cargo workspace scaffolding, `crates/common` shared library, error
types, config structs, and logging. No business logic yet — just the skeleton
that every later phase builds on. After this phase all three crates compile
cleanly with `cargo check`.

**Estimated scope:** ~700 lines of Rust + TOML config.

---

## 1.1 Workspace Setup

### `zipfs/Cargo.toml` (workspace root)

```toml
[workspace]
resolver = "2"
members = [
    "crates/common",
    "crates/filesearch",
    "crates/rar2zip",
]

[workspace.dependencies]
# Pin shared deps here; member crates reference with { workspace = true }
tokio              = { version = "1", features = ["full"] }
serde              = { version = "1", features = ["derive"] }
serde_json         = "1"
toml               = "1"
tracing            = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
tracing-appender   = "0.2"
thiserror          = "1"
anyhow             = "1"
chrono             = { version = "0.4", features = ["serde"] }
rusqlite           = { version = "0.31", features = ["bundled"] }
r2d2               = "0.8"
r2d2_sqlite        = "0.23"
ssh2               = "0.9"
openssl            = { version = "0.10", features = ["vendored"] }
clap               = { version = "4", features = ["derive"] }
glob               = "0.3"
uuid               = { version = "1", features = ["v4"] }
```

> All other crate versions are resolved at implementation time to latest stable.
> Record each resolved version in the crate's `VERSIONS.md`.

---

## 1.2 `crates/common/`

### `crates/common/Cargo.toml`

```toml
[package]
name    = "common"
version = "0.1.0"
edition = "2021"

[dependencies]
tokio              = { workspace = true }
serde              = { workspace = true }
thiserror          = { workspace = true }
anyhow             = { workspace = true }
tracing            = { workspace = true }
tracing-subscriber = { workspace = true }
tracing-appender   = { workspace = true }
chrono             = { workspace = true }
```

### `crates/common/src/lib.rs`

```rust
pub mod error;
pub mod logger;
```

### `crates/common/src/error.rs`

```rust
/// Marker trait for all project errors.
pub trait ProjectError: std::error::Error + Send + Sync + 'static {}

/// Initialisation errors shared across both crates.
#[derive(thiserror::Error, Debug)]
pub enum CommonError {
    #[error("Config error: {0}")]
    Config(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Logger init error: {0}")]
    Logger(String),
}
```

### `crates/common/src/logger.rs`

```rust
use std::path::Path;
use tracing_appender::rolling;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};
use crate::error::CommonError;

/// Initialise tracing with JSON format, stdout + rolling daily file.
/// Level controlled by RUST_LOG env var (default: info).
/// Call once at startup before any async runtime is created.
pub fn init(log_dir: &Path) -> Result<(), CommonError> {
    std::fs::create_dir_all(log_dir)
        .map_err(CommonError::Io)?;

    let file_appender = rolling::daily(log_dir, "app.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(fmt::layer().json().with_writer(std::io::stdout))
        .with(fmt::layer().json().with_writer(non_blocking))
        .init();

    Ok(())
}
```

> `_guard` must be kept alive for the lifetime of the program. Return it from
> `init()` and store it in `main()` or the service entry point.

---

## 1.3 Files Created This Phase

### `crates/filesearch/`

| File | Content |
|------|---------|
| `Cargo.toml` | Member crate; deps via `{ workspace = true }`; crate-specific deps (tantivy, axum, tonic, nucleo, etc.) at latest stable |
| `VERSIONS.md` | Template with columns: Crate, Spec version, Resolved version, API deviations |
| `src/error.rs` | `AppError` enum: Io, Config, Index, Query, Database, Tantivy, Grpc, Ipc, Content; `From<CommonError>` impl |
| `src/config.rs` | `Config`, `ExclusionConfig`, `ContentConfig`, `ApiConfig`, `SearchConfig`, `IndexerConfig`, `MacroConfig` structs; `load()`, `validate()`, `watch_config()` |
| `src/main.rs` | Stub: parse CLI args, print version, exit 0 |

### `crates/rar2zip/`

| File | Content |
|------|---------|
| `Cargo.toml` | Member crate; deps via `{ workspace = true }`; crate-specific deps (unrar, sevenz-rust, zip, windows-service, etc.) at latest stable |
| `VERSIONS.md` | Same template as filesearch |
| `src/error.rs` | `AppError` enum: Io, Config, Conversion, Transfer, Database, Ssh, Alert; `From<CommonError>` impl |
| `src/config.rs` | `Config`, `ConversionConfig`, `Destination`, `TransferConfig`, `ObservabilityConfig`, `SmtpConfig` structs + `IndexerConfig` with `multipart_timeout_hours`; `load()`, `validate()`, `watch_config()` |
| `src/main.rs` | Stub CLI with all subcommands via clap |

> **No `logger.rs` in either crate.** Both call `common::logger::init(log_dir)`
> from their entry point (main or service_main).

---

## 1.4 `VERSIONS.md` Template

Create at `crates/filesearch/VERSIONS.md` and `crates/rar2zip/VERSIONS.md`:

```markdown
# Resolved Crate Versions

Generated at implementation time. Update whenever a dep is added or changed.

| Crate | Spec version | Resolved version | API deviations |
|-------|-------------|-----------------|----------------|
| tokio | 1 | x.y.z | none |
| tantivy | 0.22 | x.y.z | list any breaking changes |
| ... | | | |

## Notes

- Any entry in the "API deviations" column must have a corresponding
  comment in the affected source file: `// VERSIONS: <crate> <note>`
```

---

## 1.5 `AppError` + `CommonError` Integration

Both crate `error.rs` files include:

```rust
use common::error::CommonError;

impl From<CommonError> for AppError {
    fn from(e: CommonError) -> Self {
        AppError::Config(e.to_string())
    }
}
```

This allows `?` to propagate logger and config init errors through `AppError`.

---

## 1.6 Config Validation Rules

### filesearch config validation
- All `watch_paths` must be absolute paths that exist and are directories.
- `data_dir` is created if absent; must be writable.
- `rest_port != grpc_port`.
- `fuzzy_threshold` must be in `[0.0, 1.0]`.
- `crawl_threads > 0` and `content_threads > 0`.

### rar2zip config validation
- `watch_folder` must exist and be a directory.
- All `ssh_key_path` files must exist.
- `failed_folder` is created if absent.
- `metrics_port != dashboard_port`.
- `compression_level` in `[0, 9]`.
- `multipart_timeout_hours` is any non-negative integer (0 = disabled).

---

## 1.7 Config Hot-Reload (both crates)

Both `watch_config()` functions follow the same pattern:
1. Set up a `notify` watcher on the config file path.
2. On `Modify` event: re-parse and re-validate the file.
3. If valid: `*config.write() = new_config;` — atomically swap.
4. If invalid: log the error, keep current config.
5. On `shutdown` cancel: exit cleanly.

---

## 1.8 `rar2zip` Config Addition — `multipart_timeout_hours`

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct IndexerConfig {
    /// Hours to wait for missing parts before expiring the group.
    /// 0 = never expire (manual cleanup required). Default: 24.
    pub multipart_timeout_hours: u64,
}
```

Add to `config.toml` example:
```toml
[indexer]
multipart_timeout_hours = 24
```

---

## 1.9 Tests — Phase 1

### `crates/filesearch/src/config.rs` tests
```
- load valid config.toml → Ok(Config)
- missing watch_paths field → AppError::Config
- fuzzy_threshold = 1.5 → validate() returns ["fuzzy_threshold must be 0.0–1.0"]
- rest_port == grpc_port → validate() returns error
- hot reload: write updated TOML to temp file → Arc<RwLock<Config>> updated
```

### `crates/rar2zip/src/config.rs` tests
```
- load valid config.toml → Ok(Config)
- missing watch_folder → AppError::Config
- compression_level = 10 → validate() returns error
- metrics_port == dashboard_port → validate() returns error
- multipart_timeout_hours missing → uses default (24)
- hot reload test (same pattern as filesearch)
```

---

## 1.10 Definition of Done

- [x] `cargo check -p common` passes with zero errors.
- [x] `cargo check -p filesearch` passes with zero errors.
- [x] `cargo check -p rar2zip` passes with zero errors.
- [x] `cargo check` (workspace root) passes, no warnings.
- [x] `cargo test -p filesearch config` passes.
- [x] `cargo test -p rar2zip config` passes.
- [x] `filesearch validate-config` and `rar2zip validate-config`
      subcommands call `validate()` and print results.
- [x] `VERSIONS.md` exists in both `crates/filesearch/` and `crates/rar2zip/`.
- [x] No `unwrap()` in any config, error, or logger code.
- [x] No `logger.rs` in either crate (only in `common`).

---

## 1.11 Implementation Notes *(added during Phase 1 implementation)*

### A. `common::logger::init` returns `WorkerGuard`

The spec code showed `_guard` dropped at end of `init()`. The implementation
changes the signature to:

```rust
pub fn init(log_dir: &Path) -> Result<WorkerGuard, CommonError>
```

Callers (`main()` / `service_main()`) must bind the guard:

```rust
let _guard = common::logger::init(&cfg.log_dir)?;
```

Dropping the guard before the process exits silently discards unflushed log
lines. Store it in `main()`'s outermost scope.

### B. `tokio-util` added to workspace dependencies

`CancellationToken` (used in `watch_config` and all later phases) lives in
`tokio_util::sync::CancellationToken`. The workspace `Cargo.toml` declares:

```toml
tokio-util = { version = "0.7" }
```

Note: `tokio-util 0.7` does **not** have a `"sync"` feature flag —
`CancellationToken` is included in the default feature set.

### C. `multipart_timeout_hours` lives in `ConversionConfig`, not a separate `IndexerConfig`

The phase 1 spec draft described an `IndexerConfig` struct for rar2zip that
held only `multipart_timeout_hours`. The reference doc (`03-ref-rar2zip-modules.md`)
places this field — plus `stream_to_transfer_threshold_mb` and
`archive_password` — inside `ConversionConfig`, which is the authoritative
layout. No `IndexerConfig` struct exists in `rar2zip`.

### D. `ArchiveIndexConfig` is a top-level section of `filesearch::Config`

The reference doc adds `archive_index: ArchiveIndexConfig` as a first-class
field on `Config` (not nested inside `IndexerConfig`). The struct tracks
`enabled`, `formats`, `inner_extension_limits`, `max_entries_per_archive`,
`max_nesting_depth`, and `passwords`. The only supported format is `"zip"`;
`validate()` returns an error for any other value.

### E. Both crates have a `lib.rs` + `main.rs` split

Each crate (`filesearch`, `rar2zip`) exposes its modules through a `lib.rs`:

```rust
pub mod config;
pub mod error;
```

`main.rs` imports via `use filesearch::config;` / `use rar2zip::config;`.
This eliminates dead-code warnings for `pub` items that are not yet wired up,
and enables proper unit-test isolation in later phases (tests link against the
library, not the binary).

### F. `ssh2` / `openssl` deferred to Phase 4

Building `openssl` with `features = ["vendored"]` requires **Strawberry Perl**
on Windows (Git's bundled Perl is missing `Locale::Maketext::Simple`). Since
`ssh2` is only used in `transfer.rs` (Phase 4), both deps are declared in the
workspace `Cargo.toml` but **not yet added to `rar2zip`'s `[dependencies]`**.

`rar2zip/src/error.rs` therefore has `Ssh(String)` instead of
`Ssh(#[from] ssh2::Error)`. In Phase 4, when `ssh2 = { workspace = true }` is
added, update that variant to `Ssh(#[from] ssh2::Error)`.

Install Strawberry Perl before Phase 4: <https://strawberryperl.com>

### G. TOML path serialisation on Windows

TOML quoted strings (`"..."`) treat `\` as an escape character. Windows paths
written with `PathBuf::display()` use backslashes, which produce invalid TOML
(e.g. `"C:\Users\"` → parse error on `\U`). All test helpers normalise paths
with:

```rust
let s = path.to_string_lossy().replace('\\', "/");
```

Windows accepts forward-slash paths, so this is safe at runtime too. Config
files written by users on Windows must also use forward slashes or raw TOML
literals (`'C:\path'`).

### H. `r2d2_sqlite` version bumped to `0.24`

`r2d2_sqlite 0.23` requires `rusqlite ^0.30`, which conflicts with
`rusqlite ^0.31` used elsewhere. Version `0.24` supports `rusqlite 0.31`.
VERSIONS.md updated accordingly.

### I. `[profile.release]` moved to workspace root

Cargo ignores `[profile.*]` sections in non-root member `Cargo.toml` files.
The release profile (`opt-level = 3`, `strip = true`) from the rar2zip spec is
declared at the workspace root instead.
