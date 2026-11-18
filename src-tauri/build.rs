fn main() {
    // Declaring the app's commands makes them need a permission, granted to the main window
    // in capabilities/default.json.
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(tauri_build::AppManifest::new().commands(&[
        "devices", "connect", "disconnect", "connections", "info", "list", "refresh", "read_text", "write_text", "mkdir", "remove", "download", "upload", "backups", "restore", "eject", "quick_look", "find", "transfer", "cancel_transfer", "compare", "sync", "rename", "space", "open_file", "app_folders", "open_folder", "clear_cache",
        "epub_open", "epub_chapter", "epub_set_source", "epub_set_body", "epub_set_meta", "epub_cover", "epub_find", "epub_replace", "epub_save", "epub_export", "epub_close",
    ])))
    .expect("failed to run tauri-build");
}
