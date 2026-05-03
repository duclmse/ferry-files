# Phase 7 — Final Testing, Docs & Release

**Goal:** Full integration testing, benchmark verification, README writing,
and release build validation for both projects. After this phase both projects
satisfy their respective Definitions of Done from the specs.

**Estimated scope:** ~600 lines of Rust (integration tests) + 2 READMEs.

---

## 7.1 Integration Test Suite

### filesearch Integration Tests

Located in `crates/filesearch/tests/integration.rs` (gated with default feature flag).

```rust
#[tokio::test]
async fn test_zip_inner_file_search() {
    // 1. Create a ZIP fixture containing 3 .rs files and 1 .png
    // 2. Place ZIP in temp watch dir, start daemon
    // 3. Wait for crawl to complete (poll /stats until archive_entries_indexed >= 3)
    // 4. Query "archive: *.rs" → 3 results, all with archive_path set
    // 5. Query "noarchive: *.rs" → 0 results (no real .rs files on disk)
    // 6. Query "archive: content:TODO" → matches files inside ZIP containing TODO
    // 7. POST /open on inner entry → temp file created, no error
    // 8. Delete ZIP → wait 1s → archive_entries_indexed == 0
}

// Test: full crawl → query → results
#[tokio::test]
async fn test_full_crawl_and_search() {
    // 1. Create temp dir with 50 files (various extensions)
    // 2. Start filesearch daemon with that dir as watch_path
    // 3. Wait for crawl to complete (poll /stats until total_files == 50)
    // 4. Query "*.rs" → expect all .rs files returned
    // 5. Query "dm:today" → expect all files (just created)
    // 6. Query "size:tiny" → expect files < 10KB
}

#[tokio::test]
async fn test_incremental_update() {
    // 1. Start daemon with temp dir
    // 2. Create new file → wait 1s → search → file appears
    // 3. Delete file → wait 1s → search → file gone
    // 4. Rename file → both old and new names searchable correctly
}

#[tokio::test]
async fn test_content_search() {
    // 1. Create .rs file with "TODO: fix this"
    // 2. Wait for indexing
    // 3. Query "content:TODO" → file returned with snippet
}

#[tokio::test]
async fn test_everything_query_acceptance() {
    // Run all acceptance queries from spec section 13 + fuzzy: additions
    // All must parse without error
    // Selected ones verified for correctness
}
```

### rar2zip Integration Tests

Located in `crates/rar2zip/tests/integration.rs` (gated with `cfg(feature = "integration")`).

```rust
// Requires: test SFTP server available at env TEST_SFTP_HOST
#[cfg(feature = "integration")]
#[tokio::test]
async fn test_end_to_end_transfer() {
    // 1. Create temp watch folder
    // 2. Start service with test config
    // 3. Copy test .rar fixture to watch folder
    // 4. Wait for job to reach Done status (poll queue)
    // 5. Disk: verify .zip exists on remote via SFTP stat
    //    Stream: verify no .zip was created locally (only source .rar existed)
    // 6. Verify original .rar removed from watch folder (both strategies)
}
```

---

## 7.2 Performance Benchmarks

### filesearch

Verify performance targets using `criterion` benchmarks in
`crates/filesearch/benches/`:

| Benchmark | Target | How to verify |
|-----------|--------|--------------|
| Exact search, 1M files | < 10ms | `criterion` bench |
| `fuzzy:` search, 1M files | < 50ms | `criterion` bench — nucleo SIMD scorer |
| Regex search, 1M files | < 200ms | `criterion` bench |
| Content search | < 100ms | `criterion` bench |

Benchmark setup: populate SQLite with 1M synthetic file records before
running each bench case.

### rar2zip

Manual verification (no automated bench):
- Convert a 100MB `.rar` (Disk strategy) → measure wall-clock time.
- Transfer 100MB via Stream strategy → verify no zip created locally.
- Transfer 100MB file at 10 MB/s → verify bandwidth throttle holds.

---

## 7.3 filesearch README

Covers all 10 sections from spec section 25:

