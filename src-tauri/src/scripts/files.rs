//! Copying, moving, deleting and making folders for scripts, a file at a time through `Device`.

use super::*;

/// Counts what a script changed.
pub(super) type Changed = Arc<std::sync::atomic::AtomicU32>;

pub(super) fn stopped(stop: &AtomicBool) -> Result<(), String> {
    if stop.load(Ordering::Relaxed) {
        Err("Stopped.".into())
    } else {
        Ok(())
    }
}

/// A file copied on its device through a file on this computer, so it needn't fit in memory.
pub(super) fn copy_file(d: &Device, from: &str, to: &str, stop: &AtomicBool) -> Result<(), String> {
    static N: AtomicU32 = AtomicU32::new(0);
    let tmp = std::env::temp_dir().join(format!(
        "ferry-script-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let r = d
        .read_to(from, &tmp, &mut |_| {}, stop)
        .and_then(|_| d.write_from(to, &tmp, None, true));
    let _ = std::fs::remove_file(&tmp);
    r.map(|_| ()).map_err(|e| e.to_string())
}

/// Copies file or folder `from` to `to` on `d`. A folder's contents go into a folder already at
/// `to`; a file already there isn't replaced (the copy stops, saying so).
pub(super) fn copy_tree(
    d: &Device,
    from: &str,
    to: &str,
    stop: &AtomicBool,
    changed: &Changed,
) -> Result<(), String> {
    stopped(stop)?;
    let e = d
        .stat(from)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("No such file or folder: {from}"))?;
    if e.dir && (to == from || to.starts_with(&format!("{}/", from.trim_end_matches('/')))) {
        return Err(format!("{from} can't be copied into itself."));
    }
    let there = d.stat(to).map_err(|e| e.to_string())?;
    if !e.dir {
        if there.is_some() {
            return Err(format!("{to} is already there. Not replaced."));
        }
        copy_file(d, from, to, stop)?;
        changed.fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }
    match there {
        Some(t) if t.dir => {}
        Some(_) => {
            return Err(format!(
                "A file is already at {to}, where the folder would go."
            ))
        }
        None => {
            d.mkdir(to).map_err(|e| e.to_string())?;
            changed.fetch_add(1, Ordering::Relaxed);
        }
    }
    for c in d.listdir(from).map_err(|e| e.to_string())? {
        copy_tree(d, &c.path, &device::join(to, &c.name), stop, changed)?;
    }
    Ok(())
}

/// Deletes file or folder `path` on `d`, a folder with everything in it: each file backed up,
/// each folder deleted once it's empty.
pub(super) fn remove_tree(
    d: &Device,
    path: &str,
    stop: &AtomicBool,
    changed: &Changed,
) -> Result<(), String> {
    stopped(stop)?;
    let e = d
        .stat(path)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("No such file or folder: {path}"))?;
    if e.dir {
        for c in d.listdir(path).map_err(|e| e.to_string())? {
            remove_tree(d, &c.path, stop, changed)?;
        }
    }
    d.remove(path).map_err(|e| e.to_string())?;
    changed.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// Moves file or folder `from` to `to` on `d`, which must be free: in one step where the device
/// can (on one of the computer's disks), otherwise copied, then the original deleted with
/// backups.
pub(super) fn move_tree(
    d: &Device,
    from: &str,
    to: &str,
    stop: &AtomicBool,
    changed: &Changed,
) -> Result<(), String> {
    if d.stat(to).map_err(|e| e.to_string())?.is_some() {
        return Err(format!("{to} is already there. Nothing was moved."));
    }
    if d.move_to(from, to).map_err(|e| e.to_string())? {
        changed.fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }
    copy_tree(d, from, to, stop, changed)?;
    remove_tree(d, from, stop, changed)
}

/// Makes folder `path`, and with `parents` the folders above it that aren't there; a folder
/// that's there already is fine then.
pub(super) fn make_dirs(
    d: &Device,
    path: &str,
    parents: bool,
    changed: &Changed,
) -> Result<(), String> {
    match d.stat(path).map_err(|e| e.to_string())? {
        Some(e) if e.dir && parents => return Ok(()),
        Some(_) => return Err(format!("{path} is already there.")),
        None => {}
    }
    let up = device::dirname(path);
    if parents && up != path && d.stat(&up).map_err(|e| e.to_string())?.is_none() {
        make_dirs(d, &up, true, changed)?;
    }
    d.mkdir(path).map_err(|e| e.to_string())?;
    changed.fetch_add(1, Ordering::Relaxed);
    Ok(())
}
