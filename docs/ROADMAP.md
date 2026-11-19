# Roadmap: advanced features

Thirteen features, in the order they'll be built. Each one ships as its own commit (or a few), with
the AGENTS.md rules: writes go through `Device::write`/`Device::remove` (version check + backup),
slow work in `blocking()`, new commands in all four places, OS differences in `platform.rs`/`os.rs`,
no warnings, plain UI text.

Status: ☐ not started · ◐ in progress · ☑ done

## What's already there

- `find` (files.rs): name search below a folder, with a result cap and time limit.
- `compare` / `sync` (transfer.rs): compares two folders by **size only**; sync copies what's
  missing, never replaces or removes.
- `transfer`: one job at a time, skips files that already exist (so re-running a stopped copy
  already "resumes" it at file granularity), cancel after the current file.
- History panel: a list of strings for this session (`Ctx::history`).
- Books: a Library panel per folder and a reader; EPUB editor.
- media.rs reads JPEG EXIF, but only the orientation.
- Wireless adb devices already work *once connected*: `adb devices -l` lists `ip:port` serials and
  `Adb` takes any serial. What's missing is pairing and connecting from the app.

---

## Phase 1 — Devices

### 1. Wireless adb ◐ (built; not yet tried with a phone)
Goal: connect a phone over Wi-Fi without a cable, and have it come back next time.

