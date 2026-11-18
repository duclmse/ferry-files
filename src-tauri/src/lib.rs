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

/// A connected device.
struct Conn {
    dev: Arc<Device>,
    /// What it is on USB ("adb:SERIAL" or "mtp:BUS-DEV"), so it isn't offered or connected twice.
    usb: Option<String>,
}

pub struct AppState {
    /// Connected devices by id ("d1", "d2", ...). Several can be connected at once.
    devs: Mutex<HashMap<String, Conn>>,
    next_dev: AtomicU32,
    /// The Mac's own files, always available.
    mac: Arc<Device>,
    /// Open EPUBs by id. protocol.rs serves their files to the chapter view.
    pub books: Mutex<HashMap<u32, Book>>,
    next_book: AtomicU32,
    /// Where each open book came from ("device" or "mac"), so it's saved back there.
    book_src: Mutex<HashMap<u32, String>>,
    /// The device and folder each pane is showing. Thumbnails for other device folders aren't made.
    viewing: Mutex<HashMap<u32, (String, String)>>,
    /// Held while a thumbnail is being made, so they're made one at a time.
    pub thumb_lock: Mutex<()>,
    /// Cancel flags of running transfers, by job number (chosen by the frontend).
    jobs: Mutex<HashMap<u32, Arc<std::sync::atomic::AtomicBool>>>,
}

impl Default for AppState {
    fn default() -> Self {
        AppState {
            devs: Mutex::default(),
            next_dev: AtomicU32::new(1),
            mac: Arc::new(Device::new(Box::new(local::Local))),
            books: Mutex::default(),
            next_book: AtomicU32::default(),
            book_src: Mutex::default(),
            viewing: Mutex::default(),
            thumb_lock: Mutex::default(),
            jobs: Mutex::default(),
        }
    }
}

impl AppState {
    /// The files to work on: "mac" for the Mac's own, or a connected device's id.
    pub fn source(&self, src: Option<&str>) -> Result<Arc<Device>, String> {
        match src {
            Some("mac") => Ok(self.mac.clone()),
            Some(id) => self
                .devs()
                .get(id)
                .map(|c| c.dev.clone())
                .ok_or_else(|| "That device isn't connected any more.".to_string()),
            None => Err("No device given.".into()),
        }
    }

    fn devs(&self) -> std::sync::MutexGuard<'_, HashMap<String, Conn>> {
        self.devs.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn is_viewing(&self, src: &str, dir: &str) -> bool {
        self.viewing.lock().unwrap_or_else(|e| e.into_inner()).values().any(|(s, d)| s == src && d == dir)
    }
}

fn msg(e: DevError) -> String {
    e.to_string()
}

/// Runs device work off the main thread so the window stays responsive (MTP can be slow).
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| e.to_string())?
}

/// A device on USB that isn't connected yet.
#[derive(Serialize)]
struct Found {
    /// "adb:SERIAL" or "mtp:BUS-DEV".
    usb: String,
    transport: &'static str,
    /// What `connect` takes as `which`: the adb serial or the MTP location.
    which: String,
    name: String,
}

#[derive(Serialize)]
struct Devices {
    found: Vec<Found>,
    adb_found: bool,
    mtp_error: Option<String>,
    /// Disks mounted on the Mac (USB drives, SD cards, disk images), as (name, path).
    drives: Vec<(String, String)>,
}

#[derive(Serialize)]
struct Info {
    /// "mac", or the id ("d1", ...) that commands take as `src`.
    id: String,
    usb: Option<String>,
    label: String,
    transport: &'static str,
    default_dir: String,
    quick_dirs: Vec<(String, String)>,
    backup_dir: String,
}

fn info_of(id: &str, usb: Option<String>, d: &Device) -> Info {
    Info {
        id: id.to_string(),
        usb,
        label: d.label(),
        transport: d.kind(),
        default_dir: d.default_dir(),
        quick_dirs: d.quick_dirs(),
        backup_dir: device::backup_dir().display().to_string(),
    }
}

/// Mounted disks other than the Mac's own startup disk.
fn drives() -> Vec<(String, String)> {
    let root = std::fs::canonicalize("/").ok();
    let mut out: Vec<(String, String)> = std::fs::read_dir("/Volumes")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name.starts_with("com.apple.") {
                return None;
            }
            let path = e.path();
            // The startup disk shows in /Volumes as a link to "/".
            if std::fs::canonicalize(&path).ok() == root || !path.is_dir() {
                return None;
            }
            Some((name, path.display().to_string()))
        })
        .collect();
    out.sort();
    out
}

