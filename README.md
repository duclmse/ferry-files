# Ferry Files

A desktop file browser for macOS, Windows and Linux (Tauri 2 + Leptos, all Rust)
for Android phones, iPhones and other MTP devices over USB, servers, and the computer's
own files and disks, with tools for EPUB books in particular.

## Features

- **Panes and views**: one or two panes (⌘\\), each with tabs (⌘T, ⌘W; drag one along, or to the other pane, which a pane's last tab closes), shown as
  a list, thumbnails or columns (names too long to show keep their extension in view). The list can
  show folders as a tree (right-click its header), and group items under headings (right-click
  the list: Group by name, kind, date or size). Sort (also from the right-click menu), filter (⌘F; Enter searches subfolders), show hidden files (⌘.),
  type a path (⌘L). All edges can be dragged to resize, and sizes are remembered.
- **Preview** beside the files:
  - pictures that zoom, scroll and turn (the next one is read ahead, and ‹ › keeps the picture shown until the one it goes to is ready), and show full screen (F). Right-click
    the zoom level to fit the whole picture, its width, or show it at actual
    size; the choice is kept for the next pictures;
  - video and audio streamed from the device, with their own controls, full
    screen and a pop-out window. Subtitles (.srt, .sub, .vtt) named like the
    video in its folder are shown, and more can be dropped on the video, from
    a pane or Finder. Two can show at once (the second at the top), and
    Settings picks the languages shown first. C turns them on and off. In full
    screen the controls hide while the mouse is still;
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
  drag and drop (onto a pane, or a folder or device in the sidebar, which opens
  when held over). Copies merge folders and never replace a file, with progress
  and Cancel. A file over 4 GB isn't copied to a FAT32 disk (many USB sticks and
  SD cards), which can't hold it; the copy says so and goes on with the rest.
- **Scripts**: Lua 5.4 scripts run on the files picked, or the folder (right-click, Run
  script). They're in the app's scripts folder and the files and folders added in
  Settings > Scripts, where they can be made and edited. A script can list, read, make,
  write, copy, move, rename and delete files and folders, only through the app, so what it
  replaces or deletes is backed up; it has no `io` or `os`. Stop ends one that runs too
  long. [docs/scripting.md](docs/scripting.md) is the guide (Settings > Scripts opens it too).
- **Sync** a folder to the other pane, with three modes: copy missing, update or
  mirror. It lists what it will do first, and pairs can be saved.
- **Sidebar** (right-click a folder to open it, paste or make something in it, copy its path, rename or delete it, eject a disk; drag a folder to a pane or another folder to copy or move it): favourites, workspaces, disks (with eject), devices, servers,
  sync pairs, backups and history. Folders show as trees, and sections can be
  moved and hidden. "Show the current folder" (right-click the sidebar, or
  Settings > Sidebar) opens the trees down to the folder shown and scrolls to it.
- **Connections**: SFTP, SMB, NFS, WebDAV (Nextcloud, NAS boxes), FTP and FTPS,
  S3 buckets (AWS, Cloudflare R2, Backblaze B2, MinIO), and Everything
  (voidtools) over HTTP. Passwords and keys are kept in the system keychain.
  Shares the system has mounted (SMB, NFS, WebDAV, FTP) show under Servers,
  to unmount, not with the disks.
- **Fast search of the computer**: when fsd (the search daemon of the fs
  project) is running and indexes the folder, a search under it (Enter in the Filter field) comes from fsd's index
  instead of walking every folder. Otherwise folders are walked as before.
- **Terminal** in a pane, on this computer, an adb phone or an SFTP server.
- **Editor view**: a folder tree beside text files in tabs.
- **Books**: list every EPUB under a folder. Check them, change their info, or
  make smaller copies. Read them a chapter at a time, or edit them in the
  preview's EPUB editor: info, cover, find and replace, a visual or XHTML view
  (in colour), and saving back to the phone. Unedited parts of an EPUB keep their exact bytes.

Press ? for the keyboard shortcuts. ⌘K opens the commands: type `>` first for actions only, or `:`
to find files and folders on the computer by name, from fsd's index.

## Connecting a phone

- **adb**: turn on USB debugging and install adb
  (`brew install --cask android-platform-tools`). It's fast. Paths look like
  `/sdcard/Download`. An SD card in the phone shows in the sidebar under it
  (`/storage/1234-ABCD`).
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

Deleting on the computer is the exception: Delete moves items to the Trash (the
Recycle Bin on Windows), or backs them up as on a phone where the disk has none
(network drives, USB sticks on Windows). Delete permanently (⌥⌘⌫, Shift+Delete
elsewhere) keeps nothing, after asking; it's offered only on the computer.

Previews read only what they show: media and hex by range, and phone thumbnails
one at a time, only for folders on screen. A phone or server that can't send part of a
file is copied to the computer in the background, and plays while it's copied.
The copy is deleted after two minutes unused.

## Platforms

The text above is written for macOS. On Windows, Ctrl replaces ⌘ and drives
show under `/` (`/C:/Users/...`); the sidebar lists every drive, C: too, by
letter, with Eject only for those that can be taken out (USB, SD cards). On Linux, disks are mounted under `/media`,
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

Searches through fsd are the `fsd` cargo feature, off by default: run
`cargo tauri dev --features fsd` (or `build`, `cargo test --lib`) to have them. CI builds
the app without them and tests them separately. They need no fs sources: `fsd/wire.rs`
carries its own copy of fsd's protocol. Without the feature, searches walk the folders and
the palette's `:` says fsd isn't in this build.

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
    `iphone.rs` (iPhones and iPads over usbmuxd and AFC),
    `local.rs`, and the servers (`sftp.rs`, `smb.rs` (SMB and NFS), `webdav.rs`, `ftp.rs`,
    `s3.rs`, `everything.rs`). `fsd.rs` answers searches of the computer from
    fsd's index.
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
