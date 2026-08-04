## The Core Tension

You have two competing goals at shutdown time:

- **Write speed** - you may have only a few seconds before the OS force-kills
  processes
- **Load speed** - next startup should be near-instant, not "re-parsing a 500MB
  file"

---

## Options Ranked for Your Use Case

### 1. Custom Memory-Mapped Binary File ✅ Best fit

Exactly what Everything itself uses. The idea: your in-memory
`Vec<FileRecord>` + string arena are already contiguous blobs - just write them
straight to disk.

```rust
// Save: two fwrite-equivalent calls
fs::write("index.bin", bytemuck::cast_slice(&records))?;
fs::write("arena.bin", &string_arena)?;

// Load: mmap both files, zero copying, zero parsing
let mmap = unsafe { MmapOptions::new().map(&file)? };
let records: &[FileRecord] = bytemuck::cast_slice(&mmap);
```

**Pros:**

- Save: essentially one `memcpy` to disk - completes in ~200ms for 1M records
- Load: `mmap` means the OS pages it in lazily - perceived startup is near-zero
- No dependency, no schema, no query planner overhead

**Cons:**

- You manage versioning/migration yourself (add a header with a schema version)
- Not human-inspectable

**Verdict:** This is the right default. Use `memmap2` crate for the mmap,
`bytemuck` for zero-copy casting.

---

### 2. `redb` ✅ Good if you want structured access

A pure-Rust embedded key-value database, ACID, very fast writes, no C dependency
unlike LMDB or RocksDB.

```rust
let db = Database::create("index.redb")?;
let write_txn = db.begin_write()?;
{
    let mut table = write_txn.open_table(FILE_RECORDS)?;
    for record in &records {
        table.insert(record.id, encode(record))?;
    }
}
write_txn.commit()?;
```

**Pros:**

- Transactional - safe against partial writes on sudden shutdown
- You can do partial updates (only flush changed records, not the whole index)
- Good fit if you later add metadata, tags, or user annotations

**Cons:**

- Slower bulk load than raw mmap (B-tree overhead)
- Overkill if you always load the whole index anyway

**Verdict:** Worth considering if you want incremental saves rather than full
rewrites on shutdown.

---

### 3. SQLite (via `rusqlite`) ⚠️ Probably too slow

SQLite is excellent but its row-oriented storage is a poor fit for bulk-loading
1M+ flat records.

- Full index write: **2–10 seconds** with WAL mode
- Full index read: **3–8 seconds** parsing rows back into structs

That's likely too slow for a clean shutdown window and too slow at startup.
You'd need to use it as a secondary store (e.g. for user-defined tags or search
history), not the primary index.

---

### 4. `sled` ⚠️ Avoid for now

Once a popular embedded Rust DB but it's been largely superseded by `redb`. Its
write performance has known issues under bulk load and it's not actively
maintained at the same pace.

---

### 5. Flat file formats (JSON, MessagePack, etc.) ❌ Too slow

Serialization overhead at 1M records is brutal. Even `postcard` (the fastest
binary serde format in Rust) will cost you 1–3 seconds just in
encoding/decoding. Not suitable as primary storage.

---

## Recommended Strategy

Use a **two-layer approach:**

```
index.bin        ← raw binary dump (mmap on load, full rewrite on clean shutdown)
index.redb       ← USN checkpoint + user metadata + search history
```

| Data                   | Storage                           |
| ---------------------- | --------------------------------- |
| `Vec<FileRecord>`      | Custom binary / mmap              |
| String arena           | Custom binary / mmap              |
| USN/inotify checkpoint | `redb` or just appended to header |
| Schema version, stats  | Binary file header                |
| User tags, bookmarks   | `redb`                            |

On **clean shutdown** (Windows `SERVICE_CONTROL_PRESHUTDOWN` or `SIGTERM`):
write the binary dump. You have roughly 5 seconds in `PRESHUTDOWN` - a 1M-record
dump takes ~200ms, plenty of margin.

On **dirty shutdown** (power loss, kill -9): at startup, check the USN journal
for delta since the last checkpoint and replay it onto the loaded index. This
covers the gap without a full rescan.

---

## The `PRESHUTDOWN` Hook (Windows-specific)

This is the key detail that makes clean saves reliable on Windows. Register for
it in your service handler:

```rust
// In your windows-service handler:
ServiceControl::Preshutdown => {
    // You get ~20 seconds here by default (vs 5 for shutdown)
    index.flush_to_disk()?;
    ServiceControlHandlerResult::NoError
}
```

`PRESHUTDOWN` fires before `SHUTDOWN` and gives you much more time. Request it
when registering your service controls.

---

**Short answer:** start with a custom mmap binary file - it's the fastest
possible save/load path and matches what Everything does internally. Add `redb`
alongside it only when you need transactional or partial-update semantics.

Good follow-up - each platform has its own shutdown notification mechanism with
different time budgets and reliability guarantees.

---

## Linux

### systemd (primary mechanism)