/// Devices on USB that aren't connected yet, and the Mac's mounted disks.
#[tauri::command(rename_all = "snake_case")]
async fn devices(state: State<'_, AppState>) -> Result<Devices, String> {
    let taken: Vec<String> = state.devs().values().filter_map(|c| c.usb.clone()).collect();
    blocking(move || {
        let mut found: Vec<Found> = adb::ready_devices()
            .into_iter()
            .map(|(serial, name)| Found { usb: format!("adb:{serial}"), transport: "adb", which: serial, name })
            .collect();
        let mtp_error = match mtp::detect() {
            Ok(v) => {
                found.extend(v.into_iter().map(|(loc, name)| Found { usb: format!("mtp:{loc}"), transport: "mtp", which: loc, name }));
                None
            }
            Err(e) => Some(e.to_string()),
        };
        found.retain(|f| !taken.contains(&f.usb));
        Ok(Devices { found, adb_found: adb::adb_path().is_some(), mtp_error, drives: drives() })
    })
    .await
}

/// Connects a device found by `devices` (`which` is its adb serial or MTP location), alongside any
/// already connected.
#[tauri::command(rename_all = "snake_case")]
async fn connect(state: State<'_, AppState>, transport: String, which: Option<String>) -> Result<Info, String> {
    let usb = which.as_ref().map(|w| format!("{transport}:{w}"));
    if usb.is_some() && state.devs().values().any(|c| c.usb == usb) {
        return Err("That device is already connected.".into());
    }
    let dev = blocking(move || device::connect(&transport, which).map_err(msg)).await?;
    let id = format!("d{}", state.next_dev.fetch_add(1, Ordering::Relaxed));
    let info = info_of(&id, usb.clone(), &dev);
    state.devs().insert(id, Conn { dev: Arc::new(dev), usb });
    Ok(info)
}

#[tauri::command(rename_all = "snake_case")]
fn disconnect(state: State<'_, AppState>, src: String) {
    state.devs().remove(&src);
    state.viewing.lock().unwrap_or_else(|e| e.into_inner()).retain(|_, (s, _)| *s != src);
}

/// The connected devices, oldest first (a reloaded window picks them up from here).
#[tauri::command(rename_all = "snake_case")]
fn connections(state: State<'_, AppState>) -> Vec<Info> {
    let devs = state.devs();
    let mut out: Vec<Info> = devs.iter().map(|(id, c)| info_of(id, c.usb.clone(), &c.dev)).collect();
    out.sort_by_key(|i| i.id[1..].parse::<u32>().unwrap_or(0));
    out
}

