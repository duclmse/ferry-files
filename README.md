# Ferry Files

A desktop file browser for macOS, Windows and Linux (Tauri 2 + Leptos, all Rust) for Android
phones, iPhones and other MTP devices over USB, servers, and the computer's own files and disks,
with tools for EPUB books.

## Features

- **Panes**: one or two (⌘\\), each with tabs (⌘T, ⌘W; drag them between panes), shown as a
  list, thumbnails or columns. The list can show a folder tree and group items by name, kind,
  date or size. Sort, filter (⌘F), show hidden files (⌘.), type a path (⌘L). Edges resize and
  sizes are remembered. Typing a name's first letters picks it. Folders on the computer list
  again when fsd sees them change, and Get Info (Properties) opens the system's window for them.
- **Preview** beside the files:
  - pictures: zoom, turn, full screen (F), the next one read ahead;
  - video and audio streamed from the device, with full screen, a pop-out window and
    subtitles (.srt, .sub, .vtt; named like the video or dropped on it, two at once);
  - text and code in colour (about 50 languages), and hex with go-to-offset;
  - PDFs drawn by PDFium, with an outline, search, selectable text and links (without PDFium,
    the web view's viewer);
  - fonts, EPUBs (cover or a chapter at a time, with Edit for the EPUB editor), and ZIP, APK
    and JAR contents, read without unpacking.
- **Files**: copy, move, rename, delete, new file or folder, by menu, F5/F6, ⌘C/⌘X/⌘V (shared
  with Finder) or drag and drop. Copies merge folders, never replace a file, show progress and
  can be cancelled. A file over 4 GB isn't copied to a FAT32 disk.
- **Filter and search**: the Filter box (⌘F) filters the open folder as you type, by name,
  pattern or regular expression (with match case and whole word), and by kind, size and date;
  Replace with (⌘R) renames what it shows, and can preserve case. On an Everything server it
  searches the server's index under the folder, or all drives (the search button beside the
  server in the sidebar). ⌘⇧F finds files anywhere on the
  computer from fsd's index (the search service of [fs](fs/), in this repo), in fsd's search syntax
  (`case:`, `ww:`, `regex:`, `ext:`, `size:`, `dm:`, `|`, `!` …), with match case, whole word
  and regular expression buttons, 100 at a time, or the words inside text files (when
  Settings > Search reads them). The app carries fsd, and Settings > Search installs it as a
  service, shows its index, and changes what it indexes.
- **Scripts**: Lua 5.4 scripts run on the files picked (right-click, Run script). They can read
  and change files only through the app, so what they replace or delete is backed up.
  [docs/scripting.md](docs/scripting.md) is the guide.
- **Duplicates**: files with the same contents across folders, on any devices. For one folder
  on the computer, fsd's index gives the files of a shared size, so the folder isn't walked.
- **Sync** a folder to the other pane: copy missing, update or mirror, with a list of what will
  happen first. Pairs can be saved.
- **Sidebar**: favourites, workspaces, disks (with eject), devices, servers, sync pairs, backups
  and history, as trees. Sections can be moved and hidden.
- **Connections**: SFTP, SMB, NFS, WebDAV, FTP and FTPS, S3 (AWS, R2, B2, MinIO), and
  Everything (voidtools) over HTTP, and fsd on another computer (its REST API). Everything and
  fsd are read-only, and the filter box searches their index. Passwords, keys and tokens stay
  in the system keychain.
- **Terminal** in a pane, on this computer, an adb phone or an SFTP server.
- **Editor view**: a folder tree beside text files in tabs.
- **Books**: list every EPUB under a folder; check them, change their info, or make smaller
  copies. Read them a chapter at a time (⌘F finds in the whole book), or edit them: info, cover,
  table of contents, find and replace, a visual or XHTML view, and moving text between chapters.
  Unedited parts keep their exact bytes.

Press ? for the keyboard shortcuts. ⌘K opens the commands (`>` for actions only, `:` to find
files with fsd, in its search syntax).

## Connecting a phone

- **adb**: turn on USB debugging and install adb (`brew install --cask android-platform-tools`).
  Fast. Paths look like `/sdcard/Download`; an SD card shows under the phone.
