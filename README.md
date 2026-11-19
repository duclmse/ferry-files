# Ferry Files

A desktop app for macOS, Windows and Linux (Tauri 2 + Leptos, all Rust) for
browsing, reading and editing the files on Android phones (and other MTP
devices) connected by USB, EPUB books in particular. It shows the Mac's own
files and disks too, so books can be copied between them.

- One or two panes (◫ in the left pane's toolbar or ⌘\ shows or hides the
  second; Tab switches between them). Each pane shows the Mac or a connected
  device (picked in the sidebar), as a list, as thumbnails (images and EPUB
  covers), or in columns (buttons at the right of the pane's status bar).
- The preview, to the right of the files in every view (at its narrowest until
  you drag the edge between them) (the last button in the status bar shows or
  hides it), shows the selected item: the picture itself, sized to the preview
  (its thumbnail shows while the file is read), details (width × height of
  images and videos, read from the file's header, and the length of videos and
  audio), videos and audio that play straight from the device without copying
  them first (GIF, HEIC, AVIF and SVG images too), and the text of text files,
  read a piece at a time as you scroll (up to 8 MB). Pictures zoom (− and +,
  pinch, or double-click; the % is of the picture's own pixels, and clicking it
  or double-clicking switches between fitted and 100%), drag around when zoomed,
  and turn (⟲ ⟳). Videos have their own controls: click to play or pause, hold
  for 2× (or the speed set in Settings > Video), double-click the left side to go
  back 5 s and the right side to go 10 s ahead, hover the seek bar to see that
  moment, turn them (the controls stay level, or turn too, as set in Settings),
  play them full screen, or pop them out into a small window that stays on top.
  K plays or pauses the preview's video or sound, J goes back 5 s, L goes ahead
  10 s (held: faster), M turns the sound off or on; a clicked player also takes
  Space, ← → and F. Videos and sounds go on from where they were stopped. ‹ and ›
  (or ← and → in the list) go to the previous and next picture, video or sound in
  the folder. PDFs (up to 64
  MB) show in the preview, fonts (TTF, OTF, WOFF, WOFF2) as samples, EPUBs as
  their cover (the cover itself once the preview is bigger than its thumbnail),
  and ZIP, APK and JAR files as the list of what's in them (read from the end of
  the file, without unpacking; the first 5000 items); in the column view, a ZIP
  picked in the last column also opens as more columns, one per folder in it; a
  file picked there shows in the preview (unpacked in memory, up to 64 MB).
  Right-click a file for "Show as text" or "Show as hex" (bytes, 16 to a line,
  up to 4 MB as you scroll).
- Drag the edges to resize the sidebar, the two panes, the files beside the
  preview, the column view's columns and the list's columns; double-click an
  edge for its default width. The sizes are remembered.
- Right-click a file or folder for open, Quick Look, download, copy (F5) or move
  (F6) to the other pane, rename (F2, in place: Enter keeps it, Escape cancels),
  favourites, copy name, copy path and delete; right-click empty space for new
  folder, new file, upload, the books in the folder, sync to the other pane,
  favourites, refresh, the view options and the second pane. Each pane's status
  bar shows the folder's contents, the picked items and their size, and the free
  space on that disk or storage.
- Pick several items with ⌘-click (one more) and Shift-click (a range), ⌘A (all)
  and Escape (none). Copy and move work on folders too, with everything in them:
  a folder already there is merged into, and a file already there is skipped,
  never replaced. Progress shows below the panes, with Cancel (it stops after
  the current file). Moving copies each file, then deletes the original (backed
  up, like any delete), so a move on the same device takes as long as a copy.
- Copy (⌘C, or Copy in the right-click menu) keeps the picked items; Paste (⌘V,
  or Paste on empty space or on a folder) copies them into the folder, on any
  device, as copying to the other pane does. Cut (⌘X) then Paste moves them
  instead. Files copied on the Mac go on the system clipboard too, so Finder can
  paste them, and files copied in Finder paste into any folder here, phones
  included. ⌘C still copies text selected in a preview.
- Drag items between panes, onto a folder, or onto a place in the sidebar. As in
  Finder, dragging to the same device moves and to another device copies; hold ⌥
  to copy or ⌘ to move. Files dragged in from Finder are copied.
- Sort by clicking the list's column headers (name, kind, date, size; folders
  first). Drag a column's heading to move it; right-click the headings (or empty
  space in the list, or the list button in the status bar) to show or hide Kind,
  Modified and Size. Text too long for its column is cut off with … The filter
  box in the pane's bar narrows the folder by name; Enter searches all its
  subfolders too, and "Back to folder" ends the search. ⌘F, ⌘K or / goes to the
  filter box, which offers the last 10 searches.