Backend (`adb.rs`, `connect.rs`):
- `adb_pair(host_port, code)` → runs `adb pair HOST:PORT CODE` (Android 11+ "Wireless debugging →
  Pair device with pairing code"). Returns adb's message; errors in plain words.
- `adb_connect(host_port)` → `adb connect HOST:PORT`. adb prints "failed to connect…" with
  exit 0, so success is judged by the output (`connected to` / `already connected`).
- `adb_disconnect(host_port)` → `adb disconnect HOST:PORT`, also run when such a device is
  disconnected in the app.
- `adb_discover()` → `adb mdns services`, parsed into (name, kind pair/connect, host:port), so the
  dialog can offer phones that have Wireless debugging open on the same network.
- `Found` gains `wireless: bool` (serial looks like `host:port` or `adb-…._adb-tls-connect._tcp`).

Frontend:
- A "Connect over Wi-Fi…" dialog (devices section of the sidebar): address field, optional pairing
  port + code, list of discovered phones (one click fills the fields), plain steps for the phone.
- Remembered addresses in localStorage (`ferry-files.wifi`); "Reconnect" at launch when auto-connect
  is on (one quiet try each, no error banners).
- A Wi-Fi mark next to wireless devices in the sidebar.

Edge cases: adb missing (reuse the install hint), wrong code, port changed (Android picks a new
connect port each time Wireless debugging is turned on → mDNS discovery fixes it), the same phone
on USB and Wi-Fi (same `ro.serialno`: compare and refuse to connect twice).

Tests: parsing of `adb mdns services` and `adb connect` output (unit tests, no phone).

### 2. Transfer queue ☑
Goal: start more copies while one runs; see, retry and resume them.

Built:
- `Ctx::jobs: RwSignal<Vec<Job>>`, each a `Work` (Copy or Sync) with a state: Queued, Running,
  Paused (interrupted by quitting), Stopped (cancelled or something skipped), Failed. One runs at a
  time; the next starts when it ends. Jobs that finish cleanly leave the list (their report is in
  the pane and in History).
- Devices are kept by serial (`Side::Phone`), so a job finds its phone again after a reconnect,
  or says "… isn't connected".
- The bar below the panes lists the jobs: the running one with progress and Cancel; waiting ones
  with Remove; stopped/failed/interrupted ones with Retry (or Resume) and Remove.
- Retry and Resume run the job again with `resume`: the backend counts a file already at the
  target with the same size as done (`TransferReport.already`) instead of reporting it skipped;
  a different file with the same name is still skipped, never replaced. When moving, originals of
  files found already there are kept (they may be from an earlier copy, not this move).
- Unfinished jobs are kept in localStorage (`ferry-files.jobs`) and come back as "Not finished
  when the app quit" at launch.

### 3. Verify after copy ☑
Goal: know a copy arrived intact.

Built:
- Setting "Check copies" (off by default: it reads every file back, about twice the time over
  MTP). Passed to `transfer` and `sync` as `verify`; the backend takes an `Opts { moving, resume,
  verify }`.
- `run_items`, after each write: reads the copy back and compares it byte for byte with the data
  it just wrote. A mismatch removes the copy (so a retry doesn't count it as done), reports
  "The copy didn't match the original…", and when moving keeps the original.
- adb: `Adb::replace` already compares an md5 of the data with `md5sum` on the phone after every
  write, so adb copies are always checked and aren't read back again.
- Untested: a real mismatch (needs a faulty cable or device); the match path runs in the tests.

## Phase 2 — Files

### 4. Undo ☐
Goal: ⌘Z undoes the last file action.

- `history` becomes structured: `Op { at, text, undo: Option<Undo> }` with
  `Undo::{Rename{src,path,old_name}, Create{src,path}, Move{from_src,paths,to_src,to_dir}, Delete{src,path,backup}}`.
- Undo of: rename → rename back; new folder/file → remove (backed up as usual); move → move back;
  delete → restore from its backup (`restore` command). Copies: remove the copies.
- Each undo checks the thing is still as left (stat), else says what changed and does nothing.
- ⌘Z in panes, "Undo …" in the background menu and an Undo button per History row.

### 5. Search with filters ☐
- `find` gains filters: kind (folder/picture/video/audio/document/book/archive), size range,
  modified range, and name as glob or plain text. Results stream in chunks (event) instead of one
  capped reply.
- Results show as a pane view ("Search: …") where the usual actions work: open, reveal in folder,
  copy, move, delete, drag.

### 6. Batch rename ☐
- Dialog on several picked items: find/replace (plain or regex), add prefix/suffix, numbering
  (`{n:03}`), case, date (modified, or EXIF date taken from phase 3 item 10).
- Live preview table old → new, conflicts and invalid names marked; Apply renames through
  `Device::rename` one by one; one Undo entry for the whole batch.

### 7. Compare two folders ☐
- With two panes open: "Compare" marks each row only-here / differs / same (colour + filter "Show
  differences only"). Uses the existing `compare_dirs`, returned per path instead of counts.
- Actions: copy only-here files to the other side; for differing files, "Replace older" (writes
  with backup).
- Compare by size (fast) or content (hash, slow; per-file progress).

### 8. Duplicate finder ☐
- Pick one or more folders (any devices). Group by size, then by a hash of the first 64 KiB, then
  by a full hash — only for files still tied.
- Results: groups with previews; "Keep one per group" selects the rest (prefers the computer's copy
  or the oldest); Move to Trash goes through `Device::remove` (backed up).

### 9. Sync, more modes ☐
Extends the existing sync:
- **Update**: also replace files that differ (newer by mtime when both sides keep times, else
  by asking), with backups.
- **Mirror**: also remove extras on the target (backed up), after a preview that lists every
  removal; requires typing confirmation when more than N files would be removed.
- Saved sync pairs ("Phone Camera → ~/Pictures/Phone") in the sidebar, one click to preview & run.
- Two-way sync is deliberately left out: without reliable modification times on MTP it can't tell
  which side changed. Revisit with a stored snapshot of the last sync.

## Phase 3 — Media and books

### 10. Photo import ☐
- media.rs: read EXIF `DateTimeOriginal` (JPEG/HEIC header, already partly parsed) and video
  creation time (MP4 `mvhd`), falling back to the file's time.
- "Import photos…" on a device's DCIM: target folder pattern (`YYYY/MM`, `YYYY-MM-DD`), skip
  already-imported (name + size, or a stored import list per phone), optional "remove from phone
  after import" (moves, backed up). Runs as a queue job.

### 11. Library across devices ☐
- One Library view over the computer's book folder(s) and each connected device's books, merged by
  identifier/title; shows where each book is.
- "Send to <device>" / "Copy to computer" per book or selection; missing-on-device filter.

## Phase 4 — Interface

### 12. Command palette ☐
- ⌘K: fuzzy list of actions (every menu action, with its shortcut), favourites, devices, recent
  folders, settings. Enter runs; acts on the active pane.
- Built from the same `Act` items the menus use, so new actions appear automatically.

### 13. Saved workspaces ☐
- Restore tabs, folders, split and widths on launch (setting), for devices by serial so a phone's
  tabs come back when it reconnects.
- Named workspaces in the sidebar: save current / open / delete.

---

## Order and size (rough)

| # | Feature | Backend | Frontend | Size |
|---|---|---|---|---|
| 1 | Wireless adb | pair/connect/mdns | dialog, remembered | M |
| 2 | Transfer queue | small | queue UI | M |
| 3 | Verify | run_items | setting | S |
| 4 | Undo | – | structured history | M |
| 5 | Search filters | find | results view | M |
| 6 | Batch rename | – | dialog | M |
| 7 | Compare marks | compare per path | pane marks | M |
| 8 | Duplicates | hashing | results dialog | L |
| 9 | Sync modes | replace/remove | preview, pairs | L |
| 10 | Photo import | EXIF/MP4 dates | dialog | L |
| 11 | Library | merge | view | M |
| 12 | Command palette | – | palette | S |
| 13 | Workspaces | – | save/restore | S |

Things that need a phone to test (wireless adb, MTP paths of sync/verify/import) will be marked as
untested until run against a real device.
