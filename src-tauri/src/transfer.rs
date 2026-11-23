//! Copying and moving between devices (folders too, never replacing a file), comparing two
//! folders and syncing them, with progress and cancel.

use super::*;

/// Copies a file between the phone and the Mac (or within one) into folder `to_dir`, keeping its
/// name. Refuses if a file of that name is already there. Returns the new file's path.
/// Progress of a transfer, sent as the "transfer" event after each file.
#[derive(Clone, Serialize)]
pub(super) struct Progress {
    job: u32,
    files_done: u32,
    files_total: u32,
    bytes_done: u64,
    bytes_total: u64,
    current: String,
}

#[derive(Serialize, Default)]
pub(super) struct TransferReport {
    pub(super) copied: u32,
    /// Name and reason, for each item that wasn't copied (or moved).
    skipped: Vec<(String, String)>,
    pub(super) cancelled: bool,
    /// Backups of moved files, made when their originals were removed, and of replaced files.
    backups: Vec<String>,
    /// Resuming: files already at the target with the same size, left as they are.
    pub(super) already: u32,
    /// Syncing as a mirror: items removed from the target (each file backed up first).
    removed: u32,
    /// Items moved whole in one step, on one of the computer's disks (not counted in `copied`).
    moved: u32,
    /// What the transfer made at the target's top level that wasn't there before (a file, or a
    /// folder with everything in it): what undoing the copy removes, or the move moves back.
    made: Vec<String>,
}

/// One file or folder to make at the target, found by walking the sources.
pub(super) struct Item {
    from: String,
    to: String,
    dir: bool,
    size: u64,
    /// Write over the file at `to` (backed up first), whatever `Opts::replace` says.
    replace: bool,
}

impl Item {
    /// A file to copy from `from` to `to`.
    pub(super) fn file(from: String, to: String, size: u64) -> Item {
        Item { from, to, dir: false, size, replace: false }
    }
}

impl Progress {
    /// Before any file is copied: what's being done meanwhile.
    pub(super) fn looking(job: u32, current: String) -> Progress {
        Progress { job, files_done: 0, files_total: 0, bytes_done: 0, bytes_total: 0, current }
    }
}

/// Lists everything under `from` (itself first), with where it goes under `to_dir`.
pub(super) fn walk(dev: &Device, from: &str, to_dir: &str, out: &mut Vec<Item>) -> Result<(), String> {
    let st = dev.stat(from).map_err(msg)?.ok_or_else(|| format!("No such file or folder: {from}"))?;
    let to = device::join(to_dir, &st.name);
    out.push(Item { from: from.to_string(), to: to.clone(), dir: st.dir, size: st.size, replace: false });
    if st.dir {
        let mut kids = dev.listdir(from).map_err(msg)?;
        kids.sort_by(|a, b| a.name.cmp(&b.name));
        for k in kids {
            walk(dev, &k.path, &to, out)?;
        }
    }
    Ok(())
}

/// How a transfer runs.
#[derive(Clone, Copy, Default)]
pub(super) struct Opts {
    /// Remove each original once its copy is made (and checked, with `verify`).
    pub moving: bool,
    /// Running a stopped job again: files already at the target with the same size count as done.
    pub resume: bool,
    /// Read each copy back and compare it with the original.
    pub verify: bool,
    /// Write over files already at the target (each backed up first) instead of skipping them.
    pub replace: bool,
}

/// What a file copy wrote: the bytes, or (for a big file) a file on this computer holding them.
enum Copied {
    Bytes(Vec<u8>),
    File(Spooled),
}

impl Copied {
    /// Whether the copy at `path` on `b` holds what was written. adb writes are already checked
    /// against an md5 of the data on the phone (`Adb::replace`), so they aren't read back again.
    fn matches(&self, b: &Device, path: &str, stop: &std::sync::atomic::AtomicBool) -> Result<bool, String> {
        if b.kind() == "adb" {
            return Ok(true);
        }
        match self {
            Copied::Bytes(data) => {
                let (back, _) = b.read(path).map_err(msg)?;
                Ok(back == *data)
            }
            Copied::File(from) => {
                let back = Spooled::temp();
                b.read_to(path, &back.path, &mut |_| {}, stop).map_err(msg)?;
                same_files(&from.path, &back.path).map_err(|e| format!("Couldn't compare the copy: {e}"))
            }
        }
    }
}