- Click the empty part of the path above the files (or press ⌘L) to type a path;
  Enter goes there, Escape cancels. A path too long for its room folds its first
  folders into …, which lists them.
- Tabs: each pane can have several. ⌘T (or "New tab" on empty space) opens one
  on the same folder, and "Open in new tab" on a folder opens it in one. The
  tabs show above the pane's bar once there are two; click one to go to it, × or
  a middle click closes it, and + opens another. ⌘W closes the tab (the window,
  when it's the last one), ⌘⇧] and ⌘⇧[ (or Ctrl+Tab and Ctrl+Shift+Tab) go to
  the next and previous one. Each tab keeps its folder, selection and view; tabs
  aren't kept after quitting.
- ? lists the keyboard shortcuts (also in Settings). Delete or ⌘⌫ deletes.
- Settings: appearance (as the Mac, light or dark).
- Space opens Quick Look (phone files are copied to the Mac first).
- Favourites (sidebar): any folder on the Mac or a device, added from the
  right-click menu. Each pane remembers its last folder for each device and the
  Mac.
- Sync to the other pane: compares the two folders (files by size) and copies
  what's missing from the other pane, at any depth. Files that differ, and files
  only in the other pane, are listed but left alone.
- Disks under Mac in the sidebar have ⏏ to eject them.
- The sidebar's places, disks, device folders and favourites have ▸ to show the
  folders in them, and theirs, as a tree.
- Backups (sidebar): every file the app replaced or deleted, with where it came
  from. Restore puts it back where it was (when that device is connected);
  Restore here puts it in the active pane's folder. A file already there is
  backed up first. Preview shows a backup in Quick Look; Clear… (after asking)
  moves the whole backups folder to the Trash.
- History (sidebar): what was done since the app started.
- Files on the Mac can be edited too (text and EPUB).
- Double-click (or Enter) opens: folders go in, text files open in the editor,
  EPUBs in the EPUB editor, anything else in its default Mac app. A phone file
  is copied to `~/Library/Caches/ferry-files/opened` first, so changes made in
  the Mac app stay on the Mac. The context menu also has "Open in default app"
  for text files and EPUBs.
- Settings (sidebar): show hidden files (names starting with a dot; off by
  default, on the phone and the Mac), turn phone thumbnails off, and see, open
  or clear the app's folders on the Mac.
- Edit text files.
- Books in this folder (right-click empty space): every EPUB in the folder and
  its subfolders, with cover, title, authors and language (read once, then
  remembered in `~/Library/Caches/ferry-files/library` until the file changes).
  Pick books, then:
  - Check: looks for what makes readers refuse or misshow a book (the zip's
    `mimetype`, missing title, language or identifier, files listed but missing,
    broken chapters, links to missing files) and for minor issues (no cover or
    table of contents, unlisted files). "Check book" in a book's right-click
    menu does one.
  - Change info: sets authors, language or publisher, or adds subjects, on all
    of them. Empty fields are left as they are; each book is backed up before
    it's changed.
  - Make smaller: writes "… (smaller).epub" next to each book, with big pictures
    scaled down and photos saved again at a lower quality. The book itself isn't
    changed, and no copy is made if it wouldn't be smaller.
- Read (a book's right-click menu, or the library): a chapter at a time in a
  plain page, with ← and → for the previous and next chapter and A− / A+ for the
  text size.
- EPUB editor:
  - book info, cover, and package stats;
  - find & replace across all chapters (text only, with match case and whole
    word);
  - each chapter in a visual view (with a formatting toolbar) or as XHTML
    source;
  - save back to the phone, or save a copy on the Mac.

Unedited parts of an EPUB keep their exact bytes.

## Connecting the phone

Two ways to reach the phone, over USB:

- **adb**: turn on USB debugging on the phone and install adb
  (`brew install --cask android-platform-tools`). Fast; paths look like
  `/sdcard/Download`.
- **MTP**: the phone's normal "File transfer" USB mode, so no developer options
  are needed. Paths start with the storage name, e.g.
  `/Internal storage/Download`. On macOS and Linux this uses libmtp, loaded at
  runtime (so the app still starts without it): the macOS app bundle carries its
  own copy, and the Linux packages depend on the system's (`libmtp9`). Without a
  bundled copy, install it with `brew install libmtp` or your package manager.
  On Windows it uses Windows' own MTP support (Windows Portable Devices), so
  nothing needs installing.

## Platforms

The text above is written for macOS. Elsewhere:

- **Windows**: the computer's files start at "/", which lists the drives
  (`/C:/Users/...`); drives other than the system drive are listed as disks and
  can be ejected. Ctrl takes the place of ⌘ (Ctrl-click picks several; dragging
  with Ctrl copies and with Shift moves). Space opens a file in its default app.
  The window looks for devices every few seconds instead of being told about
  USB. adb is looked for in the Android SDK (`%LOCALAPPDATA%\Android\Sdk`),
  scoop and `PATH`.
- **Linux**: disks are the ones mounted under `/media`, `/run/media` and `/mnt`;
  eject uses `udisksctl`. USB changes are noticed by looking at `/sys/bus/usb`
  every second. If the desktop's file manager (gvfs) has the phone open over
  MTP, unmount it there first. Using a phone over USB needs libmtp's udev rules
  (installed with the `libmtp` package on most distributions).
- Backups are kept in the app's data folder (`%LOCALAPPDATA%\ferry-files` or
  `~/.local/share/ferry-files`) rather than `~/Library/Caches`.

Windows and Linux builds are compiled by CI; they haven't had much use on real
hardware yet.

The app opens on the Mac's files. It looks for devices on USB and disks mounted
on the Mac (USB drives, SD cards, disk images; they're listed under Mac in the
sidebar) when it starts and whenever macOS reports a USB device plugged in or
taken out, or a disk mounted or ejected; ↻ next to Devices looks again. (While a
phone waits for USB debugging to be allowed, it also looks every 3 seconds; if
macOS can't report USB changes, it looks every 3 seconds while no phone is
connected.) A phone is connected as soon as it's found: with adb when USB
debugging is allowed, otherwise with MTP (File transfer). One plugged in while
the app is open is shown in the active pane; one already plugged in when the app
starts connects without taking over the window. Settings → "Connect phones when
they're plugged in" turns this off. Each phone is connected automatically once:
after a failure or a disconnect it waits until it's unplugged and plugged in
again. Opening an MTP phone is tried 4 times, 1.5 s apart, since macOS holds it
for a moment after it's plugged in; if it still fails, the error has Try again.
Before each try the app checks in `ioreg` who has the phone: Image Capture's
helpers (`ptpcamerad`, which takes the phone's MTP interface to import photos,
`icdd` and `mscamerad-xpc`) are stopped, as they come back by themselves; any
other program is named in the error and the phone isn't tried, since libmtp
keeps hold of a phone it failed to open until the app quits. If a try fails
anyway and leaves the app holding the phone, the error says to unplug it and
plug it in again.

adb and MTP aren't exclusive: a phone with both USB debugging and File transfer
on is seen both ways. The app tells it's the same phone by its USB serial number
(adb's serial, and the one macOS reports for the MTP device in `ioreg`), so it's
shown once and connected once. When a connected phone is also available the
other way, the **adb** or **MTP** badge next to its name shows ⇄, and clicking
it reconnects the other way. Phones not connected show under Devices with an
**adb | MTP** button, one half per way to connect (a half is greyed out when
that way isn't available; the highlighted one is the suggested way). If a phone
is waiting for USB debugging to be allowed, the sidebar says so. Several devices
can be connected at once. ⏏ next to a device's name disconnects it, and panes
showing it go back to the Mac; its MTP copies are stopped, and disconnecting (or
switching to the other way) waits up to 5 s until the phone is let go of, so it
can be connected again straight away. A connected phone that's unplugged is
disconnected once it's been missing from USB at two looks in a row (adb loses a
phone for a moment now and then), and the sidebar says so. Quitting disconnects
every device too, as does Ctrl+C (or SIGTERM or SIGHUP) to the app's process,
e.g. in the terminal running `cargo tauri dev`: transfers are cancelled and MTP
copies stopped, so the phone's MTP session is closed before the app ends. MTP devices share one libmtp lock, so a long transfer on
one MTP device makes the others wait. Only one program can use MTP at a time, so
quit Android File Transfer, OpenMTP and other MTP tools first.

If MTP still won't connect ("libusb_claim_interface() = -3"), run
`scripts/free-mtp.sh`: it shows which program has the phone (or one of its
interfaces) and stops the ones known to grab phones, killing Image Capture's
helpers, which ignore a polite stop. `--watch` keeps stopping those helpers
until another program has the phone, for connecting with an app that doesn't
stop them itself; `--force` stops any program holding it, Ferry Files included;
`--dry-run` only shows what it would do.

## Safety

The same rules apply to files on the phone and on the Mac. Every write or
delete:

1. refuses if the file changed on the phone since it was opened,
2. backs up the old version to `~/Library/Caches/ferry-files/backups` (see
   Backups in the sidebar; `index.tsv` there notes where each backup came from),
3. writes the new version and checks it.

Thumbnails need the whole file read (slow from the phone over MTP), so phone
thumbnails are made one at a time and only for folders on screen. Files over 60
MB get none. They are cached in `~/Library/Caches/ferry-files/thumbs`; Settings
shows its size and can clear it (backups are cleared from Backups, to the
Trash). Phone thumbnails can be turned off in Settings.

The preview's videos and audio are streamed: the player asks for parts of the
file (HTTP Range on `ferry://localhost/file`) and only those parts are read.
Over adb each part is read with `dd`. Over MTP it uses libmtp's
`LIBMTP_GetPartialObject`; when the phone or libmtp doesn't support it, the file
is copied once to the app's temporary folder (only the latest one is kept) and
read from there. The web view doesn't cache these parts, and a player lets go of
its file when the preview moves on.

## Build and run

Requirements:

- Rust with the `wasm32-unknown-unknown` target
- `trunk`
- the Tauri CLI (`cargo install tauri-cli`)
- adb and/or libmtp, as above
- on Linux, Tauri's system packages:
  `libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev patchelf`, and
  `libmtp-dev` for running with MTP

```sh
cargo tauri dev                       # run in development
cargo tauri build --debug             # build target/debug/bundle/macos/Ferry Files.app (or .deb/.rpm/AppImage, .msi/.exe)
cd src-tauri && cargo test --lib      # unit tests (XML tokenizer, EPUB editing, copy, sync, search, book tools)
```

On macOS, `cargo tauri build` first runs `scripts/bundle-libmtp.sh`, which
copies Homebrew's libmtp and libusb into `src-tauri/libs/` (pointing one at the
other) for the bundle's `Contents/Frameworks`, with their licenses (LGPL-2.1) in
`Contents/Resources/licenses`. So it needs `brew install libmtp` on the build
machine. Homebrew builds for the macOS it runs on; for an app that works on
older macOS versions, build libmtp with `MACOSX_DEPLOYMENT_TARGET=11.0` instead.
On older Macs the app falls back to a Homebrew libmtp if there is one.

Tests against a connected phone use scratch files only, and they delete those
files and their backups afterwards:

```sh
cd src-tauri
cargo run --example smoke         [-- adb|mtp]   # list, read, write, conflict check, delete
cargo run --example epub_smoke    [-- adb|mtp]   # edits a copy of an .epub found on the phone
```

## Layout

- `src/`: the frontend (Leptos 0.8, client-side, built by Trunk into `dist/`).
  - `app.rs`: the shared state (`Ctx`, panes) and the window; `app/` has the
    rest, one file per part: `sidebar`, `devices`, `pane`, `views` (list,
    thumbnails, columns), `menu`, `nav`, `files`, `transfer`, `editor` (text),
    `safety` (backups, history), `books` (library, reader), `util`.
  - `epub.rs`: EPUB editor screens.
  - `api.rs`: calls to the backend, and the types shared with it.
- `styles/`: the CSS, one file per part, loaded in order by `index.html`;
  `base.css` has the colour tokens.
- `src-tauri/`: the backend.
  - `device.rs`, `adb.rs`, `mtp.rs` (libmtp; `wpd.rs` takes its place on
    Windows), `local.rs`: transports (phone over adb or MTP, and the computer's
    own files), version checks, backups.
  - `platform.rs`: what differs between macOS, Windows and Linux (paths, disks,
    eject, opening files, free space); `src/os.rs` is the frontend's side.
  - `xml.rs`, `epub.rs` (and `epub/`: info, chapter text, find and replace,
    checks): EPUB parsing, editing and checks.
  - `books.rs`: library info, checks, info changes and shrinking for many books.
  - `protocol.rs`: the `ferry://` scheme that serves book images, styles and
    fonts, thumbnails, and files in parts for the preview's media.
  - `thumbs.rs`: thumbnails and their cache.
  - `lib.rs`: the app's state and the command list. The commands are grouped in
    `connect.rs`, `files.rs`, `transfer.rs`, `safety.rs` (space, backups,
    eject), `settings.rs`, `library.rs` (books, many at a time) and `editor.rs`
    (the EPUB editor).

## EPUB editor limits

- Find doesn't match a phrase that formatting splits in two (half of it bold,
  for example).
- In a text that gets replaced, special characters written as named codes the
  editor doesn't know (such as `&eacute;`) are written back in a different form.
- Replacing the cover with a different image type keeps the old file name.
