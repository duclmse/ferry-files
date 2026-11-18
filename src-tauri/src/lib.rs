pub mod adb;
pub mod device;
pub mod epub;
pub mod local;
pub mod mtp;
mod protocol;
pub mod thumbs;
pub mod xml;

use device::{DevError, Device, Entry, WriteResult};
use serde::Serialize;
use epub::Book;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_dialog::DialogExt;

/// Text files bigger than this open as downloads instead of in the editor.
const TEXT_MAX: u64 = 5 * 1024 * 1024;

pub struct AppState {
    dev: Mutex<Option<Arc<Device>>>,
    /// The Mac's own files, always available.
    mac: Arc<Device>,
    /// Open EPUBs by id. protocol.rs serves their files to the chapter view.
    pub books: Mutex<HashMap<u32, Book>>,
    next_book: AtomicU32,
    /// Where each open book came from ("device" or "mac"), so it's saved back there.
    book_src: Mutex<HashMap<u32, String>>,
    /// The device folder each pane is showing. Thumbnails for other device folders aren't made.
    viewing: Mutex<HashMap<u32, String>>,
    /// Held while a thumbnail is being made, so they're made one at a time.
    pub thumb_lock: Mutex<()>,
}

impl Default for AppState {
    fn default() -> Self {
        AppState {
            dev: Mutex::default(),
            mac: Arc::new(Device::new(Box::new(local::Local))),
            books: Mutex::default(),
            next_book: AtomicU32::default(),
            book_src: Mutex::default(),
            viewing: Mutex::default(),
            thumb_lock: Mutex::default(),
        }
    }
}

impl AppState {
    /// The files to work on: "mac" for the Mac's own, anything else (or nothing) for the phone.
    pub fn source(&self, src: Option<&str>) -> Result<Arc<Device>, String> {
        match src {
            Some("mac") => Ok(self.mac.clone()),
            _ => self.device(),
        }
    }

    pub fn device(&self) -> Result<Arc<Device>, String> {
        self.dev
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| "Not connected to a device.".to_string())
    }

    pub fn is_viewing(&self, dir: &str) -> bool {
        self.viewing.lock().unwrap_or_else(|e| e.into_inner()).values().any(|v| v == dir)
    }
}

fn msg(e: DevError) -> String {
    e.to_string()
}

/// Runs device work off the main thread so the window stays responsive (MTP can be slow).
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| e.to_string())?
}

#[derive(Serialize)]
struct Devices {
    adb: Vec<String>,
    adb_found: bool,
    mtp: Vec<String>,
    mtp_error: Option<String>,
}

#[derive(Serialize)]
struct Info {
    label: String,
    transport: &'static str,
    default_dir: String,
    quick_dirs: Vec<(String, String)>,
    backup_dir: String,
}

fn info_of(d: &Device) -> Info {
    Info {
        label: d.label(),
        transport: d.kind(),
        default_dir: d.default_dir(),
        quick_dirs: d.quick_dirs(),
        backup_dir: device::backup_dir().display().to_string(),
    }
}

#[tauri::command(rename_all = "snake_case")]
async fn devices() -> Result<Devices, String> {
    blocking(|| {
        let (mtp, mtp_error) = match mtp::detect() {
            Ok(v) => (v, None),
            Err(e) => (vec![], Some(e.to_string())),
        };
        Ok(Devices { adb: adb::ready_serials(), adb_found: adb::adb_path().is_some(), mtp, mtp_error })
    })
    .await
}

#[tauri::command(rename_all = "snake_case")]
async fn connect(state: State<'_, AppState>, transport: String, serial: Option<String>) -> Result<Info, String> {
    // Release the old device first: MTP allows only one open session.
    state.dev.lock().unwrap_or_else(|e| e.into_inner()).take();
    let dev = blocking(move || device::connect(&transport, serial).map_err(msg)).await?;
    let info = info_of(&dev);
    *state.dev.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(dev));
    Ok(info)
}

#[tauri::command(rename_all = "snake_case")]
fn disconnect(state: State<'_, AppState>) {
    state.dev.lock().unwrap_or_else(|e| e.into_inner()).take();
}

/// The connected phone, or with `src: "mac"`, the Mac's own files.
#[tauri::command(rename_all = "snake_case")]
fn info(state: State<'_, AppState>, src: Option<String>) -> Option<Info> {
    state.source(src.as_deref()).ok().map(|d| info_of(&d))
}

