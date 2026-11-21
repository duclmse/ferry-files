# Ferry Files

A desktop file browser for macOS, Windows and Linux (Tauri 2 + Leptos, all Rust)
for Android phones and other MTP devices over USB, servers, and the computer's
own files and disks, with tools for EPUB books in particular.

## Features

- **Panes and views**: one or two panes (⌘\\), each with tabs (⌘T, ⌘W), shown as
  a list, thumbnails or columns. Sort, filter (⌘F; Enter searches subfolders),
  type a path (⌘L). All edges can be dragged to resize, and sizes are remembered.
- **Preview** beside the files:
  - pictures that zoom and turn;
  - video and audio streamed from the device, with their own controls, full
    screen and a pop-out window;
  - text and code in colour (about 50 languages), read as you scroll;
  - hex, read by range, with go-to-offset;
  - PDFs, with an outline, go to page, zoom, turn, Open and Show in Finder;
  - fonts as samples;
  - EPUBs as their cover, or read a chapter at a time. Only the chapter shown
    (and its pictures) is read from the book. Edit opens the whole book in the
    EPUB editor, there in the preview;
  - ZIP, APK and JAR contents as a tree, read from the file's end without
    unpacking. A file picked in it shows in the preview: read in place when
    it's stored without compression (as videos usually are), otherwise
    unpacked, up to 64 MB.
- **Files**: copy, move, rename, delete, new file or folder, upload and
  download. Use the right-click menu, F5/F6, ⌘C/⌘X/⌘V (shared with Finder) or
  drag and drop. Copies merge folders and never replace a file, with progress
  and Cancel.
- **Sync** a folder to the other pane, with three modes: copy missing, update or
  mirror. It lists what it will do first, and pairs can be saved.
- **Sidebar**: favourites, workspaces, disks (with eject), devices, servers,
  sync pairs, backups and history. Folders show as trees, and sections can be
  moved and hidden.
- **Connections**: SFTP, SMB, and Everything (voidtools) over HTTP. Passwords
  are kept in the system keychain.
- **Terminal** in a pane, on this computer, an adb phone or an SFTP server.
- **Editor view**: a folder tree beside text files in tabs.
- **Books**: list every EPUB under a folder. Check them, change their info, or
  make smaller copies. Read them a chapter at a time, or edit them in the
  preview's EPUB editor: info, cover, find and replace, a visual or XHTML view
  (in colour), and saving back to the phone. Unedited parts of an EPUB keep their exact bytes.

Press ? for the keyboard shortcuts.

## Connecting a phone

- **adb**: turn on USB debugging and install adb
  (`brew install --cask android-platform-tools`). It's fast. Paths look like
  `/sdcard/Download`.
- **MTP**: the phone's normal "File transfer" mode. Paths start with the storage
  name, e.g. `/Internal storage/Download`. macOS and Linux use libmtp, which the
  macOS app bundles (otherwise `brew install libmtp`). Windows uses its own MTP
  support.

Phones are found and connected when plugged in; Settings can turn that off. A
phone available both ways shows once, and its badge (⇄) switches between them.
Only one program can use MTP at a time, so quit Android File Transfer, OpenMTP
and similar tools first. If MTP still won't connect, run `scripts/free-mtp.sh`
to see and stop what holds the phone (`--dry-run`, `--watch`, `--force`).

## Safety

Every write or delete, on a phone or the computer:

1. refuses if the file changed since it was opened,
2. backs up the old version (see Backups in the sidebar),
3. writes the new version and checks it.

Previews read only what they show: media and hex by range, and phone thumbnails
one at a time, only for folders on screen.

## Platforms

The text above is written for macOS. On Windows, Ctrl replaces ⌘ and drives
show under `/` (`/C:/Users/...`). On Linux, disks are mounted under `/media`,
`/run/media` or `/mnt`, and MTP needs libmtp's udev rules. CI builds Windows and
Linux, but they've seen little use on real hardware.

## Build and run

Requirements: Rust with the `wasm32-unknown-unknown` target, `trunk`, the Tauri
CLI (`cargo install tauri-cli`), and adb and/or libmtp. Linux also needs
Tauri's system packages (`libwebkit2gtk-4.1-dev libappindicator3-dev
librsvg2-dev patchelf libmtp-dev`).

```sh
cargo tauri dev                    # run in development
cargo tauri build --debug          # build the app bundle
cargo test                         # frontend tests
cd src-tauri && cargo test --lib   # backend tests
```

On macOS the build runs `scripts/bundle-libmtp.sh`, which bundles Homebrew's
libmtp and libusb (LGPL-2.1), so the build machine needs `brew install libmtp`.

Tests against a real phone use scratch files only and clean up after
themselves:

```sh
cd src-tauri
cargo run --example smoke      [-- adb|mtp]
cargo run --example epub_smoke [-- adb|mtp]
```

## Layout

- `src/`: the frontend (Leptos 0.8, built by Trunk). `app.rs` holds the shared
  state, and `app/` has one file per part. `api.rs` has the backend calls and
  the types shared with it.
- `styles/`: the CSS, one file per part. `base.css` has the colour tokens.
- `src-tauri/`: the backend.
  - Transports: `device.rs`, `adb.rs`, `mtp.rs` (`wpd.rs` on Windows),
    `local.rs`, and the servers (`sftp.rs`, `smb.rs`, `everything.rs`).
  - `platform.rs`: what differs between operating systems.
  - `protocol.rs`: the `ferry://` scheme that serves files to the web view.
  - EPUB: `epub/`, `epub.rs`, `xml.rs`, `books.rs`.
  - Archives and PDFs: `archive.rs`, `pdf.rs`.
  - `lib.rs`: the app state and the command list.

## EPUB editor limits

- Find doesn't match a phrase that formatting splits in two (half of it bold,
  for example).
- Named character codes the editor doesn't know (such as `&eacute;`) are written
  back in a different form when their text is replaced.
- Replacing the cover with a different image type keeps the old file name.