/// A connected device, or with `src: "mac"`, the Mac's own files.
#[tauri::command(rename_all = "snake_case")]
fn info(state: State<'_, AppState>, src: Option<String>) -> Option<Info> {
    match src.as_deref() {
        Some("mac") => Some(info_of("mac", None, &state.mac)),
        Some(id) => state.devs().get(id).map(|c| info_of(id, c.usb.clone(), &c.dev)),
        None => None,
    }
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
        match src.as_deref() {
            Some("mac") | None => viewing.remove(&pane),
            Some(s) => viewing.insert(pane, (s.to_string(), device::norm(&path).map_err(msg)?)),
        };
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
async fn download(app: AppHandle, state: State<'_, AppState>, src: String, path: String) -> Result<Option<String>, String> {
    let dev = state.source(Some(&src))?;
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
async fn upload(app: AppHandle, state: State<'_, AppState>, src: String, dir: String) -> Result<Vec<String>, String> {
    let dev = state.source(Some(&src))?;
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
/// Progress of a transfer, sent as the "transfer" event after each file.
#[derive(Clone, Serialize)]
struct Progress {
    job: u32,
    files_done: u32,
    files_total: u32,
    bytes_done: u64,
    bytes_total: u64,
    current: String,
}

#[derive(Serialize)]
struct TransferReport {
    copied: u32,
    /// Name and reason, for each item that wasn't copied (or moved).
    skipped: Vec<(String, String)>,
    cancelled: bool,
    /// Backups of moved files, made when their originals were removed.
    backups: Vec<String>,
}

/// One file or folder to make at the target, found by walking the sources.
struct Item {
    from: String,
    to: String,
    dir: bool,
    size: u64,
}

/// Lists everything under `from` (itself first), with where it goes under `to_dir`.
fn walk(dev: &Device, from: &str, to_dir: &str, out: &mut Vec<Item>) -> Result<(), String> {
    let st = dev.stat(from).map_err(msg)?.ok_or_else(|| format!("No such file or folder: {from}"))?;
    let to = device::join(to_dir, &st.name);
    out.push(Item { from: from.to_string(), to: to.clone(), dir: st.dir, size: st.size });
    if st.dir {
        let mut kids = dev.listdir(from).map_err(msg)?;
        kids.sort_by(|a, b| a.name.cmp(&b.name));
        for k in kids {
            walk(dev, &k.path, &to, out)?;
        }
    }
    Ok(())
}

/// The work of `transfer`, apart from the app, so it can be tested.
#[allow(clippy::too_many_arguments)]
fn run_transfer(
    a: &Device,
    b: &Device,
    same: bool,
    job: u32,
    paths: &[String],
    to_dir: &str,
    moving: bool,
    stop: &std::sync::atomic::AtomicBool,
    emit: impl Fn(Progress),
) -> Result<TransferReport, String> {
    let mut report = TransferReport { copied: 0, skipped: vec![], cancelled: false, backups: vec![] };
    let mut items = vec![];
    for p in paths {
        if same && (device::dirname(p) == to_dir || to_dir == p || to_dir.starts_with(&format!("{p}/"))) {
            let why = if device::dirname(p) == to_dir { "It's already in this folder." } else { "A folder can't go inside itself." };
            report.skipped.push((device::basename(p).to_string(), why.into()));
            continue;
        }
        walk(a, p, to_dir, &mut items)?;
    }
    run_items(a, b, job, &items, moving, stop, emit, report)
}

/// Copies (or moves) each item in turn: folders are made (or merged into), files are written
/// only where nothing is there yet.
#[allow(clippy::too_many_arguments)]
fn run_items(
    a: &Device,
    b: &Device,
    job: u32,
    items: &[Item],
    moving: bool,
    stop: &std::sync::atomic::AtomicBool,
    emit: impl Fn(Progress),
    mut report: TransferReport,
) -> Result<TransferReport, String> {
    let files_total = items.iter().filter(|i| !i.dir).count() as u32;
    let bytes_total: u64 = items.iter().map(|i| i.size).sum();
    let mut p = Progress { job, files_done: 0, files_total, bytes_done: 0, bytes_total, current: String::new() };
    // Folders that couldn't be made, so what's inside them is skipped too.
    let mut failed: Vec<String> = vec![];
    // Folders whose contents were all moved, removed afterwards (deepest first).
    let mut emptied: Vec<String> = vec![];
    for it in items {
        if stop.load(Ordering::Relaxed) {
            report.cancelled = true;
            break;
        }
        let name = device::basename(&it.from).to_string();
        if failed.iter().any(|f| it.from.starts_with(&format!("{f}/"))) {
            continue;
        }
        p.current = name.clone();
        emit(p.clone());
        if it.dir {
            match b.stat(&it.to).map_err(msg)? {
                Some(e) if e.dir => {}
                Some(_) => {
                    report.skipped.push((name, "A file with that name is already there.".into()));
                    failed.push(it.from.clone());
                    continue;
                }
                None => {
                    if let Err(e) = b.mkdir(&it.to) {
                        report.skipped.push((name, e.to_string()));
                        failed.push(it.from.clone());
                        continue;
                    }
                }
            }
            if moving {
                emptied.push(it.from.clone());
            }
            continue;
        }
        let done = (|| -> Result<(), String> {
            let (data, _) = a.read(&it.from).map_err(msg)?;
            b.write(&it.to, &data, None, true).map_err(|e| match e {
                DevError::Conflict(_) => "Already there.".to_string(),
                e => e.to_string(),
            })?;
            if moving {
                if let Some(bk) = a.remove(&it.from).map_err(|e| format!("Copied, but the original wasn't removed: {e}"))? {
                    report.backups.push(bk);
                }
            }
            Ok(())
        })();
        match done {
            Ok(()) => report.copied += 1,
            Err(e) => {
                report.skipped.push((name, e));
                // The folders holding it aren't empty, so they stay.
                emptied.retain(|d| !it.from.starts_with(&format!("{d}/")));
            }
        }
        p.files_done += 1;
        p.bytes_done += it.size;
    }
    if !report.cancelled {
        for d in emptied.iter().rev() {
            // Only empty folders are removed; anything left behind keeps its folder.
            let _ = a.remove(d);
        }
    }
    p.current = String::new();
    emit(p);
    Ok(report)
}

/// Copies (or moves) files and folders, folders with everything in them, into `to_dir`. A folder
/// that already exists there is merged into; a file that already exists is skipped, never
/// replaced. Moving copies each file, then removes the original (backed up, like any delete), then
/// removes the folders it emptied. Sends "transfer" events; `cancel_transfer(job)` stops it after
/// the current file.
#[tauri::command(rename_all = "snake_case")]
async fn transfer(
    app: AppHandle,
    state: State<'_, AppState>,
    job: u32,
    from_src: String,
    paths: Vec<String>,
    to_src: String,
    to_dir: String,
    mode: String,
) -> Result<TransferReport, String> {
    use tauri::Emitter;
    let (a, b) = (state.source(Some(&from_src))?, state.source(Some(&to_src))?);
    let moving = mode == "move";
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    state.jobs.lock().unwrap().insert(job, stop.clone());
    let same = from_src == to_src;
    let r = blocking(move || {
        run_transfer(&a, &b, same, job, &paths, &to_dir, moving, &stop, |p| {
            let _ = app.emit("transfer", p);
        })
    })
    .await;
    state.jobs.lock().unwrap().remove(&job);
    r
}

/// How a folder differs from another, by relative path. Files are compared by size only (times
/// aren't kept the same across devices).
#[derive(Default)]
struct Comparison {
    /// What's missing from the target, ready to copy: whole missing folders, with their contents.
    missing: Vec<Item>,
    /// Files on both sides with different sizes, or a file on one side and a folder on the other.
    differ: Vec<String>,
    /// Only in the target.
    extra: Vec<String>,
    same: u32,
}

fn compare_dirs(a: &Device, a_dir: &str, b: &Device, b_dir: &str, rel: &str, hidden: bool, out: &mut Comparison) -> Result<(), String> {
    let shown = |e: &device::Entry| hidden || !e.name.starts_with('.');
    let mine: Vec<_> = a.listdir(a_dir).map_err(msg)?.into_iter().filter(shown).collect();
    let theirs: HashMap<String, device::Entry> = b.listdir(b_dir).map_err(msg)?.into_iter().filter(shown).map(|e| (e.name.clone(), e)).collect();
    let r = |name: &str| if rel.is_empty() { name.to_string() } else { format!("{rel}/{name}") };
    for e in &mine {
        match theirs.get(&e.name) {
            None => walk(a, &e.path, b_dir, &mut out.missing)?,
            Some(t) if t.dir && e.dir => compare_dirs(a, &e.path, b, &t.path, &r(&e.name), hidden, out)?,
            Some(t) if !t.dir && !e.dir && t.size == e.size => out.same += 1,
            Some(_) => out.differ.push(r(&e.name)),
        }
    }
    let names: std::collections::HashSet<&str> = mine.iter().map(|e| e.name.as_str()).collect();
    let mut extra: Vec<_> = theirs.keys().filter(|n| !names.contains(n.as_str())).map(|n| r(n)).collect();
    extra.sort();
    out.extra.extend(extra);
    Ok(())
}

#[derive(Serialize)]
struct CompareReport {
    /// Files and folders to copy (counting everything in missing folders).
    files: u32,
    folders: u32,
    bytes: u64,
    differ: Vec<String>,
    extra: Vec<String>,
    same: u32,
}

fn refuse_nested(same: bool, from_dir: &str, to_dir: &str) -> Result<(), String> {
    let inside = |x: &str, y: &str| x == y || x.starts_with(&format!("{}/", y.trim_end_matches('/')));
    if same && (inside(from_dir, to_dir) || inside(to_dir, from_dir)) {
        return Err("Pick two separate folders: one is inside the other.".into());
    }
    Ok(())
}

/// What `sync` would copy from `from_dir` to `to_dir`, and what differs.
#[tauri::command(rename_all = "snake_case")]
async fn compare(state: State<'_, AppState>, from_src: String, from_dir: String, to_src: String, to_dir: String, hidden: bool) -> Result<CompareReport, String> {
    let (a, b) = (state.source(Some(&from_src))?, state.source(Some(&to_src))?);
    refuse_nested(from_src == to_src, &from_dir, &to_dir)?;
    blocking(move || {
        let mut c = Comparison::default();
        compare_dirs(&a, &from_dir, &b, &to_dir, "", hidden, &mut c)?;
        Ok(CompareReport {
            files: c.missing.iter().filter(|i| !i.dir).count() as u32,
            folders: c.missing.iter().filter(|i| i.dir).count() as u32,
            bytes: c.missing.iter().map(|i| i.size).sum(),
            differ: c.differ,
            extra: c.extra,
            same: c.same,
        })
    })
    .await
}

/// Copies what's missing from `to_dir`, at any depth, from `from_dir`. Never replaces or removes
/// anything in `to_dir`. Sends "transfer" events like `transfer`.
#[tauri::command(rename_all = "snake_case")]
#[allow(clippy::too_many_arguments)]
async fn sync(
    app: AppHandle,
    state: State<'_, AppState>,
    job: u32,
    from_src: String,
    from_dir: String,
    to_src: String,
    to_dir: String,
    hidden: bool,
) -> Result<TransferReport, String> {
    use tauri::Emitter;
    let (a, b) = (state.source(Some(&from_src))?, state.source(Some(&to_src))?);
    refuse_nested(from_src == to_src, &from_dir, &to_dir)?;
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    state.jobs.lock().unwrap().insert(job, stop.clone());
    let r = blocking(move || {
        let mut c = Comparison::default();
        compare_dirs(&a, &from_dir, &b, &to_dir, "", hidden, &mut c)?;
        let report = TransferReport { copied: 0, skipped: vec![], cancelled: false, backups: vec![] };
        run_items(&a, &b, job, &c.missing, false, &stop, |p| {
            let _ = app.emit("transfer", p);
        }, report)
    })
    .await;
    state.jobs.lock().unwrap().remove(&job);
    r
}

#[tauri::command(rename_all = "snake_case")]
fn cancel_transfer(state: State<'_, AppState>, job: u32) {
    if let Some(stop) = state.jobs.lock().unwrap().get(&job) {
        stop.store(true, Ordering::Relaxed);
    }
}

/// Renames a file or folder in its folder. Returns the new path.
#[tauri::command(rename_all = "snake_case")]
async fn rename(state: State<'_, AppState>, src: String, path: String, name: String) -> Result<String, String> {
    let dev = state.source(Some(&src))?;
    blocking(move || dev.rename(&path, &name).map_err(msg)).await
}

/// Free and total space of each storage on a device (each disk, for the Mac).
#[tauri::command(rename_all = "snake_case")]
async fn space(state: State<'_, AppState>, src: String) -> Result<Vec<device::Space>, String> {
    let dev = state.source(Some(&src))?;
    blocking(move || Ok(dev.space())).await
}

/// Opens a file in its default Mac app. Phone files are copied to the app's cache first
/// (`~/Library/Caches/yomi-files/opened`), so changes made there don't go back to the phone.
/// Returns the path that was opened.
/// The file as a path on the Mac: itself for Mac files; device files are copied to the app's cache
/// first (`~/Library/Caches/yomi-files/opened`), so changes made to them don't go back to the device.
fn local_copy(dev: &Device, mac: bool, path: &str) -> Result<std::path::PathBuf, String> {
    if mac {
        return Ok(std::path::PathBuf::from(path));
    }
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (dev.label(), path).hash(&mut h);
    // One folder per device and path, so files with the same name don't overwrite each other.
    let dir = opened_dir().join(format!("{:016x}", h.finish()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("Can't create {}: {e}", dir.display()))?;
    let (data, _) = dev.read(path).map_err(msg)?;
    let local = dir.join(device::basename(path));
    std::fs::write(&local, data).map_err(|e| format!("Can't write {}: {e}", local.display()))?;
    Ok(local)
}

/// Opens a file in its default Mac app (a device file: a copy of it). Returns the path opened.
#[tauri::command(rename_all = "snake_case")]
async fn open_file(state: State<'_, AppState>, src: Option<String>, path: String) -> Result<String, String> {
    let mac = src.as_deref() == Some("mac");
    let dev = state.source(src.as_deref())?;
    blocking(move || {
        let local = local_copy(&dev, mac, &path)?;
        let status = std::process::Command::new("open").arg(&local).status().map_err(|e| format!("Can't open {}: {e}", local.display()))?;
        if !status.success() {
            return Err(format!("No app on this Mac opens {}.", device::basename(&path)));
        }
        Ok(local.display().to_string())
    })
    .await
}

/// Shows a file in Quick Look (`qlmanage -p`), in its own window. A device file is copied first.
#[tauri::command(rename_all = "snake_case")]
async fn quick_look(state: State<'_, AppState>, src: String, path: String) -> Result<(), String> {
    let mac = src == "mac";
    let dev = state.source(Some(&src))?;
    blocking(move || {
        let local = local_copy(&dev, mac, &path)?;
        std::process::Command::new("qlmanage")
            .arg("-p")
            .arg(&local)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("Can't start Quick Look: {e}"))?;
        Ok(())
    })
    .await
}

#[derive(Serialize)]
struct Matches {
    entries: Vec<Entry>,
    /// False when the search stopped early (too many results, or too long).
    complete: bool,
}

/// Files and folders under `dir` whose names contain `query` (any case), nearest first.
/// Stops at 300 results or after 20 seconds. Folders starting with a dot are skipped unless `hidden`.
#[tauri::command(rename_all = "snake_case")]
async fn find(state: State<'_, AppState>, src: String, dir: String, query: String, hidden: bool) -> Result<Matches, String> {
    let dev = state.source(Some(&src))?;
    blocking(move || Ok(find_in(&dev, &dir, &query, hidden, 300, std::time::Duration::from_secs(20)))).await
}

fn find_in(dev: &Device, dir: &str, query: &str, hidden: bool, max: usize, time: std::time::Duration) -> Matches {
    let q = query.trim().to_lowercase();
    let start = std::time::Instant::now();
    let mut out = vec![];
    let mut queue = std::collections::VecDeque::from([dir.to_string()]);
    while let Some(d) = queue.pop_front() {
        if out.len() >= max || start.elapsed() > time {
            return Matches { entries: out, complete: false };
        }
        let Ok(mut items) = dev.listdir(&d) else { continue };
        items.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        for e in items {
            if !hidden && e.name.starts_with('.') {
                continue;
            }
            if e.dir {
                queue.push_back(e.path.clone());
            }
            if e.name.to_lowercase().contains(&q) && out.len() < max {
                out.push(e);
            }
        }
    }
    Matches { entries: out, complete: true }
}

#[derive(Serialize)]
struct Backup {
    /// The backup's path on the Mac.
    file: String,
    /// The file's name when it was backed up.
    name: String,
    size: u64,
    /// When the backup was made (seconds).
    mtime: i64,
    /// Where it came from, for backups made since the app started noting it.
    note: Option<device::BackupNote>,
}

/// The backups, newest first.
#[tauri::command(rename_all = "snake_case")]
async fn backups() -> Result<Vec<Backup>, String> {
    blocking(|| Ok(list_backups(&device::backup_dir()))).await
}

fn list_backups(dir: &std::path::Path) -> Vec<Backup> {
    let notes = device::BackupNote::read_all(dir);
    let mut out: Vec<Backup> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let file = e.file_name().to_string_lossy().into_owned();
            let m = e.metadata().ok().filter(|m| m.is_file())?;
            if file == "index.tsv" || file.starts_with('.') {
                return None;
            }
            let note = notes.get(&file).cloned();
            let name = note.as_ref().map(|n| device::basename(&n.path).to_string()).unwrap_or_else(|| backup_name(&file));
            let mtime = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs() as i64);
            Some(Backup { file: e.path().display().to_string(), name, size: m.len(), mtime, note })
        })
        .collect();
    out.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| b.file.cmp(&a.file)));
    out
}

