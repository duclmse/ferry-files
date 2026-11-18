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

#[derive(Serialize)]
pub(super) struct TransferReport {
    copied: u32,
    /// Name and reason, for each item that wasn't copied (or moved).
    skipped: Vec<(String, String)>,
    cancelled: bool,
    /// Backups of moved files, made when their originals were removed.
    backups: Vec<String>,
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

/// The work of `transfer`, apart from the app, so it can be tested.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_transfer(
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
pub(super) fn run_items(
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
pub(super) async fn transfer(
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
        let report = super::TransferReport { copied: 0, skipped: vec![], cancelled: false, backups: vec![] };
        let r = super::run_items(&mac, &mac, 1, &c.missing, false, &stop, |_| {}, report).unwrap();
        assert_eq!((r.copied, r.skipped.len()), (2, 0));
        assert_eq!(fs::read(b.join("x/y/deep.txt")).unwrap(), b"dd");
        assert_eq!(fs::read(b.join("big.txt")).unwrap(), b"s", "different files are left alone");
        assert!(!b.join(".hid").exists(), "hidden files only when asked");
        assert!(super::refuse_nested(true, &s(&a), &s(&a.join("x"))).is_err());
        fs::remove_dir_all(&root).unwrap();
    }
}
