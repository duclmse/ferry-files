Great scope - full Everything clone, cross-platform, Windows-first. Here's a
comprehensive implementation plan:

---

## Architecture Overview

```
everything-rs/
├── crates/
│   ├── core/          # Platform-agnostic index, search, persistence
│   ├── platform/      # Platform-specific scanner + watcher backends
│   ├── ipc/           # Named pipe / Unix socket service protocol
│   └── cli/           # Binary: daemon + query CLI
```

Keeping platform code in its own crate makes `core` fully testable on any OS and
lets you swap backends cleanly.

---

## Crate-by-Crate Plan

### `core` - Index, Search, Persistence

**Data model:**

```rust
// Compact record - target ~64 bytes per entry
pub struct FileRecord {
    pub id: u64,          // MFT record number (Windows) or inode (Linux/Mac)
    pub parent_id: u64,
    pub name: NameRef,    // offset into string arena
    pub size: u64,
    pub flags: FileFlags, // is_dir, is_hidden, etc.
    pub modified: u64,    // epoch seconds, u32 would suffice too
}

pub struct NameRef {
    pub offset: u32,
    pub len: u16,
}
```

Store records in a `Vec<FileRecord>` (flat array, cache-friendly). Names go into
a separate `Vec<u8>` arena - one big contiguous string pool. At 1M files this is
roughly 80–120 MB total, matching Everything's footprint.

**Path reconstruction:**

Don't store full paths - reconstruct on demand by walking `parent_id` up the
chain to a root sentinel. Cache a `HashMap<u64, u32>` (id → index) for O(1)
parent lookups.

**Search:**

Start with a simple linear scan with `memchr`/SIMD substring matching. The
`memchr` crate already uses SSE2/AVX2 automatically. For the future, you can add
a trigram index for regex queries.

```rust
pub fn search<'a>(
    index: &'a Index,
    query: &Query,
) -> impl Iterator<Item = SearchResult<'a>>
```

`Query` should be an enum covering: `Substring`, `Glob`, `Regex`,
`Compound(And/Or)`, plus filter predicates for size, modified date, and flags.

**Persistence:**

Use a simple custom binary format rather than SQLite - it's faster to mmap on
load:

```
[Header: magic + version + record_count + arena_len]
[FileRecord × N]          // fixed-size, directly deserializable
[string arena bytes]      // variable
[USN checkpoint: u64]     // last seen USN / inotify position
```

Use `zerocopy` or `bytemuck` for zero-copy deserialization. Target <100ms load
time for 1M records.

---

### `platform` - Scanner + Watcher Backends

Define a trait that both Windows and Linux/macOS backends implement:

```rust
pub trait VolumeScanner {
    fn scan(&self) -> Result<impl Iterator<Item = RawEntry>>;
}

pub trait ChangeWatcher {
    fn watch(&self, since: Checkpoint)
        -> Result<impl Stream<Item = ChangeEvent>>;
}

pub struct RawEntry {
    pub id: u64,
    pub parent_id: u64,
    pub name: String,
    pub size: u64,
    pub flags: FileFlags,
    pub modified: u64,
}

pub enum ChangeEvent {
    Created(RawEntry),
    Deleted { id: u64 },
    Renamed { id: u64, new_name: String, new_parent_id: u64 },
    Modified { id: u64, new_size: u64, new_modified: u64 },
}
```

**Windows backend:**

The trickiest part. You'll need `windows-rs` (Microsoft's official Rust
bindings).

_Initial scan - MFT enumeration:_

```rust
// Open volume with FILE_READ_DATA on the root
let vol = CreateFileW(r"\\.\C:", FILE_READ_DATA, ...);

// Get volume data to find MFT location
DeviceIoControl(vol, FSCTL_GET_NTFS_VOLUME_DATA, ...);

// Enumerate via USN_RECORD_V2 - much simpler than raw MFT parsing
DeviceIoControl(vol, FSCTL_ENUM_USN_DATA, ...);
// yields USN_RECORD_V2 records: FileReferenceNumber, ParentFileReferenceNumber, FileName
```

`FSCTL_ENUM_USN_DATA` gives you exactly the fields you need without parsing raw
MFT structures. Much safer than reading `$MFT` directly.

_Change watching - USN journal:_

```rust
DeviceIoControl(vol, FSCTL_READ_USN_JOURNAL, &READ_USN_JOURNAL_DATA {
    StartUsn: last_checkpoint,
    ReasonMask: USN_REASON_FILE_CREATE | USN_REASON_FILE_DELETE
              | USN_REASON_RENAME_NEW_NAME | USN_REASON_DATA_OVERWRITE,
    ...
}, ...);
```

