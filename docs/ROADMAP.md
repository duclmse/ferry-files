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

### 4. Undo ☑
Goal: ⌘Z undoes the last file action.

Built (`src/app/undo.rs`):
- `history` is a list of `Op { at, text, undo: Option<Undo>, undone }`.
- `Undo::Renames` (rename back; a list, ready for batch rename), `Create` (remove the new item;
  a file only while still empty), `Delete` (folders made again, files restored from their
  backups), `Copy` (removes what the copy made, with the new `remove_tree` command: every file
  backed up), `Move` (moves back through the transfer queue).
- The backend reports `TransferReport.made`: top-level items the transfer created that weren't
  there before. A copy's undo removes only those; a move can be undone only when every item went
  into a new place and nothing was skipped (otherwise moving back would take files that were
  there before).
- Each undo first checks things are as the action left them (the renamed/created/copied items
  are there, a deleted name is still free), else says what changed and does nothing.
- ⌘Z in the panes (not in text fields), "Undo: …" at the top of the background menu, an Undo
  button per row in History (undone rows are crossed out).

### 5. Search with filters ☑
Built:
- `find` takes `filters` (kind, size range, modified range) and treats a query with `*` or `?`
  as a pattern on the whole name (plain text still matches anywhere). An empty query with
  filters finds everything they let through. Limits raised to 1000 results / 30 seconds.
- Kinds by extension (`files::kind_of`): folder, picture, video, audio, document, book,
  archive. Size and date filters skip folders.
- A filter button beside the pane's Filter field opens a row with Kind, Size and Modified, Search
  and Clear. The status bar names the search with its filters.
- Results show in the pane as before, where the usual actions work.
- Not done: streaming results in chunks (still one reply; the limits keep it bounded).

### 6. Batch rename ☑
- F2 or "Rename N items…" with several picked opens a dialog: Find / Replace with (plain, or a
  JavaScript regular expression with $1…; match case optional), a New name pattern with {name},
  {n}, {n:3} and {date} (date modified), a Start number, and case (lower, UPPER, Title).
  The extension stays.
- Live preview table: now → new, with "No change", "Same as another new name", "Already there"
  and bad names marked; Rename is off until nothing is marked.
- Renames go one by one in an order where no item takes a name another still has (names swapped
  in a ring go by way of a temporary name). One Undo entry for the batch; undo uses the same
  ordering.
- Not done: EXIF date taken (comes with phase 3 item 10).

### 7. Compare two folders ☑
- "Compare with the other pane" (folder menu) marks each item in both panes: only here, newer,
  older, different or the same, as a coloured stripe (the same ones fade). New command
  `compare_marks`; folders on both sides are compared by what's in them (by size).
- A bar above the files: counts, "Differences only", "By content" (same-size files byte by byte,
  read in 4 MiB parts), "Copy N to <other>" for what's only here, "Replace N older in <other>"
  (a copy with mode "replace": the files there are backed up first), Again and Done.
- Marks clear when either pane opens another folder, searches, or the second pane closes; the
  same folder reloading (after a copy) compares again.
- Not done: per-file progress while comparing by content.

### 8. Duplicate finder ☑
- "Find duplicates…" (folder menu) opens a window with the pane folders (both, when split) to
  look in, and a smallest size. New command `duplicates`: groups by size, then md5 of the first
  64 KiB, then md5 of the whole file, only for files still tied. Progress comes as the "dupes"
  event; Stop cancels (through `cancel_transfer`). Up to 200,000 files.
- Results: groups biggest first, each copy with a thumbnail, date, device and path, and Show (opens
  its folder in the active pane). "Keep one per group" picks all but one (the computer's copy, else
  the oldest). Delete removes the picked files through `remove` (backed up), refusing to take every
  copy in a group; one Undo entry per device.

### 9. Sync, more modes ☑
- The Sync preview has a mode: **Copy missing** (as before), **Update** (also replaces files that
  differ in size, each backed up through `Device::write`), **Mirror** (Update, and also removes
  what's only in the target, files backed up). `sync` takes `mode` and `remove`; Mirror removes
  only the listed items that are still extras, so nothing goes that the preview didn't show.
- Mirror lists every item it will remove; more than 10 needs "remove" typed to run.
- "Save" keeps the pair (folders on either side, and the mode) under Sync pairs in the sidebar;
  clicking one previews it in the active pane. Phones are found again by serial.
- Not done: Update compares by size only (times aren't kept across devices), so a same-size
  edit isn't replaced. Two-way sync is left out: without reliable modification times on MTP it
  can't tell which side changed. Revisit with a stored snapshot of the last sync.

## Phase 3 — Media and books

### 10. Photo import ☑
- media.rs `taken`: EXIF `DateTimeOriginal` (else `DateTime`) from a JPEG's APP1 or a HEIC's
  Exif item (found through `iinf`/`iloc`), and an MP4/MOV's `mvhd` creation time (only the start
  of `moov` is read). Falls back to the file's date; with none, "Undated".
- "Import photos…" (folder menu) opens a window: into the other pane's folder or Pictures on the
  computer, folders as `2024/05`, `2024-05-14`, `2024/2024-05-14` or all in one, and "remove them
  from the phone once imported" (a move: originals backed up). New command `import_photos` runs as
  a queue job (`Work::Import`) with progress, cancel, verify and retry.
- Imported before = a file of the same name and size already in its dated folder (the same check
  as resuming a copy); those originals are kept even when moving. No stored per-phone list.

### 11. Library across devices ☑
- Each place (the computer, each device by serial) has a book folder: the folder Books was last
  opened on there (`ferry-files.book-dirs`).
- "Books on all devices…" (folder menu, and "All devices…" in the Books window) lists the books
  of every connected place's book folder, merged by title and first author (else the file name),
  each with where it is (✓ place) and "Copy to <place>" for places without it.
- Filter by title/author/file, "Missing on <place>", and pick several to "Copy picked to <place>":
  copies go through the transfer queue, from the computer when it has the book. Places not
  connected are named.
- Frontend only (`library` + `book_info`). Not done: the EPUB identifier isn't read, so two
  editions with the same title and author count as one book.

## Phase 4 — Interface

### 12. Command palette ☑
- ⌘K (also from the filter) opens it; ⌘K or Esc closes. Typed letters match in order (runs and
  word starts score higher); ↑ ↓ choose, ↵ runs on the active pane.
- Choices: the menu for what's picked (File / Folder / Picked), the folder's menu (Here), app
  commands (Settings, Keyboard shortcuts, Backups, History, Refresh devices, Connect over Wi-Fi,
  Go up), favourites, devices with their quick folders, recent folders and sync pairs.
- Built from `menu_items`, so new menu actions show up by themselves; only enabled items appear,
  and ✓ items read as "View as …" / "Show/Hide the … column".
- Recent folders: the last 20 opened, kept in `ferry-files.recent` (devices by name, shown only
  while connected). ⌘K no longer focuses the filter (⌘F and / still do).

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
