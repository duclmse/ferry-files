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

/// Files that are compressed already, stored in a ZIP as they are: packing them again would take
/// time and save next to nothing.
fn packed_already(name: &str) -> bool {
    const EXTS: &[&str] = &[
        "jpg", "jpeg", "png", "gif", "webp", "heic", "heif", "avif", "mp4", "m4v", "mov", "mkv",
        "webm", "mp3", "m4a", "aac", "ogg", "opus", "flac", "zip", "cbz", "epub", "apk", "jar",
        "7z", "rar", "gz", "xz", "bz2", "zst",
    ];
    name.rsplit_once('.')
        .is_some_and(|(_, x)| EXTS.contains(&x.to_ascii_lowercase().as_str()))
}

/// `secs` since 1970 (UTC) as a ZIP's date, which can only say 1980 to 2107.
pub(super) fn zip_time(secs: i64) -> zip::DateTime {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Days to a date in the Gregorian calendar (Howard Hinnant's civil_from_days).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    zip::DateTime::from_date_and_time(
        u16::try_from(year).unwrap_or(0),
        month as u8,
        day as u8,
        (rem / 3600) as u8,
        (rem % 3600 / 60) as u8,
        (rem % 60) as u8,
    )
    .unwrap_or_default()
}

/// A temporary file on this computer for packing.
fn pack_tmp(what: &str) -> std::path::PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    std::env::temp_dir().join(format!(
        "ferry-pack-{what}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Packs each of `from` (files, and folders with everything in them, each under its own name)
/// into a new ZIP at `to` on `d`. The ZIP is made in a file on this computer, a file at a time,
/// then written to `to`, which must be free. Returns how many files went in.
pub(super) fn pack(
    d: &Device,
    from: &[String],
    to: &str,
    stop: &AtomicBool,
    changed: &Changed,
) -> Result<u32, String> {
    if from.is_empty() {
        return Err("Nothing to pack: give a file or folder.".into());
    }
    if d.stat(to).map_err(|e| e.to_string())?.is_some() {
        return Err(format!("{to} is already there. Not replaced."));
    }
    let mut items = vec![];
    for f in from {
        let e = d
            .stat(f)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("No such file or folder: {f}"))?;
        items.push(e);
    }
    let tmp = pack_tmp("zip");
    let r = (|| {
        let file = std::fs::File::create(&tmp)
            .map_err(|e| format!("Can't make the ZIP on this computer: {e}"))?;
        let mut z = zip::ZipWriter::new(std::io::BufWriter::new(file));
        let mut n = 0;
        for e in &items {
            add_to_zip(&mut z, d, e, &e.name, stop, &mut n)?;
        }
        z.finish()
            .map_err(|e| format!("Can't finish the ZIP: {e}"))?;
        stopped(stop)?;
        d.write_from(to, &tmp, None, true)
            .map_err(|e| e.to_string())?;
        Ok(n)
    })();
    let _ = std::fs::remove_file(&tmp);
    if r.is_ok() {
        changed.fetch_add(1, Ordering::Relaxed);
    }
    r
}

/// Adds `e` to the ZIP as `name`: a folder with what's in it, a file read through a file on this
/// computer, so it needn't fit in memory.
fn add_to_zip<W: std::io::Write + std::io::Seek>(
    z: &mut zip::ZipWriter<W>,
    d: &Device,
    e: &Entry,
    name: &str,
    stop: &AtomicBool,
    n: &mut u32,
) -> Result<(), String> {
    use zip::write::SimpleFileOptions;
    stopped(stop)?;
    let method = if e.dir || packed_already(&e.name) {
        zip::CompressionMethod::Stored
    } else {
        zip::CompressionMethod::Deflated
    };
    let opts = SimpleFileOptions::default()
        .compression_method(method)
        .last_modified_time(zip_time(e.mtime))
        .large_file(e.size >= u64::from(u32::MAX));
    let bad = |e: zip::result::ZipError| format!("Can't add {name} to the ZIP: {e}");
    if e.dir {
        z.add_directory(format!("{name}/"), opts).map_err(bad)?;
        for c in d.listdir(&e.path).map_err(|e| e.to_string())? {
            add_to_zip(z, d, &c, &format!("{name}/{}", c.name), stop, n)?;
        }
        return Ok(());
    }
    let piece = pack_tmp("file");
    let r = (|| {
        d.read_to(&e.path, &piece, &mut |_| {}, stop)
            .map_err(|e| e.to_string())?;
        z.start_file(name, opts).map_err(bad)?;
        let mut f = std::fs::File::open(&piece).map_err(|e| e.to_string())?;
        std::io::copy(&mut f, z).map_err(|e| format!("Can't add {name} to the ZIP: {e}"))?;
        *n += 1;
        Ok(())
    })();
    let _ = std::fs::remove_file(&piece);
    r
}
