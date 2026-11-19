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
    copied: u32,
    /// Name and reason, for each item that wasn't copied (or moved).
    skipped: Vec<(String, String)>,
    cancelled: bool,
    /// Backups of moved files, made when their originals were removed, and of replaced files.
    backups: Vec<String>,
    /// Resuming: files already at the target with the same size, left as they are.
    already: u32,
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
}

/// Lists everything under `from` (itself first), with where it goes under `to_dir`.
pub(super) fn walk(dev: &Device, from: &str, to_dir: &str, out: &mut Vec<Item>) -> Result<(), String> {
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

/// Whether the copy at `path` on `b` holds `data`. adb writes are already checked against an md5
/// of the data on the phone (`Adb::replace`), so they aren't read back again.
fn copy_matches(b: &Device, path: &str, data: &[u8]) -> Result<bool, String> {
    if b.kind() == "adb" {
        return Ok(true);
    }
    let (back, _) = b.read(path).map_err(msg)?;
    Ok(back == data)
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
        if b.stat(&target).map_err(msg)?.is_none() {
            new.push(target);
        }
        walk(a, p, to_dir, &mut items)?;
    }
    let mut report = run_items(a, b, job, &items, opts, stop, emit, report)?;
    report.made = new.into_iter().filter(|t| b.stat(t).ok().flatten().is_some()).collect();
    Ok(report)
}

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
        let done = (|| -> Result<(), String> {
            let (data, _) = a.read(&it.from).map_err(msg)?;
            let w = b.write(&it.to, &data, None, !replace).map_err(|e| match e {
                DevError::Conflict(_) => "Already there.".to_string(),
                e => e.to_string(),
            })?;
            if let Some(bk) = w.backup {
                report.backups.push(bk);
            }
            if verify && !copy_matches(b, &it.to, &data)? {
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
    /// Only in the target.
    extra: Vec<String>,
    same: u32,
}

pub(super) fn compare_dirs(a: &Device, a_dir: &str, b: &Device, b_dir: &str, rel: &str, hidden: bool, out: &mut Comparison) -> Result<(), String> {
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
pub(super) struct CompareReport {
    /// Files and folders to copy (counting everything in missing folders).
    files: u32,
    folders: u32,
    bytes: u64,
    differ: Vec<String>,
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
    let shown = |e: &device::Entry| hidden || !e.name.starts_with('.');
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
) -> Result<TransferReport, String> {
    use tauri::Emitter;
    let (a, b) = (state.source(Some(&from_src))?, state.source(Some(&to_src))?);
    refuse_nested(from_src == to_src, &from_dir, &to_dir)?;
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    state.jobs.lock().unwrap().insert(job, stop.clone());
    let r = blocking(move || {
        let mut c = Comparison::default();
        compare_dirs(&a, &from_dir, &b, &to_dir, "", hidden, &mut c)?;
        let report = TransferReport::default();
        run_items(&a, &b, job, &c.missing, Opts { verify: verify.unwrap_or(false), ..Opts::default() }, &stop, |p| {
            let _ = app.emit("transfer", p);
        }, report)
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
        // Move to a fresh folder: originals go (backed up), emptied folders too.
        fs::create_dir(root.join("moved")).unwrap();
        let r = super::run_transfer(&mac, &mac, true, 1, &paths, &s(&root.join("moved")), super::Opts { moving: true, resume: false, verify: true, replace: false }, &stop, |_| {}).unwrap();
        assert_eq!((r.copied, r.skipped.len(), r.backups.len()), (2, 0, 2));
        assert!(!src.join("book").exists());
        assert_eq!(fs::read(root.join("moved/book/inner/b.txt")).unwrap(), b"bb");
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