/// Lists a folder. `current` (default true) says it's the folder `pane` (default 0) now shows;
/// the column view passes false for the folders to its left.
#[tauri::command(rename_all = "snake_case")]
async fn list(
    state: State<'_, AppState>,
    src: Option<String>,
    path: String,
    current: Option<bool>,
    pane: Option<u32>,
) -> Result<Vec<Entry>, String> {
    let dev = state.source(src.as_deref())?;
    if current.unwrap_or(true) {
        let mut viewing = state.viewing.lock().unwrap_or_else(|e| e.into_inner());
        let pane = pane.unwrap_or(0);
        if src.as_deref() == Some("mac") {
            viewing.remove(&pane);
        } else {
            viewing.insert(pane, device::norm(&path).map_err(msg)?);
        }
    }
    blocking(move || dev.listdir(&path).map_err(msg)).await
}

#[tauri::command(rename_all = "snake_case")]
async fn refresh(state: State<'_, AppState>, src: Option<String>) -> Result<(), String> {
    let dev = state.source(src.as_deref())?;
    blocking(move || {
        dev.refresh();
        Ok(())
    })
    .await
}

#[derive(Serialize)]
struct TextFile {
    text: String,
    version: String,
}

#[tauri::command(rename_all = "snake_case")]
async fn read_text(state: State<'_, AppState>, src: Option<String>, path: String) -> Result<TextFile, String> {
    let dev = state.source(src.as_deref())?;
    blocking(move || {
        let st = dev.stat(&path).map_err(msg)?.ok_or_else(|| format!("No such file: {path}"))?;
        if st.dir {
            return Err(format!("{path} is a folder."));
        }
        if st.size > TEXT_MAX {
            return Err(format!("{path} is too big to edit here ({} MB). Download it instead.", st.size / 1024 / 1024));
        }
        let (data, version) = dev.read(&path).map_err(msg)?;
        let text = String::from_utf8(data).map_err(|_| format!("{path} isn't UTF-8 text. Download it instead."))?;
        Ok(TextFile { text, version })
    })
    .await
}

#[tauri::command(rename_all = "snake_case")]
async fn write_text(
    state: State<'_, AppState>,
    src: Option<String>,
    path: String,
    text: String,
    base_version: Option<String>,
    create: bool,
) -> Result<WriteResult, String> {
    let dev = state.source(src.as_deref())?;
    blocking(move || dev.write(&path, text.as_bytes(), base_version.as_deref(), create).map_err(msg)).await
}

#[tauri::command(rename_all = "snake_case")]
async fn mkdir(state: State<'_, AppState>, src: Option<String>, path: String) -> Result<(), String> {
    let dev = state.source(src.as_deref())?;
    blocking(move || dev.mkdir(&path).map_err(msg)).await
}

#[tauri::command(rename_all = "snake_case")]
async fn remove(state: State<'_, AppState>, src: Option<String>, path: String) -> Result<Option<String>, String> {
    let dev = state.source(src.as_deref())?;
    blocking(move || dev.remove(&path).map_err(msg)).await
}

/// Asks where to save, then copies the device file to the Mac. Returns the Mac path, or None if cancelled.
#[tauri::command(rename_all = "snake_case")]
async fn download(app: AppHandle, state: State<'_, AppState>, path: String) -> Result<Option<String>, String> {
    let dev = state.device()?;
    blocking(move || {
        let Some(dest) = app.dialog().file().set_file_name(device::basename(&path)).blocking_save_file() else {
            return Ok(None);
        };
        let dest = dest.into_path().map_err(|e| e.to_string())?;
        let (data, _) = dev.read(&path).map_err(msg)?;
        std::fs::write(&dest, data).map_err(|e| format!("Can't write {}: {e}", dest.display()))?;
        Ok(Some(dest.display().to_string()))
    })
    .await
}

/// Asks for Mac files and copies them into a device folder. Existing files are replaced
/// (the old version is backed up first). Returns one line per file.
#[tauri::command(rename_all = "snake_case")]
async fn upload(app: AppHandle, state: State<'_, AppState>, dir: String) -> Result<Vec<String>, String> {
    let dev = state.device()?;
    blocking(move || {
        let Some(files) = app.dialog().file().blocking_pick_files() else { return Ok(vec![]) };
        let mut report = Vec::new();
        for f in files {
            let local = f.into_path().map_err(|e| e.to_string())?;
            let name = local.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let target = device::join(&dir, &name);
            let line = match std::fs::read(&local) {
                Err(e) => format!("{name}: can't read it ({e})"),
                Ok(data) => match dev.write(&target, &data, None, false) {
                    Ok(r) => match r.backup {
                        Some(_) => format!("{name}: replaced (old version backed up)"),
                        None => format!("{name}: uploaded"),
                    },
                    Err(e) => format!("{name}: {e}"),
                },
            };
            report.push(line);
        }
        Ok(report)
    })
    .await
}