Poll this on a background thread every 500ms–1s. No need for OS-level push
notifications; the journal is a ring buffer NTFS maintains regardless.

**Linux backend:**

Use `inotify` (via the `inotify` crate) or `fanotify` for recursive watching.
Initial scan is a standard `walkdir` traversal - no special privileges needed.
The `notify` crate (which you've already used) abstracts both and gives you a
unified `Event` stream, so you could lean on that here.

**macOS backend:** FSEvents via the `fsevent` crate or again via `notify`.

---

### `ipc` - Service Protocol

The daemon holds the index in memory; CLI queries talk to it over:

- **Windows:** Named pipe (`\\.\pipe\everything-rs`)
- **Linux/macOS:** Unix domain socket (`/run/everything-rs.sock`)

Keep the protocol simple - length-prefixed messages, serialized with `bincode`
or `postcard`:

```rust
#[derive(Serialize, Deserialize)]
pub enum Request {
    Search(QueryParams),
    Ping,
    Shutdown,
    Stats,
}

#[derive(Serialize, Deserialize)]
pub enum Response {
    Results(Vec<SearchResultItem>),
    Pong,
    Stats(IndexStats),
    Error(String),
}
```

Stream results back in pages (e.g. 500 at a time) so the CLI can display
incrementally.

On Windows, the service runs as a Windows Service (use the `windows-service`
crate). On Linux, it's a systemd unit or a simple background process.

---

### `cli` - User-Facing Binary

Two subcommands:

```
# Start the daemon (or install as service)
everything-rs daemon [--foreground]

# Query
everything-rs search "*.rs" [--size ">1MB"] [--modified "today"] [--limit 50]
everything-rs search --regex "foo.*\.log$"
```

Use `clap` for argument parsing. Output as a table (`tabled` or `comfy-table`)
or `--json` for scripting.

---

## Key Crates

| Purpose                 | Crate                            |
| ----------------------- | -------------------------------- |
| Windows APIs            | `windows` (microsoft/windows-rs) |
| Cross-platform watching | `notify`                         |
| Async runtime           | `tokio`                          |
| IPC serialization       | `postcard` or `bincode`          |
| CLI args                | `clap`                           |
| Zero-copy structs       | `bytemuck` or `zerocopy`         |
| SIMD string search      | `memchr`                         |
| Regex                   | `regex`                          |
| Windows service         | `windows-service`                |
| Glob matching           | `globset`                        |

---

## Build & Feature Flag Strategy

```toml
# platform/Cargo.toml
[features]
default = []

[target.'cfg(target_os = "windows")'.dependencies]
windows = { version = "0.58", features = ["Win32_Storage_FileSystem", ...] }

[target.'cfg(target_os = "linux")'.dependencies]
inotify = "0.10"

[target.'cfg(target_os = "macos")'.dependencies]
fsevent = "..."
```

This keeps platform deps from bleeding into non-target builds.

---

## Recommended Build Order

1. **`core`** - `FileRecord`, arena, flat index, linear search, binary
   persistence. Fully testable with synthetic data on any OS.
2. **`platform/windows`** - MFT scan via `FSCTL_ENUM_USN_DATA`, USN journal
   watcher.
3. **Wire them together** - index builder that feeds `RawEntry` from scanner
   into `core::Index`.
4. **`cli`** - basic `search` subcommand, no daemon yet. Scan → index → query in
   one process.
5. **`ipc` + daemon** - split into service + client, add persistence so the
   daemon survives restarts.
6. **`platform/linux` + `platform/macos`** - drop-in via the trait.

---

## Gotchas to Plan For

- **MFT record number width:** The high 16 bits of `FileReferenceNumber` are a
  sequence number - mask them off with `& 0x0000_FFFF_FFFF_FFFF` to get the
  actual record number.
- **Elevation on Windows:** MFT access requires admin. Build in a clear error
  message and a `--install-service` path early.
- **USN journal wraparound:** If the daemon is offline long enough, journal
  entries can be overwritten. Detect this (`ERROR_JOURNAL_ENTRY_DELETED`) and
  fall back to a full rescan.
- **Rename pairs:** USN emits `RENAME_OLD_NAME` and `RENAME_NEW_NAME` as
  separate records. You need to correlate them by `FileReferenceNumber` within
  the same poll batch.
- **Hard links:** One MFT record can have multiple names. Decide early whether
  to index all names or just the primary one.

---

This gives you a solid, layered foundation. Want me to dive into any specific
layer - for example, the actual MFT enumeration code, the index data structures,
or the IPC protocol?
