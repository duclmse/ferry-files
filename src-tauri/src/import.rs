//! Importing photos and videos from a folder (a phone's DCIM, say) into folders by the date each
//! was taken. Files already there with the same name and size count as imported before.

use super::*;
use crate::transfer::{run_items, Item, Opts, Progress, TransferReport};

/// Photos and videos, by extension.
const MEDIA: [&str; 20] = [
    "jpg", "jpeg", "heic", "heif", "avif", "png", "gif", "webp", "dng", "raw", "cr2", "cr3", "nef", "arw", "mp4", "mov", "m4v", "3gp", "avi", "mkv",
];

fn is_media(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(_, x)| MEDIA.contains(&x.to_ascii_lowercase().as_str()))
}

/// The folder (under the target) for a date: `pattern` is "y/m" (2024/05), "ymd" (2024-05-14),
/// "y/ymd" (2024/2024-05-14) or "none" (all in the target).
pub(super) fn folder_for(pattern: &str, date: Option<(i32, u32, u32)>) -> String {
    let Some((y, m, d)) = date else {
        return if pattern == "none" { String::new() } else { "Undated".into() };
    };
    match pattern {
        "none" => String::new(),
        "ymd" => format!("{y}-{m:02}-{d:02}"),
        "y/ymd" => format!("{y}/{y}-{m:02}-{d:02}"),
        _ => format!("{y}/{m:02}"),
    }
}

/// A file's own date, for one whose contents don't say when it was taken.
fn file_date(mtime: i64) -> Option<(i32, u32, u32)> {
    use chrono::Datelike;
    let d = chrono::DateTime::from_timestamp(mtime, 0).filter(|_| mtime > 0)?.with_timezone(&chrono::Local).date_naive();
    Some((d.year(), d.month(), d.day()))
}

/// Makes `dir` on `b`, and the folders it's in, as needed.
fn make_dirs(b: &Device, dir: &str) -> Result<(), String> {
    if b.stat(dir).map_err(msg)?.is_some() {
        return Ok(());
    }
    make_dirs(b, &device::dirname(dir))?;
    b.mkdir(dir).map_err(msg)
}

/// The work of `import_photos`, apart from the app.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_import(
    a: &Device,
    b: &Device,
    from_dir: &str,
    to_dir: &str,
    pattern: &str,
    hidden: bool,
    job: u32,
    opts: Opts,
    stop: &std::sync::atomic::AtomicBool,
    emit: impl Fn(Progress),
) -> Result<TransferReport, String> {
    let mut files = vec![];
    let mut queue = std::collections::VecDeque::from([from_dir.to_string()]);
    while let Some(d) = queue.pop_front() {
        if stop.load(Ordering::Relaxed) {
            let mut r = TransferReport::default();
            r.cancelled = true;
            return Ok(r);
        }
        let items = a.listdir(&d).map_err(msg)?;
        for e in items {
            if !hidden && e.is_hidden() {
                continue;
            }
            if e.dir {
                queue.push_back(e.path.clone());
            } else if is_media(&e.name) {
                files.push(e);
            }
        }
        emit(Progress::looking(job, format!("finding photos: {}", files.len())));
    }
    files.sort_by(|x, y| x.path.cmp(&y.path));
    let mut items = vec![];
    let mut made: Vec<String> = vec![];
    let mut report = TransferReport::default();
    for (i, e) in files.iter().enumerate() {
        if stop.load(Ordering::Relaxed) {
            report.cancelled = true;
            return Ok(report);
        }
        if i % 20 == 0 {
            emit(Progress::looking(job, format!("reading dates: {} of {}", i, files.len())));
        }
        let date = crate::media::taken(&e.name, &mut |at, len| a.read_range(&e.path, at, len).map_err(msg)).or_else(|| file_date(e.mtime));
        let sub = folder_for(pattern, date);
        let dir = if sub.is_empty() { to_dir.to_string() } else { device::join(to_dir, &sub) };
        if !made.contains(&dir) {
            // A folder that can't be made shows as each of its files skipped.
            let _ = make_dirs(b, &dir);
            made.push(dir.clone());
        }
        items.push(Item::file(e.path.clone(), device::join(&dir, &e.name), e.size));
    }
    // Resuming: what's already there with the same size counts as imported before.
    run_items(a, b, job, &items, Opts { resume: true, replace: false, ..opts }, stop, emit, report)
}

/// Copies (or, with `moving`, moves: originals backed up) the photos and videos under `from_dir`
/// into folders under `to_dir` by the date each was taken (see `folder_for`). Sends "transfer"
/// events like `transfer`; stopped by `cancel_transfer`.
#[tauri::command(rename_all = "snake_case")]
#[allow(clippy::too_many_arguments)]
pub(super) async fn import_photos(
    app: AppHandle,
    state: State<'_, AppState>,
    job: u32,
    from_src: String,
    from_dir: String,
    to_src: String,
    to_dir: String,
    pattern: String,
    hidden: bool,
    moving: bool,
    verify: Option<bool>,
) -> Result<TransferReport, String> {
    use tauri::Emitter;
    let (a, b) = (state.source(Some(&from_src))?, state.source(Some(&to_src))?);
    crate::transfer::refuse_nested(from_src == to_src, &from_dir, &to_dir)?;
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    state.jobs.lock().unwrap().insert(job, stop.clone());
    let r = blocking(move || {
        let opts = Opts { moving, verify: verify.unwrap_or(false), ..Opts::default() };
        run_import(&a, &b, &from_dir, &to_dir, &pattern, hidden, job, opts, &stop, |p| {
            let _ = app.emit("transfer", p);
        })
    })
    .await;
    state.jobs.lock().unwrap().remove(&job);
    r
}

#[cfg(test)]
mod tests {
    #[test]
    fn imports_into_dated_folders() {
        use std::fs;
        let root = std::env::temp_dir().join(format!("ff-import-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (a, b) = (root.join("DCIM"), root.join("Pictures"));
        fs::create_dir_all(a.join("Camera")).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("Camera/one.jpg"), b"not really a jpeg").unwrap();
        fs::write(a.join("Camera/notes.txt"), b"skip").unwrap();
        fs::write(a.join(".hidden.jpg"), b"x").unwrap();
        let s = |p: &std::path::Path| crate::platform::app_path(p);
        let mac = crate::Device::new(Box::new(crate::local::Local));
        let stop = std::sync::atomic::AtomicBool::new(false);
        let run = || super::run_import(&mac, &mac, &s(&a), &s(&b), "ymd", false, 1, super::Opts::default(), &stop, |_| {}).unwrap();
        let r = run();
        assert_eq!((r.copied, r.already), (1, 0));
        // No date inside: the file's own date names the folder.
        let day: Vec<_> = fs::read_dir(&b).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        assert_eq!(day.len(), 1);
        assert_eq!(day[0].len(), 10, "YYYY-MM-DD: {day:?}");
        assert!(b.join(&day[0]).join("one.jpg").exists());
        let r = run();
        assert_eq!((r.copied, r.already), (0, 1), "imported before");
        assert_eq!(super::folder_for("y/m", Some((2024, 5, 4))), "2024/05");
        assert_eq!(super::folder_for("y/ymd", Some((2024, 5, 4))), "2024/2024-05-04");
        assert_eq!(super::folder_for("y/m", None), "Undated");
        let _ = fs::remove_dir_all(&root);
    }
}