/// The original name in a backup's file name: "20250101-120000-a.txt" or "20250101-120000-2-a.txt".
fn backup_name(file: &str) -> String {
    let mut rest = file;
    for _ in 0..2 {
        match rest.split_once('-') {
            Some((head, tail)) if head.chars().all(|c| c.is_ascii_digit()) => rest = tail,
            _ => return file.to_string(),
        }
    }
    if let Some((n, tail)) = rest.split_once('-') {
        if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
            return tail.to_string();
        }
    }
    rest.to_string()
}

/// Puts a backup back at `path` on `src`. A file there now is replaced, and backed up first like
/// any other replaced file.
#[tauri::command(rename_all = "snake_case")]
async fn restore(state: State<'_, AppState>, file: String, src: String, path: String) -> Result<WriteResult, String> {
    let dev = state.source(Some(&src))?;
    blocking(move || {
        let dir = device::backup_dir();
        let f = std::path::Path::new(&file);
        if f.parent() != Some(dir.as_path()) {
            return Err("That isn't one of the app's backups.".into());
        }
        let data = std::fs::read(f).map_err(|e| format!("Can't read {file}: {e}"))?;
        let parent = device::dirname(&path);
        if !dev.stat(&parent).map_err(msg)?.is_some_and(|e| e.dir) {
            return Err(format!("The folder {parent} isn't there any more. Restore it into another folder."));
        }
        let now = dev.version(&path).map_err(msg)?;
        dev.write(&path, &data, now.as_deref(), now.is_none()).map_err(msg)
    })
    .await
}

