# yomi-files

A Mac desktop app (Tauri 2 + Leptos, all Rust) for browsing, reading and editing the files on an Android phone connected by USB, EPUB books in particular.

- Browse folders on the phone as a list, as thumbnails (images and EPUB covers), or in columns.
- Right-click a file or folder for open, download and delete; right-click empty space for new folder, new file, upload, refresh and the view options. A status bar shows the folder's contents and the selected item.
- Edit text files.
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

When both work, adb is used. Only one program can use MTP at a time, so quit Android File Transfer, Image Capture and other MTP tools first. If MTP still won't connect, run `killall ptpcamerad`.

## Safety

Every write or delete:

1. refuses if the file changed on the phone since it was opened,
2. backs up the old version to `~/Library/Caches/yomi-files/backups`,
3. writes the new version and checks it.

Thumbnails need the whole file read from the phone (slow over MTP), so they are made one at a time, only for the folder on screen, and only for files up to 60 MB. They are cached in `~/Library/Caches/yomi-files/thumbs`; delete that folder to clear them.

## Build and run

Requirements:

- Rust with the `wasm32-unknown-unknown` target
- `trunk`
- the Tauri CLI (`cargo install tauri-cli`)
- adb and/or libmtp, as above

```sh
cargo tauri dev                       # run in development
cargo tauri build --debug             # build target/debug/bundle/macos/yomi-files.app
cd src-tauri && cargo test --lib      # unit tests (XML tokenizer, EPUB editing)
```

Tests against a connected phone use scratch files only, and they delete those files and their backups afterwards:

```sh
cd src-tauri
cargo run --example smoke         [-- adb|mtp]   # list, read, write, conflict check, delete
cargo run --example epub_smoke    [-- adb|mtp]   # edits a copy of an .epub found on the phone
```

## Layout

- `src/`: the frontend (Leptos 0.8, client-side, built by Trunk into `dist/`).
  - `app.rs`: connect screen, file views, context menu, text editor.
  - `epub.rs`: EPUB editor screens.
  - `api.rs`: calls to the backend, and the types shared with it.
- `src-tauri/`: the backend.
  - `device.rs`, `adb.rs`, `mtp.rs`: transports, version checks, backups.
  - `xml.rs`, `epub.rs`: EPUB parsing and editing.
  - `protocol.rs`: the `yomi://` scheme that serves book images, styles and fonts, and thumbnails.
  - `thumbs.rs`: thumbnails and their cache.
  - `lib.rs`: the commands the frontend calls.

## EPUB editor limits

- Find doesn't match a phrase that formatting splits in two (half of it bold, for example).
- In a text that gets replaced, special characters written as named codes the editor doesn't know (such as `&eacute;`) are written back in a different form.
- Replacing the cover with a different image type keeps the old file name.