1. **Overview** — what it does, architecture ASCII diagram.
2. **Prerequisites** — Rust 1.78+, platform-specific notes:
   - Linux: `sysctl fs.inotify.max_user_watches=524288`
   - macOS: no per-file watch limit
   - Windows: Named Pipes, no `unrar.dll` required
3. **Build** — `cargo build --release`
4. **Configuration** — full table of every `config.toml` field with type, default, description.
5. **Running the Daemon** — `filesearch daemon`, background/service setup per OS.
6. **Searching** — all CLI examples, full query syntax reference table.
7. **APIs** — REST endpoint reference, gRPC with `grpcurl` examples, IPC protocol.
8. **TUI** — keybindings reference.
9. **Performance Tuning** — inotify limits, crawl threads, commit interval.
10. **Troubleshooting** — common errors:
    - inotify limit hit
    - port in use
    - socket permission denied
    - `grpcurl` reflection not found

---

## 7.4 rar2zip README

Covers all 8 sections from spec section 18:

1. **Overview** — what the tool does, architecture diagram (ASCII).
2. **Prerequisites**:
   - Rust 1.78+
   - `unrar.dll` on PATH (from WinRAR installation)
   - SSH key pair installed on remote
   - `sha256sum` available on remote (standard on Linux)
3. **Build** — `cargo build --release`
4. **Configuration** — full table of every `config.toml` field.
5. **Running** — CLI usage for all subcommands with examples.
6. **Windows Service** — install/uninstall with `install-service.ps1`.
7. **Monitoring** — Prometheus scrape config snippet, dashboard URL.
8. **Troubleshooting**:
   - `unrar.dll not found`
   - SSH auth failure
   - `sha256sum: command not found` on remote
   - Port already in use

---

## 7.5 Example Config Files

Both `crates/filesearch/config.toml` and `crates/rar2zip/config.toml` are written
with every field documented inline, including `multipart_timeout_hours`.
They serve as the primary reference for users.

---

## 7.6 Final Release Checklist

### Both projects

- [ ] `cargo build --release` succeeds with zero warnings (workspace).
- [ ] `cargo test` passes all unit tests (workspace).
- [ ] `cargo clippy -- -D warnings` produces no output.
- [ ] `cargo doc --no-deps` generates docs without warnings.
- [ ] No `unwrap()` or `todo!()` in any production path.
- [ ] No `#[allow(dead_code)]` or `#[allow(unused)]` in final code.
- [ ] `cargo check -p common` passes.

### filesearch

- [ ] All modules listed in spec section 2 implemented (no stubs).
- [ ] `query/exact.rs`, `query/fuzzy.rs`, `query/regex_search.rs` do not exist.
- [ ] `filesearch daemon` starts, crawls, accepts queries via all three APIs.
- [ ] ZIP inner files are indexed during crawl and watcher events.
- [ ] `filesearch tui` launches interactive TUI; inner entries show `[zip]` tag.
- [ ] `filesearch search "main.rs"` returns results in < 50ms on 1M file index.
- [ ] `filesearch search "fuzzy:mian"` returns `main.rs` as a top result.
- [ ] `filesearch search "archive: *.rs"` returns only inner-archive entries.
- [ ] `POST /open` on inner entry extracts to temp and opens.
- [ ] `filesearch validate-config` exits 0 on valid config, 1 on invalid.
- [ ] README covers all 10 sections + ZIP inner-file section.
- [ ] `VERSIONS.md` populated with all resolved crate versions (incl. `zip`, `tempfile`).
- [ ] Builds on Windows, Linux, and macOS.

### rar2zip

- [ ] All modules listed in spec section 1 implemented (no stubs).
- [ ] `validate-config` exits 0 on valid `config.toml`.
- [ ] `multipart_timeout_hours` respected: expired groups moved to `failed/`.
- [ ] `Strategy::Stream` confirmed: no zip file created for large archives.
- [ ] README covers all 8 sections.
- [ ] Builds on Windows only (Windows Service code must compile).
- [ ] `VERSIONS.md` populated with all resolved crate versions.
- [ ] Integration test passes for both Disk and Stream strategies.

---

## 7.7 Definition of Done

This phase is done when both projects satisfy every checkbox in their
respective spec Definitions of Done (spec sections 20 and 24).