/// Ejects a disk mounted on the Mac (`diskutil eject`).
#[tauri::command(rename_all = "snake_case")]
async fn eject(path: String) -> Result<(), String> {
    blocking(move || {
        if !path.starts_with("/Volumes/") {
            return Err(format!("{path} isn't a disk that can be ejected."));
        }
        let out = std::process::Command::new("diskutil").args(["eject", &path]).output().map_err(|e| format!("Can't run diskutil: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
            Err(format!("Couldn't eject {}: {}. Close files open on it and try again.", device::basename(&path), if why.is_empty() { "the disk is in use".into() } else { why }))
        }
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
    /// "mac" or a device id: where the book is saved back to.
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
    let src = src.unwrap_or_default();
    let src = src.as_str();
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
            devices, connect, disconnect, connections, info, list, refresh, read_text, write_text, mkdir, remove, download,
            upload, backups, restore, eject, quick_look, find, transfer, cancel_transfer, compare, sync, rename, space, open_file, app_folders, open_folder, clear_cache, epub_open, epub_chapter, epub_set_source, epub_set_body, epub_set_meta, epub_cover, epub_find,
            epub_replace, epub_save, epub_export, epub_close
        ])
        .run(tauri::generate_context!())
        .expect("error while running yomi-files");
}

pub fn state(app: &AppHandle) -> tauri::State<'_, AppState> {
    app.state::<AppState>()
}

