//! Finding duplicate files across folders on any devices: files of the same size, then the same
//! first 64 KiB, then the same whole content (by md5). Only files still tied are read further.

use super::*;

/// A file in a group of duplicates; `at` is the index of the folder it was found under.
#[derive(Clone, Debug, Serialize)]
pub(super) struct Dupe {
    at: usize,
    path: String,
    name: String,
    size: u64,
    mtime: i64,
}

#[derive(Serialize, Default)]
pub(super) struct Dupes {
    /// Groups of files with the same content, biggest files first.
    groups: Vec<Vec<Dupe>>,
    /// Files looked at.
    files: u32,
    /// False when stopped, or the limits cut the search short.
    complete: bool,
}

/// How far along the search is, sent as the "dupes" event.
#[derive(Clone, Serialize)]
struct DupeProgress {
    job: u32,
    text: String,
}

/// Most files looked at in one search.
const MAX_FILES: usize = 200_000;
/// Bytes read for the first comparison.
const HEAD: u64 = 64 << 10;

/// md5 of `len` bytes of `path` from the start, read in parts.
fn digest(dev: &Device, path: &str, len: u64, stop: &std::sync::atomic::AtomicBool) -> Option<[u8; 16]> {
    const PART: u64 = 4 << 20;
    let mut ctx = md5::Context::new();
    let mut at = 0;
    while at < len {
        if stop.load(Ordering::Relaxed) {
            return None;
        }
        let n = PART.min(len - at);
        let data = dev.read_range(path, at, n).ok()?;
        if data.is_empty() {
            break;
        }
        at += data.len() as u64;
        ctx.consume(&data);
    }
    Some(ctx.compute().0)
}

/// Splits each group by `key` (files it can't read drop out), keeping groups of two or more.
fn split<K: std::hash::Hash + Eq>(groups: Vec<Vec<(usize, Entry)>>, mut key: impl FnMut(&(usize, Entry)) -> Option<K>) -> Vec<Vec<(usize, Entry)>> {
    let mut out = vec![];
    for g in groups {
        let mut by: HashMap<K, Vec<(usize, Entry)>> = HashMap::new();
        for f in g {
            if let Some(k) = key(&f) {
                by.entry(k).or_default().push(f);
            }
        }
        out.extend(by.into_values().filter(|g| g.len() > 1));
    }
    out
}

pub(super) fn find_dupes(
    devs: &[(Arc<Device>, String)],
    hidden: bool,
    min_size: u64,
    stop: &std::sync::atomic::AtomicBool,
    emit: impl Fn(String),
) -> Dupes {
    let mut report = Dupes { complete: true, ..Default::default() };
    // Every file, by size.
    let mut by_size: HashMap<u64, Vec<(usize, Entry)>> = HashMap::new();
    let mut seen = std::collections::HashSet::new();
    let mut told = 0;
    'walk: for (at, (dev, dir)) in devs.iter().enumerate() {
        let mut queue = std::collections::VecDeque::from([dir.clone()]);
        while let Some(d) = queue.pop_front() {
            if stop.load(Ordering::Relaxed) || report.files as usize >= MAX_FILES {
                report.complete = false;
                break 'walk;
            }
            let Ok(items) = dev.listdir(&d) else { continue };
            for e in items {
                if !hidden && e.name.starts_with('.') {
                    continue;
                }
                if e.dir {
                    queue.push_back(e.path.clone());
                } else if e.size >= min_size.max(1) && seen.insert((dev.label(), e.path.clone())) {
                    report.files += 1;
                    by_size.entry(e.size).or_default().push((at, e));
                }
            }
            if report.files >= told + 500 {
                told = report.files;
                emit(format!("Looked at {} files…", report.files));
            }
        }
    }
    let groups: Vec<_> = by_size.into_values().filter(|g| g.len() > 1).collect();
    let n: usize = groups.iter().map(Vec::len).sum();
    let mut done = 0;
    let groups = split(groups, |(at, e)| {
        done += 1;
        if done % 50 == 0 {
            emit(format!("Comparing the starts of {n} files of the same size: {done}…"));
        }
        digest(&devs[*at].0, &e.path, e.size.min(HEAD), stop)
    });
    let n: usize = groups.iter().filter(|g| g[0].1.size > HEAD).map(Vec::len).sum();
    let mut done = 0;
    let mut groups = split(groups, |(at, e)| {
        if e.size <= HEAD {
            return Some([0; 16]);
        }
        done += 1;
        emit(format!("Comparing {n} files whole: {done}…"));
        digest(&devs[*at].0, &e.path, e.size, stop)
    });
    if stop.load(Ordering::Relaxed) {
        report.complete = false;
    }
    groups.sort_by_key(|g| std::cmp::Reverse(g[0].1.size));
    report.groups = groups
        .into_iter()
        .map(|mut g| {
            g.sort_by(|a, b| (a.0, &a.1.path).cmp(&(b.0, &b.1.path)));
            g.into_iter().map(|(at, e)| Dupe { at, path: e.path, name: e.name, size: e.size, mtime: e.mtime }).collect()
        })
        .collect();
    report
}

