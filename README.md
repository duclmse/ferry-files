# yomi-files

A Mac desktop app (Tauri 2 + Leptos, all Rust) for browsing, reading and editing the files on Android phones (and other MTP devices) connected by USB, EPUB books in particular. It shows the Mac's own files and disks too, so books can be copied between them.

- One or two panes (◫ in the left pane's toolbar or ⌘\ shows or hides the second; Tab switches between them). Each pane shows the Mac or a connected device (💻, then one 📱 per device, in its toolbar), as a list, as thumbnails (images and EPUB covers), or in columns (buttons at the right of the pane's status bar).
- Right-click a file or folder for open, Quick Look, download, copy (F5) or move (F6) to the other pane, rename (F2), favourites and delete; right-click empty space for new folder, new file, upload, the books in the folder, sync to the other pane, favourites, refresh, the view options and the second pane. Each pane's status bar shows the folder's contents, the picked items and their size, and the free space on that disk or storage.
- Pick several items with ⌘-click (one more) and Shift-click (a range), ⌘A (all) and Escape (none). Copy and move work on folders too, with everything in them: a folder already there is merged into, and a file already there is skipped, never replaced. Progress shows below the panes, with Cancel (it stops after the current file). Moving copies each file, then deletes the original (backed up, like any delete), so a move on the same device takes as long as a copy.
- Drag items between panes, onto a folder, or onto a place in the sidebar. As in Finder, dragging to the same device moves and to another device copies; hold ⌥ to copy or ⌘ to move. Files dragged in from Finder are copied.
- Sort by clicking the list's column headers (name, kind, date, size; folders first). The filter box in the pane's bar narrows the folder by name; Enter searches all its subfolders too, and "Back to folder" ends the search. ⌘F goes to the filter box.
- Space opens Quick Look (phone files are copied to the Mac first).
- Favourites (sidebar): any folder on the Mac or a device, added from the right-click menu. Each pane remembers its last folder for each device and the Mac.
- Sync to the other pane: compares the two folders (files by size) and copies what's missing from the other pane, at any depth. Files that differ, and files only in the other pane, are listed but left alone.
- Disks under Mac in the sidebar have ⏏ to eject them.
- Backups (sidebar): every file the app replaced or deleted, with where it came from. Restore puts it back where it was (when that device is connected); Restore here puts it in the active pane's folder. A file already there is backed up first.
- History (sidebar): what was done since the app started.
- Files on the Mac can be edited too (text and EPUB).
- Double-click (or Enter) opens: folders go in, text files open in the editor, EPUBs in the EPUB editor, anything else in its default Mac app. A phone file is copied to `~/Library/Caches/yomi-files/opened` first, so changes made in the Mac app stay on the Mac. The context menu also has "Open in default app" for text files and EPUBs.
- Settings (sidebar): show hidden files (names starting with a dot; off by default, on the phone and the Mac), turn phone thumbnails off, and see, open or clear the app's folders on the Mac.
- Edit text files.
- Books in this folder (right-click empty space): every EPUB in the folder and its subfolders, with cover, title, authors and language (read once, then remembered in `~/Library/Caches/yomi-files/library` until the file changes). Pick books, then:
  - Check: looks for what makes readers refuse or misshow a book (the zip's `mimetype`, missing title, language or identifier, files listed but missing, broken chapters, links to missing files) and for minor issues (no cover or table of contents, unlisted files). "Check book" in a book's right-click menu does one.
  - Change info: sets authors, language or publisher, or adds subjects, on all of them. Empty fields are left as they are; each book is backed up before it's changed.
  - Make smaller: writes "… (smaller).epub" next to each book, with big pictures scaled down and photos saved again at a lower quality. The book itself isn't changed, and no copy is made if it wouldn't be smaller.
- Read (a book's right-click menu, or the library): a chapter at a time in a plain page, with ← and → for the previous and next chapter and A− / A+ for the text size.
- EPUB editor:
  - book info, cover, and package stats;
  - find & replace across all chapters (text only, with match case and whole word);
  - each chapter in a visual view (with a formatting toolbar) or as XHTML source;
  - save back to the phone, or save a copy on the Mac.

Unedited parts of an EPUB keep their exact bytes.

## Connecting the phone

Two ways to reach the phone, over USB:

- **adb**: turn on USB debugging on the phone and install adb (`brew install --cask android-platform-tools`). Fast; paths look like `/sdcard/Download`.
- **MTP**: the phone's normal "File transfer" USB mode, so no developer options are needed. Needs libmtp (`brew install libmtp`), which is loaded at runtime. Paths start with the storage name, e.g. `/Internal storage/Download`.

The app opens on the Mac's files. Every few seconds it looks for devices on USB and disks mounted on the Mac (USB drives, SD cards, disk images; they're listed under Mac in the sidebar). Phones found show under Devices with an **adb | MTP** button, one half per way to connect (a half is greyed out when that way isn't available; the highlighted one is the suggested way, adb when both work). A phone with both USB debugging and File transfer on is seen both ways; when exactly one of each is found they're shown as one row. Several devices can be connected at once. ⏏ next to a device's name disconnects it, and panes showing it go back to the Mac. MTP devices share one libmtp lock, so a long transfer on one MTP device makes the others wait. Only one program can use MTP at a time, so quit Android File Transfer, Image Capture and other MTP tools first. If MTP still won't connect, run `killall ptpcamerad`.

## Safety

The same rules apply to files on the phone and on the Mac. Every write or delete:

1. refuses if the file changed on the phone since it was opened,
2. backs up the old version to `~/Library/Caches/yomi-files/backups` (see Backups in the sidebar; `index.tsv` there notes where each backup came from),
3. writes the new version and checks it.

Thumbnails need the whole file read (slow from the phone over MTP), so phone thumbnails are made one at a time and only for folders on screen. Files over 60 MB get none. They are cached in `~/Library/Caches/yomi-files/thumbs`; Settings shows its size and can clear it (backups are never cleared by the app). Phone thumbnails can be turned off in Settings.

## Build and run

Requirements:

- Rust with the `wasm32-unknown-unknown` target
- `trunk`
- the Tauri CLI (`cargo install tauri-cli`)
- adb and/or libmtp, as above

```sh
cargo tauri dev                       # run in development
cargo tauri build --debug             # build target/debug/bundle/macos/yomi-files.app
cd src-tauri && cargo test --lib      # unit tests (XML tokenizer, EPUB editing, copy, sync, search, book tools)
```

Tests against a connected phone use scratch files only, and they delete those files and their backups afterwards:

```sh
cd src-tauri
cargo run --example smoke         [-- adb|mtp]   # list, read, write, conflict check, delete
cargo run --example epub_smoke    [-- adb|mtp]   # edits a copy of an .epub found on the phone
```

## Layout

- `src/`: the frontend (Leptos 0.8, client-side, built by Trunk into `dist/`).
  - `app.rs`: sidebar (devices, places, favourites), panes and file views, context menu, transfers, text editor, and the Backups, History, Library and Reader windows.
  - `epub.rs`: EPUB editor screens.
  - `api.rs`: calls to the backend, and the types shared with it.
- `src-tauri/`: the backend.
  - `device.rs`, `adb.rs`, `mtp.rs`, `local.rs`: transports (phone over adb or MTP, and the Mac's own files), version checks, backups.
  - `xml.rs`, `epub.rs`: EPUB parsing, editing and checks.
  - `books.rs`: library info, checks, info changes and shrinking for many books.
  - `protocol.rs`: the `yomi://` scheme that serves book images, styles and fonts, and thumbnails.
  - `thumbs.rs`: thumbnails and their cache.
  - `lib.rs`: the commands the frontend calls, including copying, moving, syncing, searching and backups.

## EPUB editor limits

- Find doesn't match a phrase that formatting splits in two (half of it bold, for example).
- In a text that gets replaced, special characters written as named codes the editor doesn't know (such as `&eacute;`) are written back in a different form.
- Replacing the cover with a different image type keeps the old file name.