/// Copies a file between the phone and the Mac (or within one) into folder `to_dir`, keeping its
/// name. Refuses if a file of that name is already there. Returns the new file's path.
#[tauri::command(rename_all = "snake_case")]
async fn copy(
    state: State<'_, AppState>,
    from_src: String,
    from: String,
    to_src: String,
    to_dir: String,
) -> Result<String, String> {
    let (a, b) = (state.source(Some(&from_src))?, state.source(Some(&to_src))?);
    blocking(move || {
        let st = a.stat(&from).map_err(msg)?.ok_or_else(|| format!("No such file: {from}"))?;
        if st.dir {
            return Err(format!("{} is a folder. Only files can be copied.", st.name));
        }
        let target = device::join(&to_dir, &st.name);
        let (data, _) = a.read(&from).map_err(msg)?;
        b.write(&target, &data, None, true).map_err(|e| match e {
            DevError::Conflict(_) => format!("{} already exists there. Nothing was copied.", st.name),
            e => e.to_string(),
        })?;
        Ok(target)
    })
    .await
}

/// Opens a file in its default Mac app. Phone files are copied to the app's cache first
/// (`~/Library/Caches/yomi-files/opened`), so changes made there don't go back to the phone.
/// Returns the path that was opened.
#[tauri::command(rename_all = "snake_case")]
async fn open_file(state: State<'_, AppState>, src: Option<String>, path: String) -> Result<String, String> {
    let mac = src.as_deref() == Some("mac");
    let dev = state.source(src.as_deref())?;
    blocking(move || {
        let local = if mac {
            std::path::PathBuf::from(&path)
        } else {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            path.hash(&mut h);
            // One folder per phone path, so files with the same name don't overwrite each other.
            let dir = opened_dir().join(format!("{:016x}", h.finish()));
            std::fs::create_dir_all(&dir).map_err(|e| format!("Can't create {}: {e}", dir.display()))?;
            let (data, _) = dev.read(&path).map_err(msg)?;
            let local = dir.join(device::basename(&path));
            std::fs::write(&local, data).map_err(|e| format!("Can't write {}: {e}", local.display()))?;
            local
        };
        let status = std::process::Command::new("open").arg(&local).status().map_err(|e| format!("Can't open {}: {e}", local.display()))?;
        if !status.success() {
            return Err(format!("No app on this Mac opens {}.", device::basename(&path)));
        }
        Ok(local.display().to_string())
    })
    .await
}

/// Where copies of phone files opened in Mac apps go.
fn opened_dir() -> std::path::PathBuf {
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("yomi-files").join("opened")
}

// ----- Settings: the app's folders on the Mac -----

#[derive(Serialize)]
struct Folder {
    path: String,
    files: u64,
    bytes: u64,
}

/// The folders the app keeps on the Mac: "backups", "thumbs" (the thumbnail cache) or "opened"
/// (copies of phone files opened in Mac apps).
fn app_folder(which: &str) -> Result<std::path::PathBuf, String> {
    match which {
        "backups" => Ok(device::backup_dir()),
        "thumbs" => Ok(thumbs::cache_dir()),
        "opened" => Ok(opened_dir()),
        other => Err(format!("Unknown folder {other:?}.")),
    }
}

fn folder_size(dir: &std::path::Path) -> (u64, u64) {
    let (mut files, mut bytes) = (0, 0);
    for item in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        match item.metadata() {
            Ok(m) if m.is_dir() => {
                let (f, b) = folder_size(&item.path());
                files += f;
                bytes += b;
            }
            Ok(m) => {
                files += 1;
                bytes += m.len();
            }
            Err(_) => {}
        }
    }
    (files, bytes)
}

/// Where the backups and the thumbnail cache are, and how much they hold.
#[tauri::command(rename_all = "snake_case")]
async fn app_folders() -> Result<HashMap<String, Folder>, String> {
    blocking(|| {
        let mut out = HashMap::new();
        for which in ["backups", "thumbs", "opened"] {
            let dir = app_folder(which)?;
            let (files, bytes) = folder_size(&dir);
            out.insert(which.to_string(), Folder { path: dir.display().to_string(), files, bytes });
        }
        Ok(out)
    })
    .await
}