If your daemon runs as a systemd service, you get the most reliable shutdown
hook. systemd sends `SIGTERM` first, then waits `TimeoutStopSec` before
force-killing with `SIGKILL`.

```ini
# /etc/systemd/system/everything-rs.service
[Service]
ExecStart=/usr/bin/everything-rs daemon
TimeoutStopSec=15        # give yourself 15s to flush
KillMode=mixed           # SIGTERM to main, SIGKILL to rest after timeout
NotifyAccess=main
Type=notify              # daemon calls sd_notify("STOPPING=1") before flush
```

In Rust, handle it like this:

```rust
use tokio::signal::unix::{signal, SignalKind};

let mut sigterm = signal(SignalKind::terminate())?;
let mut sigint  = signal(SignalKind::interrupt())?;

tokio::select! {
    _ = sigterm.recv() => { flush_index().await?; }
    _ = sigint.recv()  => { flush_index().await?; }
}
```

You can also use the `sd-notify` crate to tell systemd you're actively stopping:

```rust
sd_notify::notify(false, &[NotifyState::Stopping])?;
// now flush - systemd won't SIGKILL until TimeoutStopSec elapses
```

**Time budget:** whatever you set in `TimeoutStopSec`. 10–20s is reasonable,
well within your ~200ms flush window.

---

### Non-systemd Linux (SysVinit, runit, OpenRC)

These all ultimately send `SIGTERM` → wait → `SIGKILL`. The time budget varies
by init system (runit gives you 7s by default, OpenRC is configurable), but
`SIGTERM` handling is universal - the same Tokio signal handler above works
everywhere.

---

### `/etc/init.d` shutdown ordering

If you want your service to flush before the filesystem unmounts, add LSB header
hints to ensure correct ordering:

```bash
# Required-Stop: $local_fs $network
# Default-Stop: 0 1 6
```

This ensures the stop script runs before filesystems are torn down.

---

## macOS

### `SIGTERM` - works but with caveats

macOS sends `SIGTERM` to processes during logout/shutdown, so the same Tokio
signal handler works. However the time budget is short and not guaranteed - if
the system decides you're taking too long, it moves on.

```rust
// Same handler as Linux - SIGTERM works on macOS too
let mut sigterm = signal(SignalKind::terminate())?;
sigterm.recv().await;
flush_index().await?;
```

---

### `launchd` (the right way on macOS)

If your daemon is managed by `launchd` (the macOS equivalent of systemd), you
get cleaner lifecycle management. Define a `launchd` plist:

```xml
<!-- ~/Library/LaunchDaemons/rs.everything.plist -->
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" ...>
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>rs.everything</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/local/bin/everything-rs</string>
        <string>daemon</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>ExitTimeOut</key>
    <integer>15</integer>   <!-- seconds before SIGKILL -->
</dict>
</plist>
```

`ExitTimeOut` is the macOS equivalent of `TimeoutStopSec`. 15 seconds is plenty.

---

### `NSWorkspaceWillPowerOffNotification` (GUI apps only)

If you ever build a macOS GUI frontend, you can subscribe to this notification
for an early warning before shutdown. Not relevant for a background daemon
though.

---

## Cross-Platform Signal Handling in Tokio

Since you'll want one clean abstraction across all three platforms, here's a
pattern that compiles correctly on all of them:

```rust
pub async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).unwrap();
        let mut sigint  = signal(SignalKind::interrupt()).unwrap();
        tokio::select! {
            _ = sigterm.recv() => {},
            _ = sigint.recv()  => {},
        }
    }

    #[cfg(windows)]
    {
        // On Windows this is handled by the SERVICE_CONTROL_PRESHUTDOWN
        // handler in the windows-service callback instead.
        // For non-service mode (--foreground), use ctrl_c:
        tokio::signal::ctrl_c().await.unwrap();
    }
}
```

Then in your daemon main loop:

```rust
tokio::select! {
    _ = wait_for_shutdown_signal() => {
        tracing::info!("Shutdown signal received, flushing index...");
        index.flush().await?;
        tracing::info!("Flush complete, exiting.");
    }
    _ = run_daemon(&index) => {}
}
```

---

## Summary

| Platform        | Mechanism                     | Time Budget                  | Reliability      |
| --------------- | ----------------------------- | ---------------------------- | ---------------- |
| Windows         | `SERVICE_CONTROL_PRESHUTDOWN` | ~20s (configurable)          | ✅ Very reliable |
| Linux (systemd) | `SIGTERM` + `TimeoutStopSec`  | Configurable (recommend 15s) | ✅ Very reliable |
| Linux (other)   | `SIGTERM`                     | Varies (5–10s typical)       | ✅ Good          |
| macOS (launchd) | `SIGTERM` + `ExitTimeOut`     | Configurable (recommend 15s) | ✅ Good          |
| macOS (manual)  | `SIGTERM`                     | Short, not guaranteed        | ⚠️ Best effort   |

The dirty-shutdown recovery path (replay USN/inotify delta on next start)
remains your safety net for all platforms - power loss and `SIGKILL` can't be
caught by anyone.