#[cfg(test)]
mod tests {
    #[test]
    fn transfer_copies_and_moves_folders() {
        use std::fs;
        let root = std::env::temp_dir().join(format!("yomi-transfer-test-{}", std::process::id()));
        let (src, dst) = (root.join("src"), root.join("dst"));
        fs::create_dir_all(src.join("book/inner")).unwrap();
        fs::create_dir_all(dst.join("book")).unwrap();
        fs::write(src.join("book/a.txt"), b"a").unwrap();
        fs::write(src.join("book/inner/b.txt"), b"bb").unwrap();
        fs::write(dst.join("book/a.txt"), b"old").unwrap();
        let mac = crate::Device::new(Box::new(crate::local::Local));
        let s = |p: &std::path::Path| p.display().to_string();
        let stop = std::sync::atomic::AtomicBool::new(false);
        let paths = [s(&src.join("book"))];
        let r = super::run_transfer(&mac, &mac, true, 1, &paths, &s(&dst), false, &stop, |_| {}).unwrap();
        assert_eq!(r.copied, 1);
        assert_eq!(r.skipped.len(), 1, "the existing a.txt is skipped");
        assert_eq!(fs::read(dst.join("book/a.txt")).unwrap(), b"old");
        assert_eq!(fs::read(dst.join("book/inner/b.txt")).unwrap(), b"bb");
        // Into itself is refused.
        let r = super::run_transfer(&mac, &mac, true, 1, &paths, &s(&src.join("book/inner")), false, &stop, |_| {}).unwrap();
        assert_eq!((r.copied, r.skipped.len()), (0, 1));
        // Move to a fresh folder: originals go (backed up), emptied folders too.
        fs::create_dir(root.join("moved")).unwrap();
        let r = super::run_transfer(&mac, &mac, true, 1, &paths, &s(&root.join("moved")), true, &stop, |_| {}).unwrap();
        assert_eq!((r.copied, r.skipped.len(), r.backups.len()), (2, 0, 2));
        assert!(!src.join("book").exists());
        assert_eq!(fs::read(root.join("moved/book/inner/b.txt")).unwrap(), b"bb");
        for b in r.backups {
            fs::remove_file(b).unwrap();
        }
        // Cancelled before starting: nothing happens.
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let r = super::run_transfer(&mac, &mac, true, 1, &[s(&dst.join("book"))], &s(&root.join("moved")), true, &stop, |_| {}).unwrap();
        assert!(r.cancelled && r.copied == 0 && dst.join("book/a.txt").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn sync_copies_only_what_is_missing() {
        use std::fs;
        let root = std::env::temp_dir().join(format!("yomi-sync-test-{}", std::process::id()));
        let (a, b) = (root.join("a"), root.join("b"));
        fs::create_dir_all(a.join("x/y")).unwrap();
        fs::create_dir_all(b.join("x")).unwrap();
        fs::write(a.join("same.txt"), b"1").unwrap();
        fs::write(b.join("same.txt"), b"2").unwrap();
        fs::write(a.join("big.txt"), b"long").unwrap();
        fs::write(b.join("big.txt"), b"s").unwrap();
        fs::write(a.join("x/new.txt"), b"n").unwrap();
        fs::write(a.join("x/y/deep.txt"), b"dd").unwrap();
        fs::write(a.join(".hid"), b"").unwrap();
        fs::write(b.join("x/only-b.txt"), b"").unwrap();
        let mac = crate::Device::new(Box::new(crate::local::Local));
        let s = |p: &std::path::Path| p.display().to_string();
        let mut c = super::Comparison::default();
        super::compare_dirs(&mac, &s(&a), &mac, &s(&b), "", false, &mut c).unwrap();
        let mut missing: Vec<_> = c.missing.iter().map(|i| i.to.strip_prefix(&s(&b)).unwrap().to_string()).collect();
        missing.sort();
        assert_eq!(missing, ["/x/new.txt", "/x/y", "/x/y/deep.txt"]);
        assert_eq!((c.differ, c.extra, c.same), (vec!["big.txt".to_string()], vec!["x/only-b.txt".to_string()], 1));
        let stop = std::sync::atomic::AtomicBool::new(false);
        let report = super::TransferReport { copied: 0, skipped: vec![], cancelled: false, backups: vec![] };
        let r = super::run_items(&mac, &mac, 1, &c.missing, false, &stop, |_| {}, report).unwrap();
        assert_eq!((r.copied, r.skipped.len()), (2, 0));
        assert_eq!(fs::read(b.join("x/y/deep.txt")).unwrap(), b"dd");
        assert_eq!(fs::read(b.join("big.txt")).unwrap(), b"s", "different files are left alone");
        assert!(!b.join(".hid").exists(), "hidden files only when asked");
        assert!(super::refuse_nested(true, &s(&a), &s(&a.join("x"))).is_err());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn find_searches_subfolders_and_rename_refuses_taken_names() {
        use std::fs;
        let root = std::env::temp_dir().join(format!("yomi-find-test-{}", std::process::id()));
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join(".hid")).unwrap();
        for f in ["Book one.epub", "a/b/book two.EPUB", "a/notes.txt", ".hid/book.epub"] {
            fs::write(root.join(f), b"x").unwrap();
        }
        let mac = crate::Device::new(Box::new(crate::local::Local));
        let r = root.display().to_string();
        let names = |hidden| -> Vec<String> {
            super::find_in(&mac, &r, "BOOK", hidden, 100, std::time::Duration::from_secs(5)).entries.into_iter().map(|e| e.name).collect()
        };
        assert_eq!(names(false), ["Book one.epub", "book two.EPUB"]);
        assert_eq!(names(true).len(), 3);
        let f = super::find_in(&mac, &r, "o", true, 2, std::time::Duration::from_secs(5));
        assert!(!f.complete && f.entries.len() == 2);
        // Rename.
        assert!(mac.rename(&format!("{r}/a/notes.txt"), "Book one.epub").is_ok(), "other folder, free");
        assert!(mac.rename(&format!("{r}/a/Book one.epub"), "b").is_err(), "taken by a folder");
        assert!(mac.rename(&format!("{r}/a/Book one.epub"), "x/y").is_err());
        let new = mac.rename(&format!("{r}/a/Book one.epub"), "BOOK ONE.epub").unwrap();
        assert!(new.ends_with("/a/BOOK ONE.epub"), "a change of case is allowed");
        assert!(crate::local::parse_df("Filesystem 1024-blocks Used Available Capacity\n/dev/disk3 100 40 60 40% /\n") == Some((60 * 1024, 100 * 1024)));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn backups_know_where_they_came_from() {
        assert_eq!(super::backup_name("20250101-120000-a-b.txt"), "a-b.txt");
        assert_eq!(super::backup_name("20250101-120000-2-a.txt"), "a.txt");
        assert_eq!(super::backup_name("odd.txt"), "odd.txt");
        let dir = std::env::temp_dir().join(format!("yomi-backup-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mac = crate::Device::new(Box::new(crate::local::Local));
        let f = dir.join("note.txt").display().to_string();
        let v = mac.write(&f, b"one", None, true).unwrap().version;
        let b = mac.write(&f, b"two", Some(&v), false).unwrap().backup.unwrap();
        let listed = super::list_backups(&crate::device::backup_dir());
        let mine = listed.iter().find(|x| x.file == b).expect("listed");
        let note = mine.note.as_ref().expect("noted");
        assert_eq!((note.place.as_str(), note.path.as_str(), note.reason.as_str()), ("mac", f.as_str(), "replaced"));
        assert_eq!(mine.name, "note.txt");
        std::fs::remove_file(&b).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn drives_skip_the_startup_disk() {
        let root = std::fs::canonicalize("/").unwrap();
        for (name, path) in super::drives() {
            assert!(!name.starts_with('.'), "{name}");
            assert_ne!(std::fs::canonicalize(&path).unwrap(), root, "{path}");
        }
    }
}