/// Shows one of the app's folders in the Finder, creating it if it isn't there yet.
#[tauri::command(rename_all = "snake_case")]
fn open_folder(which: String) -> Result<(), String> {
    let dir = app_folder(&which)?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("Can't create {}: {e}", dir.display()))?;
    std::process::Command::new("open").arg(&dir).spawn().map_err(|e| format!("Can't open {}: {e}", dir.display()))?;
    Ok(())
}

/// Empties the thumbnail cache (made again when shown) or the copies of opened phone files.
/// Backups can't be cleared from the app.
#[tauri::command(rename_all = "snake_case")]
async fn clear_cache(which: String) -> Result<(), String> {
    blocking(move || {
        if !matches!(which.as_str(), "thumbs" | "opened") {
            return Err(format!("{which:?} can't be cleared."));
        }
        let dir = app_folder(&which)?;
        match std::fs::remove_dir_all(&dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("Can't clear {}: {e}", dir.display())),
            _ => Ok(()),
        }
    })
    .await
}

// ----- EPUB editor -----
// A book is read from the device once, edited in memory, and written back by epub_save.

#[derive(Serialize)]
struct BookView {
    id: u32,
    /// "device" or "mac": where the book is saved back to.
    src: String,
    path: String,
    name: String,
    dirty: bool,
    meta: epub::Meta,
    chapters: Vec<epub::ChapterRef>,
    cover: Option<String>,
    stats: epub::Stats,
}

fn book_view(id: u32, src: &str, b: &Book) -> Result<BookView, String> {
    Ok(BookView {
        id,
        src: src.to_string(),
        path: b.source.clone(),
        name: device::basename(&b.source).to_string(),
        dirty: b.dirty,
        meta: b.meta()?,
        chapters: b.chapters()?,
        cover: b.cover_path().map(|p| protocol::book_url(id, &p, b.rev)),
        stats: b.stats()?,
    })
}

/// Runs `f` on open book `id`, then returns the refreshed view of the book.
fn with_book<T>(state: &AppState, id: u32, f: impl FnOnce(&mut Book) -> Result<T, String>) -> Result<(T, BookView), String> {
    let src = state.book_src.lock().unwrap_or_else(|e| e.into_inner()).get(&id).cloned().unwrap_or_default();
    let mut books = state.books.lock().unwrap_or_else(|e| e.into_inner());
    let book = books.get_mut(&id).ok_or("This book is no longer open.")?;
    let out = f(book)?;
    Ok((out, book_view(id, &src, book)?))
}

#[tauri::command(rename_all = "snake_case")]
async fn epub_open(state: State<'_, AppState>, src: Option<String>, path: String) -> Result<BookView, String> {
    let dev = state.source(src.as_deref())?;
    let book = blocking(move || {
        let (data, version) = dev.read(&path).map_err(msg)?;
        Book::open(&data, path, version)
    })
    .await?;
    let id = state.next_book.fetch_add(1, Ordering::Relaxed);
    let src = if src.as_deref() == Some("mac") { "mac" } else { "device" };
    let view = book_view(id, src, &book)?;
    state.books.lock().unwrap_or_else(|e| e.into_inner()).insert(id, book);
    state.book_src.lock().unwrap_or_else(|e| e.into_inner()).insert(id, src.to_string());
    Ok(view)
}

#[derive(Serialize)]
struct ChapterDoc {
    source: String,
    /// The chapter as HTML for the visual editor, or None when its XHTML is broken.
    html: Option<String>,
    problem: Option<String>,
}

#[tauri::command(rename_all = "snake_case")]
fn epub_chapter(state: State<'_, AppState>, id: u32, path: String) -> Result<ChapterDoc, String> {
    let (doc, _) = with_book(&state, id, |b| {
        let source = b.text(&path)?;
        let (html, problem) = match b.visual_html(&path, &protocol::book_base(id, &path)) {
            Ok(h) => (Some(h), None),
            Err(e) => (None, Some(e)),
        };
        Ok(ChapterDoc { source, html, problem })
    })?;
    Ok(doc)
}

#[tauri::command(rename_all = "snake_case")]
fn epub_set_source(state: State<'_, AppState>, id: u32, path: String, source: String) -> Result<BookView, String> {
    Ok(with_book(&state, id, |b| b.set_source(&path, &source))?.1)
}

#[tauri::command(rename_all = "snake_case")]
fn epub_set_body(state: State<'_, AppState>, id: u32, path: String, body: String) -> Result<BookView, String> {
    Ok(with_book(&state, id, |b| b.set_body(&path, &body))?.1)
}