- **MTP**: the phone's "File transfer" mode. Paths start with the storage name
  (`/Internal storage/Download`). macOS and Linux use libmtp, which the macOS app bundles;
  Windows uses its own MTP support.

Phones connect when plugged in (Settings can turn that off). Only one program can use MTP at a
time: quit Android File Transfer, OpenMTP and the like, or run `scripts/free-mtp.sh` to see and
stop what holds the phone.

## Safety

Every write or delete, on a phone or the computer:

1. refuses if the file changed since it was opened,
2. backs up the old version (Backups in the sidebar),
3. writes the new version and checks it.

On the computer, Delete moves items to the Trash (or backs them up where the disk has none), and
Delete permanently (⌥⌘⌫) keeps nothing, after asking. Previews read only what they show.

## Platforms

The text above is written for macOS. On Windows, Ctrl replaces ⌘ and drives show under `/`
(`/C:/Users/...`). On Linux, disks are mounted under `/media`, `/run/media` or `/mnt`, and MTP
needs libmtp's udev rules. CI builds Windows and Linux, but they've seen little real use.

## Build and run

Requirements: Rust with the `wasm32-unknown-unknown` target, `trunk`, the Tauri CLI
(`cargo install tauri-cli`), and adb and/or libmtp. Linux also needs
`libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev patchelf libmtp-dev`.

```sh
cargo tauri dev                    # run in development
cargo tauri build --debug          # build the app bundle
cargo test                         # frontend tests
cd src-tauri && cargo test --lib   # backend tests
```

- **libmtp**: the macOS build bundles Homebrew's libmtp and libusb (`brew install libmtp`).
- **PDFium**: `scripts/fetch-pdfium.sh` downloads it into `src-tauri/libs/pdfium`. The macOS
  bundle runs it; on Linux and Windows run it once before building.
- **fsd**: searches through fsd are the `fsd` cargo feature, which `cargo tauri dev` and
  `cargo tauri build` turn on; add `--features fsd` to plain `cargo build` and `cargo test`.
  The bundle step builds fsd from `fs/crates/fs-server` and carries it as `ferry-fsd`; plain
  `cargo build` needs nothing for it.

Tests against a real phone use scratch files only and clean up after themselves:

```sh
cd src-tauri
cargo run --example smoke      [-- adb|mtp]
cargo run --example epub_smoke [-- adb|mtp]
```

## Layout

- `src/`: the frontend (Leptos 0.8, built by Trunk). `app.rs` holds the shared state, `app/`
  has one file per part, and `api.rs` the backend calls and shared types.
- `styles/`: the CSS, one file per part; `base.css` has the colour tokens.
- `src-tauri/src/`: the backend.
  - `device.rs`: the `Device` every read and write goes through; `local.rs`: the computer.
  - `devices/`: adb, MTP (`wpd.rs` on Windows), iPhones, and finding and connecting them.
  - `servers/`: SFTP, SMB and NFS, WebDAV, FTP, S3, Everything, another computer's fsd, and
    the keychain.
  - `books/`: EPUB reading, editing and batch work.
  - `preview/`: ZIPs, PDFs, media, thumbnails, colouring, subtitles and streaming.
  - `files.rs`, `transfer.rs`, `safety.rs`, `scripts.rs`, `terminal.rs`: file work.
  - `fsd.rs` and `fsd_service.rs`: searching with fsd, and installing and setting it up.
  - `platform.rs`: what differs between operating systems; `protocol.rs`: the `ferry://`
    scheme; `lib.rs`: the app state and the command list.
- `fs/`: the fs project, with its history: fsd (the indexing daemon, `fs-server`), the `fs`
  command line (`fs-cli`), the wire format and client (`fs-protocol`), and the rar2zip and
  sftpd extensions. Its crates are members of this workspace (one `Cargo.lock`, one
  `target/`); `cargo fs-build`, `fs-test` and `fs-clippy` build, test and lint them alone.
  Ferry uses only `fs-protocol`, to talk to fsd. [fs/README.md](fs/README.md) and
  [fs/CLAUDE.md](fs/CLAUDE.md) say more.

## EPUB editor limits

- Find matches within a paragraph, not across paragraphs, line breaks or pictures.
- Moving part of a chapter between chapters in different folders is refused when its styles
  name files (`url()`).
