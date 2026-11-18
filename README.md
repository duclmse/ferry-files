# yomi-files

A Mac desktop app (Tauri 2 + Leptos, all Rust) for browsing, reading and editing the files on an Android phone connected by USB, EPUB books in particular. It shows the Mac's own files too, so books can be copied between the two.

- One or two panes (⌘\ shows or hides the second; Tab switches between them). Each pane shows the phone or the Mac (📱 / 💻 in its toolbar), as a list, as thumbnails (images and EPUB covers), or in columns.
- Right-click a file or folder for open, download, copy to the other pane (F5) and delete; right-click empty space for new folder, new file, upload, refresh, the view options and the second pane. Each pane's status bar shows the folder's contents and the selected item.
- Files on the Mac can be edited too (text and EPUB).
- Double-click (or Enter) opens: folders go in, text files open in the editor, EPUBs in the EPUB editor, anything else in its default Mac app. A phone file is copied to `~/Library/Caches/yomi-files/opened` first, so changes made in the Mac app stay on the Mac. The context menu also has "Open in default app" for text files and EPUBs.
- Settings (sidebar): show hidden files (names starting with a dot; off by default, on the phone and the Mac), turn phone thumbnails off, and see, open or clear the app's folders on the Mac. "Open backup folder" in the sidebar shows the backups in the Finder.
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

The app opens on the Mac's files. While no phone is connected it looks for one on USB every few seconds; a phone it finds shows in the sidebar with an **adb | MTP** button, one half per way to connect (a half is greyed out when that way isn't ready; the highlighted one is the suggested way, adb when both work). ⏏ next to the phone's name disconnects it, and panes showing it go back to the Mac. Only one program can use MTP at a time, so quit Android File Transfer, Image Capture and other MTP tools first. If MTP still won't connect, run `killall ptpcamerad`.

## Safety

The same rules apply to files on the phone and on the Mac. Every write or delete:

1. refuses if the file changed on the phone since it was opened,
2. backs up the old version to `~/Library/Caches/yomi-files/backups` (sidebar: Open backup folder),
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
  - `app.rs`: sidebar (phone finder and places), panes and file views, context menu, text editor.
  - `epub.rs`: EPUB editor screens.
  - `api.rs`: calls to the backend, and the types shared with it.
- `src-tauri/`: the backend.
  - `device.rs`, `adb.rs`, `mtp.rs`, `local.rs`: transports (phone over adb or MTP, and the Mac's own files), version checks, backups.
  - `xml.rs`, `epub.rs`: EPUB parsing and editing.
  - `protocol.rs`: the `yomi://` scheme that serves book images, styles and fonts, and thumbnails.
  - `thumbs.rs`: thumbnails and their cache.
  - `lib.rs`: the commands the frontend calls.

## EPUB editor limits

- Find doesn't match a phrase that formatting splits in two (half of it bold, for example).
- In a text that gets replaced, special characters written as named codes the editor doesn't know (such as `&eacute;`) are written back in a different form.
- Replacing the cover with a different image type keeps the old file name.