#[tauri::command(rename_all = "snake_case")]
fn epub_set_meta(state: State<'_, AppState>, id: u32, meta: epub::Meta) -> Result<BookView, String> {
    Ok(with_book(&state, id, |b| b.set_meta(&meta))?.1)
}

/// Asks for an image on the Mac and makes it the cover. None if cancelled.
#[tauri::command(rename_all = "snake_case")]
async fn epub_cover(app: AppHandle, state: State<'_, AppState>, id: u32) -> Result<Option<BookView>, String> {
    let picked = blocking(move || {
        let Some(f) = app
            .dialog()
            .file()
            .add_filter("Images", &["jpg", "jpeg", "png", "gif", "webp", "svg"])
            .blocking_pick_file()
        else {
            return Ok(None);
        };
        let local = f.into_path().map_err(|e| e.to_string())?;
        let ext = local.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
        let data = std::fs::read(&local).map_err(|e| format!("Can't read {}: {e}", local.display()))?;
        Ok(Some((data, ext)))
    })
    .await?;
    let Some((data, ext)) = picked else { return Ok(None) };
    Ok(Some(with_book(&state, id, |b| b.set_cover(data, &ext))?.1))
}

#[tauri::command(rename_all = "snake_case")]
fn epub_find(state: State<'_, AppState>, id: u32, query: epub::Query) -> Result<epub::Found, String> {
    Ok(with_book(&state, id, |b| b.find(&query))?.0)
}

#[derive(Serialize)]
struct Replaced {
    count: usize,
    book: BookView,
}

#[tauri::command(rename_all = "snake_case")]
fn epub_replace(state: State<'_, AppState>, id: u32, query: epub::Query) -> Result<Replaced, String> {
    let (count, book) = with_book(&state, id, |b| b.replace_all(&query))?;
    Ok(Replaced { count, book })
}

#[derive(Serialize)]
struct Saved {
    book: BookView,
    backup: bool,
}

/// Writes the book back where it came from. Refuses if that copy changed since it was opened.
#[tauri::command(rename_all = "snake_case")]
async fn epub_save(state: State<'_, AppState>, id: u32) -> Result<Saved, String> {
    let src = state.book_src.lock().unwrap_or_else(|e| e.into_inner()).get(&id).cloned();
    let dev = state.source(src.as_deref())?;
    let (bytes, path, version) = with_book(&state, id, |b| {
        b.touch_modified();
        Ok((b.to_bytes()?, b.source.clone(), b.version.clone()))
    })?
    .0;
    let r = blocking(move || dev.write(&path, &bytes, Some(&version), false).map_err(msg)).await?;
    let (_, book) = with_book(&state, id, |b| {
        b.version = r.version.clone();
        b.dirty = false;
        Ok(())
    })?;
    Ok(Saved { book, backup: r.backup.is_some() })
}

/// Saves a copy of the book (with any unsaved edits) on the Mac. Returns its path, or None if cancelled.
#[tauri::command(rename_all = "snake_case")]
async fn epub_export(app: AppHandle, state: State<'_, AppState>, id: u32) -> Result<Option<String>, String> {
    let ((bytes, name), _) = with_book(&state, id, |b| Ok((b.to_bytes()?, device::basename(&b.source).to_string())))?;
    blocking(move || {
        let Some(dest) = app.dialog().file().set_file_name(name).blocking_save_file() else { return Ok(None) };
        let dest = dest.into_path().map_err(|e| e.to_string())?;
        std::fs::write(&dest, bytes).map_err(|e| format!("Can't write {}: {e}", dest.display()))?;
        Ok(Some(dest.display().to_string()))
    })
    .await
}

#[tauri::command(rename_all = "snake_case")]
fn epub_close(state: State<'_, AppState>, id: u32) {
    state.books.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
    state.book_src.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::default())
        .register_asynchronous_uri_scheme_protocol("yomi", |ctx, request, responder| {
            let app = ctx.app_handle().clone();
            std::thread::spawn(move || responder.respond(protocol::handle(&app, request)));
        })
        .invoke_handler(tauri::generate_handler![
            devices, connect, disconnect, info, list, refresh, read_text, write_text, mkdir, remove, download,
            upload, copy, open_file, app_folders, open_folder, clear_cache, epub_open, epub_chapter, epub_set_source, epub_set_body, epub_set_meta, epub_cover, epub_find,
            epub_replace, epub_save, epub_export, epub_close
        ])
        .run(tauri::generate_context!())
        .expect("error while running yomi-files");
}

pub fn state(app: &AppHandle) -> tauri::State<'_, AppState> {
    app.state::<AppState>()
}