/// A file's contents on this computer, for copying a big file without holding it in memory: the
/// file itself when it's on this computer, otherwise a copy in the temporary folder, deleted when
/// this is dropped.
struct Spooled {
    path: std::path::PathBuf,
    temp: bool,
}

impl Spooled {
    fn of(a: &Device, path: &str, stop: &std::sync::atomic::AtomicBool, read: &mut dyn FnMut(u64)) -> Result<Spooled, String> {
        if a.kind() == "mac" {
            return Ok(Spooled { path: crate::platform::native(path), temp: false });
        }
        let s = Spooled::temp();
        a.read_to(path, &s.path, read, stop).map_err(msg)?;
        Ok(s)
    }

    fn temp() -> Spooled {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        Spooled { path: std::env::temp_dir().join(format!("ferry-files-copy-{}-{n}", std::process::id())), temp: true }
    }
}

impl Drop for Spooled {
    fn drop(&mut self) {
        if self.temp {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Whether two files on this computer hold the same bytes, read a piece at a time.
fn same_files(a: &std::path::Path, b: &std::path::Path) -> std::io::Result<bool> {
    use std::io::Read;
    let (mut fa, mut fb) = (std::fs::File::open(a)?, std::fs::File::open(b)?);
    if fa.metadata()?.len() != fb.metadata()?.len() {
        return Ok(false);
    }
    let (mut ba, mut bb) = (vec![0u8; 1 << 20], vec![0u8; 1 << 20]);
    loop {
        let n = fa.read(&mut ba)?;
        if n == 0 {
            return Ok(true);
        }
        fb.read_exact(&mut bb[..n])?;
        if ba[..n] != bb[..n] {
            return Ok(false);
        }
    }
}

/// The work of `transfer`, apart from the app, so it can be tested.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_transfer(
    a: &Device,
    b: &Device,
    same: bool,
    job: u32,
    paths: &[String],
    to_dir: &str,
    opts: Opts,
    stop: &std::sync::atomic::AtomicBool,
    emit: impl Fn(Progress),
) -> Result<TransferReport, String> {
    let mut report = TransferReport::default();
    let mut items = vec![];
    let mut new = vec![];
    for p in paths {
        if same && (device::dirname(p) == to_dir || to_dir == p || to_dir.starts_with(&format!("{p}/"))) {
            let why = if device::dirname(p) == to_dir { "It's already in this folder." } else { "A folder can't go inside itself." };
            report.skipped.push((device::basename(p).to_string(), why.into()));
            continue;
        }
        let target = device::join(to_dir, device::basename(p));
        let free = b.stat(&target).map_err(msg)?.is_none();
        if free {
            new.push(target.clone());
        }
        // Moving to a free place on the same disk: the file system moves it whole, at once, as
        // Finder and File Explorer do. Otherwise (another disk, a phone) it's copied, then removed.
        if same && opts.moving && free && !stop.load(Ordering::Relaxed) {
            match a.move_to(p, &target) {
                Ok(true) => {
                    report.moved += 1;
                    continue;
                }
                Ok(false) => {}
                Err(e) => {
                    report.skipped.push((device::basename(p).to_string(), e.to_string()));
                    continue;
                }
            }
        }
        walk(a, p, to_dir, &mut items)?;
    }
    let mut report = run_items(a, b, job, &items, opts, stop, emit, report)?;
    report.made = new.into_iter().filter(|t| b.stat(t).ok().flatten().is_some()).collect();
    Ok(report)
}

/// Files at least this big are copied through a file on this computer instead of memory.
const PIECE: u64 = 8 * 1024 * 1024;

/// Copies (or moves) each item in turn: folders are made (or merged into), files are written
/// only where nothing is there yet. With `resume` (running a stopped or failed job again), a file
/// already at the target with the same size counts as done instead of skipped; when moving, its
/// original is kept, since it isn't known that this copy is the one made from it. With `verify`,
/// a copy that reads back different is removed and reported, and its original is kept.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_items(
    a: &Device,
    b: &Device,
    job: u32,
    items: &[Item],
    opts: Opts,
    stop: &std::sync::atomic::AtomicBool,
    emit: impl Fn(Progress),
    mut report: TransferReport,
) -> Result<TransferReport, String> {
    let Opts { moving, resume, verify, replace } = opts;
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
        let replace = replace || it.replace;
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
        if resume {
            if let Ok(Some(e)) = b.stat(&it.to) {
                // Replacing: a different file there is what's to be replaced, so it goes on.
                if !e.dir && (e.size == it.size || !replace) {
                    if e.size == it.size {
                        report.already += 1;
                    } else {
                        report.skipped.push((name, "A different file with this name is already there.".into()));
                    }
                    // Its original stays, so the folders holding it aren't emptied.
                    emptied.retain(|d| !it.from.starts_with(&format!("{d}/")));
                    p.files_done += 1;
                    p.bytes_done += it.size;
                    continue;
                }
            }
        }
        let mut counted = 0u64;
        let done = (|| -> Result<(), String> {
            let mut moved = |n: u64| {
                counted += n;
                p.bytes_done += n;
                emit(p.clone());
            };
            let conflict = |e: DevError| match e {
                DevError::Conflict(_) => "Already there.".to_string(),
                e => e.to_string(),
            };
            // A big file goes through a file on this computer rather than memory.
            let (w, copied) = if it.size < PIECE {
                let (data, _) = a.read(&it.from).map_err(msg)?;
                moved(data.len() as u64);
                (b.write(&it.to, &data, None, !replace).map_err(conflict)?, Copied::Bytes(data))
            } else {
                let from = Spooled::of(a, &it.from, stop, &mut moved)?;
                (b.write_from(&it.to, &from.path, None, !replace).map_err(conflict)?, Copied::File(from))
            };
            if let Some(bk) = w.backup {
                report.backups.push(bk);
            }
            if verify && !copied.matches(b, &it.to, stop)? {
                if replace {
                    return Err("The copy didn't match the original. The file it replaced is in Backups; try again.".into());
                }
                // Not left looking like a finished copy (a retry would count it as done).
                let _ = b.remove(&it.to);
                return Err("The copy didn't match the original, so it was removed. Try again; if it keeps happening, the cable or the device may be faulty.".into());
            }
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
        p.bytes_done += it.size.saturating_sub(counted);
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
#[allow(clippy::too_many_arguments)]
pub(super) async fn transfer(
    app: AppHandle,
    state: State<'_, AppState>,
    job: u32,
    from_src: String,
    paths: Vec<String>,
    to_src: String,
    to_dir: String,
    mode: String,
    resume: Option<bool>,
    verify: Option<bool>,
) -> Result<TransferReport, String> {
    use tauri::Emitter;
    let (a, b) = (state.source(Some(&from_src))?, state.source(Some(&to_src))?);
    let opts = Opts { moving: mode == "move", resume: resume.unwrap_or(false), verify: verify.unwrap_or(false), replace: mode == "replace" };
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    state.jobs.lock().unwrap().insert(job, stop.clone());
    let same = from_src == to_src;
    let r = blocking(move || {
        run_transfer(&a, &b, same, job, &paths, &to_dir, opts, &stop, |p| {
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
pub(super) struct Comparison {
    /// What's missing from the target, ready to copy: whole missing folders, with their contents.
    missing: Vec<Item>,
    /// Files on both sides with different sizes, or a file on one side and a folder on the other.
    differ: Vec<String>,
    /// The files of `differ` that are files on both sides, ready to replace the target's.
    changed: Vec<Item>,
    /// Only in the target.
    extra: Vec<String>,
    same: u32,
}

pub(super) fn compare_dirs(a: &Device, a_dir: &str, b: &Device, b_dir: &str, rel: &str, hidden: bool, out: &mut Comparison) -> Result<(), String> {
    let shown = |e: &device::Entry| hidden || !e.is_hidden();
    let mine: Vec<_> = a.listdir(a_dir).map_err(msg)?.into_iter().filter(shown).collect();
    let theirs: HashMap<String, device::Entry> = b.listdir(b_dir).map_err(msg)?.into_iter().filter(shown).map(|e| (e.name.clone(), e)).collect();
    let r = |name: &str| if rel.is_empty() { name.to_string() } else { format!("{rel}/{name}") };
    for e in &mine {
        match theirs.get(&e.name) {
            None => walk(a, &e.path, b_dir, &mut out.missing)?,
            Some(t) if t.dir && e.dir => compare_dirs(a, &e.path, b, &t.path, &r(&e.name), hidden, out)?,
            Some(t) if !t.dir && !e.dir && t.size == e.size => out.same += 1,
            Some(t) => {
                if !t.dir && !e.dir {
                    out.changed.push(Item { from: e.path.clone(), to: t.path.clone(), dir: false, size: e.size, replace: true });
                }
                out.differ.push(r(&e.name));
            }
        }
    }
    let names: std::collections::HashSet<&str> = mine.iter().map(|e| e.name.as_str()).collect();
    let mut extra: Vec<_> = theirs.keys().filter(|n| !names.contains(n.as_str())).map(|n| r(n)).collect();
    extra.sort();
    out.extra.extend(extra);
    Ok(())
}

#[derive(Serialize)]
pub(super) struct CompareReport {
    /// Files and folders to copy (counting everything in missing folders).
    files: u32,
    folders: u32,
    bytes: u64,
    differ: Vec<String>,
    /// Of `differ`, the files an update would replace, and their size.
    changed: u32,
    changed_bytes: u64,
    extra: Vec<String>,
    same: u32,
}

/// How each item in one folder stands against the item of the same name in the other: "only"
/// (nothing of that name there), "same", "newer" or "older" (by date modified, when they differ),
/// or "differs" (different but just as new, or a file against a folder).
#[derive(Serialize, Default)]
pub(super) struct Marks {
    a: HashMap<String, &'static str>,
    b: HashMap<String, &'static str>,
}

/// Whether two files of the same size hold the same bytes, read in parts.
fn same_bytes(a: &Device, a_path: &str, b: &Device, b_path: &str, size: u64) -> Result<bool, String> {
    const PART: u64 = 4 << 20;
    let mut at = 0;
    while at < size {
        let len = PART.min(size - at);
        if a.read_range(a_path, at, len).map_err(msg)? != b.read_range(b_path, at, len).map_err(msg)? {
            return Ok(false);
        }
        at += len;
    }
    Ok(true)
}

pub(super) fn mark_dirs(a: &Device, a_dir: &str, b: &Device, b_dir: &str, hidden: bool, content: bool) -> Result<Marks, String> {
    let shown = |e: &device::Entry| hidden || !e.is_hidden();
    let mine: Vec<_> = a.listdir(a_dir).map_err(msg)?.into_iter().filter(shown).collect();
    let theirs: HashMap<String, device::Entry> = b.listdir(b_dir).map_err(msg)?.into_iter().filter(shown).map(|e| (e.name.clone(), e)).collect();
    let mut m = Marks::default();
    for e in &mine {
        let Some(t) = theirs.get(&e.name) else {
            m.a.insert(e.name.clone(), "only");
            continue;
        };
        let by_date = || match e.mtime.cmp(&t.mtime) {
            std::cmp::Ordering::Greater => ("newer", "older"),
            std::cmp::Ordering::Less => ("older", "newer"),
            std::cmp::Ordering::Equal => ("differs", "differs"),
        };
        let (x, y) = match (e.dir, t.dir) {
            (true, true) => {
                let mut c = Comparison::default();
                compare_dirs(a, &e.path, b, &t.path, "", hidden, &mut c)?;
                if c.missing.is_empty() && c.differ.is_empty() && c.extra.is_empty() { ("same", "same") } else { ("differs", "differs") }
            }
            (false, false) if e.size != t.size => by_date(),
            (false, false) if content && !same_bytes(a, &e.path, b, &t.path, e.size)? => by_date(),
            (false, false) => ("same", "same"),
            _ => ("differs", "differs"),
        };
        m.a.insert(e.name.clone(), x);
        m.b.insert(t.name.clone(), y);
    }
    for name in theirs.keys() {
        m.b.entry(name.clone()).or_insert("only");
    }
    Ok(m)
}

/// Marks for the items of two folders against each other (see `Marks`). Files are compared by
/// size, and with `content` by their bytes too; folders by what's in them, by size.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn compare_marks(state: State<'_, AppState>, a_src: String, a_dir: String, b_src: String, b_dir: String, hidden: bool, content: bool) -> Result<Marks, String> {
    let (a, b) = (state.source(Some(&a_src))?, state.source(Some(&b_src))?);
    refuse_nested(a_src == b_src, &a_dir, &b_dir)?;
    blocking(move || mark_dirs(&a, &a_dir, &b, &b_dir, hidden, content)).await
}

pub(super) fn refuse_nested(same: bool, from_dir: &str, to_dir: &str) -> Result<(), String> {
    let inside = |x: &str, y: &str| x == y || x.starts_with(&format!("{}/", y.trim_end_matches('/')));
    if same && (inside(from_dir, to_dir) || inside(to_dir, from_dir)) {
        return Err("Pick two separate folders: one is inside the other.".into());
    }
    Ok(())
}

/// What `sync` would copy from `from_dir` to `to_dir`, and what differs.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn compare(state: State<'_, AppState>, from_src: String, from_dir: String, to_src: String, to_dir: String, hidden: bool) -> Result<CompareReport, String> {
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
            changed: c.changed.len() as u32,
            changed_bytes: c.changed.iter().map(|i| i.size).sum(),
            extra: c.extra,
            same: c.same,
        })
    })
    .await
}

/// How `sync` treats the target.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum SyncMode {
    /// Only copy what's missing.
    Copy,
    /// Also replace files that differ (each backed up first).
    Update,
    /// Also remove what's only in the target (backed up first): the target ends up like the source.
    Mirror,
}

/// The work of `sync`, apart from the app. In `Mirror` mode only the `remove` paths (relative to
/// `to_dir`, as `compare` listed them) that are still only in the target are removed, so nothing
/// goes that wasn't shown first.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_sync(
    a: &Device,
    b: &Device,
    from_dir: &str,
    to_dir: &str,
    hidden: bool,
    mode: SyncMode,
    remove: &[String],
    job: u32,
    opts: Opts,
    stop: &std::sync::atomic::AtomicBool,
    emit: impl Fn(Progress),
) -> Result<TransferReport, String> {
    let mut c = Comparison::default();
    compare_dirs(a, from_dir, b, to_dir, "", hidden, &mut c)?;
    let mut items = c.missing;
    if mode != SyncMode::Copy {
        items.append(&mut c.changed);
    }
    let mut report = run_items(a, b, job, &items, opts, stop, emit, TransferReport::default())?;
    if mode == SyncMode::Mirror && !report.cancelled {
        for rel in remove.iter().filter(|r| c.extra.contains(r)) {
            if stop.load(Ordering::Relaxed) {
                report.cancelled = true;
                break;
            }
            match crate::files::remove_all(b, &device::join(to_dir, rel)) {
                Ok(_) => report.removed += 1,
                Err(e) => report.skipped.push((rel.clone(), format!("Not removed: {e}"))),
            }
        }
    }
    Ok(report)
}

/// Syncs `to_dir` from `from_dir`, at any depth: copies what's missing; with mode "update" also
/// replaces files that differ; with "mirror" also removes the `remove` items found only in the
/// target. Whatever is replaced or removed is backed up first. Sends "transfer" events like
/// `transfer`.
#[tauri::command(rename_all = "snake_case")]
#[allow(clippy::too_many_arguments)]
pub(super) async fn sync(
    app: AppHandle,
    state: State<'_, AppState>,
    job: u32,
    from_src: String,
    from_dir: String,
    to_src: String,
    to_dir: String,
    hidden: bool,
    verify: Option<bool>,
    mode: Option<String>,
    remove: Option<Vec<String>>,
) -> Result<TransferReport, String> {
    use tauri::Emitter;
    let mode = match mode.as_deref() {
        None | Some("copy") => SyncMode::Copy,
        Some("update") => SyncMode::Update,
        Some("mirror") => SyncMode::Mirror,
        Some(m) => return Err(format!("Unknown sync mode: {m}")),
    };
    let (a, b) = (state.source(Some(&from_src))?, state.source(Some(&to_src))?);
    refuse_nested(from_src == to_src, &from_dir, &to_dir)?;
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    state.jobs.lock().unwrap().insert(job, stop.clone());
    let r = blocking(move || {
        let opts = Opts { verify: verify.unwrap_or(false), ..Opts::default() };
        run_sync(&a, &b, &from_dir, &to_dir, hidden, mode, &remove.unwrap_or_default(), job, opts, &stop, |p| {
            let _ = app.emit("transfer", p);
        })
    })
    .await;
    state.jobs.lock().unwrap().remove(&job);
    r
}

#[tauri::command(rename_all = "snake_case")]
pub(super) fn cancel_transfer(state: State<'_, AppState>, job: u32) {
    if let Some(stop) = state.jobs.lock().unwrap().get(&job) {
        stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use crate::device::{Entry, Result, Transport};

    /// The computer's files, as if on another device: kind "test", so copies from it go through
    /// a temporary file, with the default `read_to` and `replace_from`.
    struct Other;

    impl Transport for Other {
        fn kind(&self) -> &'static str { "test" }
        fn label(&self) -> String { "test".into() }
        fn default_dir(&self) -> String { "/".into() }
        fn quick_dirs(&self) -> Vec<(String, String)> { vec![] }
        fn listdir(&self, path: &str) -> Result<Vec<Entry>> { crate::local::Local.listdir(path) }
        fn stat(&self, path: &str) -> Result<Option<Entry>> { crate::local::Local.stat(path) }
        fn read(&self, path: &str) -> Result<(Vec<u8>, String)> { crate::local::Local.read(path) }
        fn read_range(&self, path: &str, start: u64, len: u64) -> Result<Vec<u8>> { crate::local::Local.read_range(path, start, len) }
        fn version(&self, path: &str) -> Result<Option<String>> { crate::local::Local.version(path) }
        fn replace(&self, path: &str, data: &[u8], exists: bool) -> Result<String> { crate::local::Local.replace(path, data, exists) }
        fn delete(&self, path: &str, is_dir: bool) -> Result<()> { crate::local::Local.delete(path, is_dir) }
        fn make_dir(&self, path: &str) -> Result<()> { crate::local::Local.make_dir(path) }
        fn rename(&self, path: &str, new_name: &str) -> Result<()> { crate::local::Local.rename(path, new_name) }
    }

    #[test]
    fn big_files_are_copied_through_a_file() {
        use std::fs;
        let root = std::env::temp_dir().join(format!("ferry-big-copy-test-{}", std::process::id()));
        let (src, dst) = (root.join("src"), root.join("dst"));
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        let big: Vec<u8> = (0..super::PIECE + 12345).map(|i| (i % 251) as u8).collect();
        fs::write(src.join("big.bin"), &big).unwrap();
        let s = |p: &std::path::Path| crate::platform::app_path(p);
        let stop = std::sync::atomic::AtomicBool::new(false);
        let mac = crate::Device::new(Box::new(crate::local::Local));
        let other = crate::Device::new(Box::new(Other));
        let opts = super::Opts { verify: true, ..Default::default() };
        // From the other device (through a temporary file) to this computer, and back as a move.
        let r = super::run_transfer(&other, &mac, false, 1, &[s(&src.join("big.bin"))], &s(&dst), opts, &stop, |_| {}).unwrap();
        assert_eq!((r.copied, r.skipped.len()), (1, 0), "{:?}", r.skipped);
        assert!(fs::read(dst.join("big.bin")).unwrap() == big);
        fs::create_dir(root.join("back")).unwrap();
        let r = super::run_transfer(&mac, &other, false, 1, &[s(&dst.join("big.bin"))], &s(&root.join("back")), super::Opts { moving: true, ..opts }, &stop, |_| {}).unwrap();
        assert_eq!((r.copied, r.skipped.len(), r.backups.len()), (1, 0, 1), "{:?}", r.skipped);
        assert!(fs::read(root.join("back/big.bin")).unwrap() == big);
        assert!(fs::read(&r.backups[0]).unwrap() == big, "the moved original was backed up whole");
        assert!(!dst.join("big.bin").exists());
        // Replacing a big file backs the old one up through a file too.
        let r = super::run_transfer(&other, &mac, false, 1, &[s(&src.join("big.bin"))], &s(&root.join("back")), super::Opts { replace: true, ..opts }, &stop, |_| {}).unwrap();
        assert_eq!((r.copied, r.backups.len()), (1, 1));
        assert!(fs::read(&r.backups[0]).unwrap() == big);
        for b in r.backups {
            fs::remove_file(b).unwrap();
        }
        let left: Vec<_> = fs::read_dir(std::env::temp_dir()).unwrap().flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(&format!("ferry-files-copy-{}-", std::process::id())))
            .collect();
        assert!(left.is_empty(), "temporary copies are deleted");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn transfer_copies_and_moves_folders() {
        use std::fs;
        let root = std::env::temp_dir().join(format!("ferry-transfer-test-{}", std::process::id()));
        let (src, dst) = (root.join("src"), root.join("dst"));
        fs::create_dir_all(src.join("book/inner")).unwrap();
        fs::create_dir_all(dst.join("book")).unwrap();
        fs::write(src.join("book/a.txt"), b"a").unwrap();
        fs::write(src.join("book/inner/b.txt"), b"bb").unwrap();
        fs::write(dst.join("book/a.txt"), b"old").unwrap();
        let mac = crate::Device::new(Box::new(crate::local::Local));
        let s = |p: &std::path::Path| crate::platform::app_path(p);
        let stop = std::sync::atomic::AtomicBool::new(false);
        let paths = [s(&src.join("book"))];
        let r = super::run_transfer(&mac, &mac, true, 1, &paths, &s(&dst), super::Opts { moving: false, resume: false, verify: true, replace: false }, &stop, |_| {}).unwrap();
        assert_eq!(r.copied, 1);
        assert_eq!(r.skipped.len(), 1, "the existing a.txt is skipped");
        assert_eq!(fs::read(dst.join("book/a.txt")).unwrap(), b"old");
        assert_eq!(fs::read(dst.join("book/inner/b.txt")).unwrap(), b"bb");
        assert!(r.made.is_empty(), "book was there before: merged into, not made");
        // A copy into a fresh folder makes book there; undoing it removes all of it.
        fs::create_dir(root.join("copied")).unwrap();
        let r = super::run_transfer(&mac, &mac, true, 1, &paths, &s(&root.join("copied")), super::Opts::default(), &stop, |_| {}).unwrap();
        assert_eq!(r.made, vec![s(&root.join("copied/book"))]);
        assert_eq!(crate::files::remove_all(&mac, &r.made[0]).unwrap(), 2, "a.txt and b.txt backed up");
        assert!(!root.join("copied/book").exists());
        // Resuming: b.txt is there with the same size (done); a.txt differs (skipped, kept).
        let r = super::run_transfer(&mac, &mac, true, 1, &paths, &s(&dst), super::Opts { moving: false, resume: true, verify: true, replace: false }, &stop, |_| {}).unwrap();
        assert_eq!((r.copied, r.already, r.skipped.len()), (0, 1, 1));
        assert!(r.skipped[0].1.contains("different"));
        assert_eq!(fs::read(dst.join("book/a.txt")).unwrap(), b"old");
        // Into itself is refused.
        let r = super::run_transfer(&mac, &mac, true, 1, &paths, &s(&src.join("book/inner")), super::Opts { moving: false, resume: false, verify: true, replace: false }, &stop, |_| {}).unwrap();
        assert_eq!((r.copied, r.skipped.len()), (0, 1));
        // Move to a fresh folder on the same disk: moved whole, nothing copied or backed up.
        fs::create_dir(root.join("moved")).unwrap();
        let r = super::run_transfer(&mac, &mac, true, 1, &paths, &s(&root.join("moved")), super::Opts { moving: true, resume: false, verify: true, replace: false }, &stop, |_| {}).unwrap();
        assert_eq!((r.moved, r.copied, r.skipped.len(), r.backups.len()), (1, 0, 0, 0));
        assert_eq!(r.made, vec![s(&root.join("moved/book"))]);
        assert!(!src.join("book").exists());
        assert_eq!(fs::read(root.join("moved/book/inner/b.txt")).unwrap(), b"bb");
        // Into a folder that has one by that name: merged file by file (a.txt is there, skipped).
        fs::create_dir_all(root.join("merge/book")).unwrap();
        fs::write(root.join("merge/book/a.txt"), b"there").unwrap();
        let r = super::run_transfer(&mac, &mac, true, 1, &[s(&root.join("moved/book"))], &s(&root.join("merge")), super::Opts { moving: true, ..Default::default() }, &stop, |_| {}).unwrap();
        assert_eq!((r.moved, r.copied, r.skipped.len()), (0, 1, 1));
        assert_eq!(fs::read(root.join("merge/book/a.txt")).unwrap(), b"there");
        assert_eq!(fs::read(root.join("moved/book/a.txt")).unwrap(), b"a", "the skipped original stays");
        for b in r.backups {
            fs::remove_file(b).unwrap();
        }
        // Cancelled before starting: nothing happens.
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let r = super::run_transfer(&mac, &mac, true, 1, &[s(&dst.join("book"))], &s(&root.join("moved")), super::Opts { moving: true, ..Default::default() }, &stop, |_| {}).unwrap();
        assert!(r.cancelled && r.copied == 0 && dst.join("book/a.txt").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn sync_copies_only_what_is_missing() {
        use std::fs;
        let root = std::env::temp_dir().join(format!("ferry-sync-test-{}", std::process::id()));
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
        let s = |p: &std::path::Path| crate::platform::app_path(p);
        let mut c = super::Comparison::default();
        super::compare_dirs(&mac, &s(&a), &mac, &s(&b), "", false, &mut c).unwrap();
        let mut missing: Vec<_> = c.missing.iter().map(|i| i.to.strip_prefix(&s(&b)).unwrap().to_string()).collect();
        missing.sort();
        assert_eq!(missing, ["/x/new.txt", "/x/y", "/x/y/deep.txt"]);
        assert_eq!((c.differ, c.extra, c.same), (vec!["big.txt".to_string()], vec!["x/only-b.txt".to_string()], 1));
        let stop = std::sync::atomic::AtomicBool::new(false);
        let report = super::TransferReport::default();
        let r = super::run_items(&mac, &mac, 1, &c.missing, super::Opts::default(), &stop, |_| {}, report).unwrap();
        assert_eq!((r.copied, r.skipped.len()), (2, 0));
        assert_eq!(fs::read(b.join("x/y/deep.txt")).unwrap(), b"dd");
        assert_eq!(fs::read(b.join("big.txt")).unwrap(), b"s", "different files are left alone");
        assert!(!b.join(".hid").exists(), "hidden files only when asked");
        assert!(super::refuse_nested(true, &s(&a), &s(&a.join("x"))).is_err());

        // Update replaces the file that differs; Mirror also removes what was listed.
        fs::write(b.join("x/also-b.txt"), b"").unwrap();
        let r = super::run_sync(&mac, &mac, &s(&a), &s(&b), false, super::SyncMode::Update, &[], 1, super::Opts::default(), &stop, |_| {}).unwrap();
        assert_eq!((r.copied, r.backups.len(), r.removed), (1, 1, 0));
        assert_eq!(fs::read(b.join("big.txt")).unwrap(), b"long");
        assert!(b.join("x/only-b.txt").exists());
        let listed = ["x/only-b.txt".to_string()];
        let r = super::run_sync(&mac, &mac, &s(&a), &s(&b), false, super::SyncMode::Mirror, &listed, 1, super::Opts::default(), &stop, |_| {}).unwrap();
        assert_eq!((r.copied, r.removed), (0, 1));
        assert!(!b.join("x/only-b.txt").exists());
        assert!(b.join("x/also-b.txt").exists(), "only what was listed goes");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn marks_and_replace() {
        let root = std::env::temp_dir().join(format!("ff-marks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (a, b) = (root.join("a"), root.join("b"));
        for d in [&a, &b] {
            std::fs::create_dir_all(d.join("dir")).unwrap();
            std::fs::write(d.join("dir/x.txt"), "x").unwrap();
            std::fs::write(d.join("same.txt"), "same").unwrap();
            std::fs::write(d.join("tie.txt"), if d == &a { "aaaa" } else { "bbbb" }).unwrap();
        }
        std::fs::write(a.join("only.txt"), "only").unwrap();
        std::fs::write(b.join("theirs.txt"), "t").unwrap();
        std::fs::write(b.join("grew.txt"), "old").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(a.join("grew.txt"), "newer").unwrap();
        let s = |p: &std::path::Path| crate::platform::app_path(p);
        let mac = crate::Device::new(Box::new(crate::local::Local));
        let m = super::mark_dirs(&mac, &s(&a), &mac, &s(&b), false, false).unwrap();
        assert_eq!(m.a["only.txt"], "only");
        assert_eq!(m.b["theirs.txt"], "only");
        assert_eq!((m.a["same.txt"], m.a["dir"], m.a["tie.txt"]), ("same", "same", "same"), "by size");
        assert_eq!((m.a["grew.txt"], m.b["grew.txt"]), ("newer", "older"));
        let m = super::mark_dirs(&mac, &s(&a), &mac, &s(&b), false, true).unwrap();
        assert_ne!(m.a["tie.txt"], "same", "by content");
        assert_eq!(m.a["same.txt"], "same");

        let stop = std::sync::atomic::AtomicBool::new(false);
        let paths = [s(&a.join("grew.txt"))];
        let opts = super::Opts { replace: true, verify: true, ..Default::default() };
        let r = super::run_transfer(&mac, &mac, true, 1, &paths, &s(&b), opts, &stop, |_| {}).unwrap();
        assert_eq!((r.copied, r.backups.len(), r.made.len()), (1, 1, 0));
        assert_eq!(std::fs::read_to_string(b.join("grew.txt")).unwrap(), "newer");
        let _ = std::fs::remove_dir_all(&root);
    }
}