/// Finds files with the same content under the `folders` (source, folder) given. Stopped by
/// `cancel_transfer` with the same `job`; progress goes out as the "dupes" event.
#[tauri::command(rename_all = "snake_case")]
pub(super) async fn duplicates(app: AppHandle, state: State<'_, AppState>, job: u32, folders: Vec<(String, String)>, hidden: bool, min_size: u64) -> Result<Dupes, String> {
    use tauri::Emitter;
    let mut devs = vec![];
    for (src, dir) in folders {
        devs.push((state.source(Some(&src))?, dir));
    }
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    state.jobs.lock().unwrap().insert(job, stop.clone());
    let s = stop.clone();
    let r = blocking(move || {
        Ok(find_dupes(&devs, hidden, min_size, &s, |text| {
            let _ = app.emit("dupes", DupeProgress { job, text });
        }))
    })
    .await;
    state.jobs.lock().unwrap().remove(&job);
    r
}

#[cfg(test)]
mod tests {
    #[test]
    fn finds_duplicates() {
        let root = std::env::temp_dir().join(format!("ff-dupes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a/sub")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let mut other = big.clone();
        *other.last_mut().unwrap() ^= 1;
        std::fs::write(root.join("a/one.bin"), &big).unwrap();
        std::fs::write(root.join("a/sub/two.bin"), &big).unwrap();
        std::fs::write(root.join("b/three.bin"), &big).unwrap();
        std::fs::write(root.join("b/near.bin"), &other).unwrap();
        std::fs::write(root.join("a/x.txt"), "same").unwrap();
        std::fs::write(root.join("b/y.txt"), "same").unwrap();
        std::fs::write(root.join("b/z.txt"), "diff").unwrap();
        std::fs::write(root.join("b/empty1"), "").unwrap();
        std::fs::write(root.join("b/empty2"), "").unwrap();
        let s = |p: std::path::PathBuf| crate::platform::app_path(&p);
        let mac = || std::sync::Arc::new(crate::Device::new(Box::new(crate::local::Local)));
        let devs = [(mac(), s(root.join("a"))), (mac(), s(root.join("b")))];
        let stop = std::sync::atomic::AtomicBool::new(false);
        let r = super::find_dupes(&devs, false, 0, &stop, |_| {});
        assert!(r.complete);
        let names: Vec<Vec<&str>> = r.groups.iter().map(|g| g.iter().map(|d| d.name.as_str()).collect()).collect();
        assert_eq!(names, vec![vec!["one.bin", "two.bin", "three.bin"], vec!["x.txt", "y.txt"]]);
        assert_eq!(r.groups[0][2].at, 1);
        let _ = std::fs::remove_dir_all(&root);
    }
}
