//! The catch-up walk on Windows, reading each folder's times without opening
//! it.
//!
//! A folder's times in its parent's listing are the copy NTFS keeps in the
//! parent's index, which it does not refresh when the folder's contents
//! change. Asking the folder itself (`GetFileAttributesExW` or
//! `symlink_metadata`) opens it: about 155 µs each, 2.5 s of a 7.5 s scan of
//! C:\Windows (167k folders). The listing of a folder starts with its own `.`
//! entry, which carries the current times, so this walk reads them there.
//! It lists folders itself because `ignore` and `std::fs::read_dir` drop
//! `.`; on C:\Windows it takes 2.7 s against 5.0 s for the `ignore` walk.
//!
//! Unlike the `ignore` walk, `.ignore` files are not honoured here.

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

use windows::core::PCWSTR;
use windows::Win32::Foundation::FILETIME;
use windows::Win32::Storage::FileSystem::{
    FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FIND_FIRST_EX_LARGE_FETCH,
    WIN32_FIND_DATAW,
};

use super::{dir_newest_time, is_hidden, newest_time, MAX_REPLAY_PATHS};

const DOT: &[u16] = &[b'.' as u16];
const DOT_DOT: &[u16] = &[b'.' as u16, b'.' as u16];

/// Reparse tags with this bit are links (symbolic links, junctions), which
/// the walk does not follow. Other reparse points, such as cloud-file
/// placeholders, are ordinary folders to it, as they are to std.
const NAME_SURROGATE: u32 = 0x2000_0000;

/// Past this many UTF-16 units a path needs the `\\?\` form; `CreateDirectory`
/// documents MAX_PATH less 12 for folders, and std switches at the same
/// point for its own calls.
const SHORT_PATH_MAX: usize = 248;

struct Dir {
    path: PathBuf,
    /// The times from the parent's listing, used when the folder's own
    /// cannot be read.
    listed: Option<i64>,
}

/// Folders waiting to be listed, and how many are being listed now: the
/// walk is done when both are empty.
struct Queue {
    state: Mutex<(Vec<Dir>, usize)>,
    ready: Condvar,
}

impl Queue {
    fn next(&self) -> Option<Dir> {
        let mut state = self.state.lock().ok()?;
        loop {
            if let Some(dir) = state.0.pop() {
                state.1 += 1;
                return Some(dir);
            }
            if state.1 == 0 {
                return None;
            }
            state = self.ready.wait(state).ok()?;
        }
    }

    fn finish(&self, subdirs: Vec<Dir>) {
        if let Ok(mut state) = self.state.lock() {
            state.0.extend(subdirs);
            state.1 -= 1;
        }
        self.ready.notify_all();
    }
}

/// As the portable `changed_since`: paths under `roots`, not excluded by
/// `skip`, whose write or creation time is at or after `threshold`, with
/// whether each is a folder. `None` past `MAX_REPLAY_PATHS`.
pub(super) fn changed_since(
    roots: &[PathBuf],
    threshold: i64,
    skip: &(dyn Fn(&Path, bool) -> bool + Sync),
) -> Option<Vec<(PathBuf, bool)>> {
    let mut found = Vec::new();
    let mut start = Vec::new();
    for root in roots {
        if skip(root, is_hidden(root)) {
            continue;
        }
        match std::fs::metadata(root) {
            Ok(meta) if meta.is_dir() => start.push(Dir {
                path: root.clone(),
                listed: None,
            }),
            Ok(meta) if newest_time(&meta) >= threshold => found.push((root.clone(), false)),
            _ => {}
        }
    }
    let queue = Queue {
        state: Mutex::new((start, 0)),
        ready: Condvar::new(),
    };
    let count = AtomicUsize::new(found.len());
    let overflow = AtomicBool::new(false);
    let threads = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    let parts = std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads)
            .map(|_| s.spawn(|| worker(&queue, threshold, skip, &count, &overflow)))
            .collect();
        workers
            .into_iter()
            .map(|w| w.join())
            .collect::<Result<Vec<_>, _>>()
    })
    .ok()?;
    if overflow.load(Ordering::Relaxed) {
        return None;
    }
    found.extend(parts.into_iter().flatten());
    // Parents first, so a new directory's full walk covers its new children.
    found.sort();
    Some(found)
}

fn worker(
    queue: &Queue,
    threshold: i64,
    skip: &(dyn Fn(&Path, bool) -> bool + Sync),
    count: &AtomicUsize,
    overflow: &AtomicBool,
) -> Vec<(PathBuf, bool)> {
    let mut found = Vec::new();
    while let Some(dir) = queue.next() {
        let mut subdirs = Vec::new();
        if !overflow.load(Ordering::Relaxed) {
            let before = found.len();
            list(&dir, threshold, skip, &mut found, &mut subdirs);
            let added = found.len() - before;
            if count.fetch_add(added, Ordering::Relaxed) + added > MAX_REPLAY_PATHS {
                overflow.store(true, Ordering::Relaxed);
                subdirs.clear();
            }
        }
        queue.finish(subdirs);
    }
    found
}

fn list(
    dir: &Dir,
    threshold: i64,
    skip: &(dyn Fn(&Path, bool) -> bool + Sync),
    found: &mut Vec<(PathBuf, bool)>,
    subdirs: &mut Vec<Dir>,
) {
    let pattern = search_pattern(&dir.path);
    let mut data = WIN32_FIND_DATAW::default();
    // SAFETY: `pattern` is NUL-terminated and outlives the call, and `data`
    // is the struct FindExInfoBasic fills.
    let handle = unsafe {
        FindFirstFileExW(
            PCWSTR(pattern.as_ptr()),
            FindExInfoBasic,
            std::ptr::from_mut(&mut data).cast(),
            FindExSearchNameMatch,
            None,
            FIND_FIRST_EX_LARGE_FETCH,
        )
    };
    let mut own = None;
    if let Ok(handle) = handle {
        loop {
            let len = data
                .cFileName
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(data.cFileName.len());
            let name = &data.cFileName[..len];
            let times = find_data_time(&data);
            if name == DOT {
                own = Some(times);
            } else if name != DOT_DOT {
                let path = dir.path.join(OsString::from_wide(name));
                let attrs = data.dwFileAttributes;
                let link = attrs & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
                    && data.dwReserved0 & NAME_SURROGATE != 0;
                let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY.0 != 0 && !link;
                if !skip(&path, is_hidden(&path)) {
                    if is_dir {
                        subdirs.push(Dir {
                            path,
                            listed: Some(times),
                        });
                    } else if times >= threshold {
                        found.push((path, false));
                    }
                }
            }
            // SAFETY: `handle` is the open search and `data` the struct it
            // fills.
            if unsafe { FindNextFileW(handle, &mut data) }.is_err() {
                break;
            }
        }
        // SAFETY: `handle` came from FindFirstFileExW and is closed once.
        unsafe {
            let _ = FindClose(handle);
        }
    }
    // A drive root has no `.` entry, and a folder that cannot be listed has
    // none to read.
    let newest = own.or_else(|| dir_newest_time(&dir.path)).or(dir.listed);
    if newest.is_some_and(|t| t >= threshold) {
        found.push((dir.path.clone(), true));
    }
}

fn find_data_time(data: &WIN32_FIND_DATAW) -> i64 {
    let secs = |t: FILETIME| {
        fs_protocol::efu::filetime_to_unix(
            (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime),
        )
    };
    secs(data.ftLastWriteTime).max(secs(data.ftCreationTime))
}

/// `dir\*`, NUL-terminated, in the `\\?\` form when it is too long for the
/// plain one.
fn search_pattern(dir: &Path) -> Vec<u16> {
    let pattern = dir.join("*");
    let mut wide: Vec<u16> = pattern.as_os_str().encode_wide().collect();
    if wide.len() >= SHORT_PATH_MAX && pattern.is_absolute() {
        let slash = u16::from(b'/');
        let backslash = u16::from(b'\\');
        for c in &mut wide {
            if *c == slash {
                *c = backslash;
            }
        }
        let prefix: &[u16] = &[backslash, backslash, u16::from(b'?'), backslash];
        if !wide.starts_with(prefix) {
            let unc = wide.starts_with(&[backslash, backslash]);
            let mut long: Vec<u16> = prefix.to_vec();
            if unc {
                long.extend("UNC".encode_utf16());
                long.extend_from_slice(&wide[1..]);
            } else {
                long.extend_from_slice(&wide);
            }
            wide = long;
        }
    }
    wide.push(0);
    wide
}

#[cfg(test)]
mod tests {
    use std::os::windows::fs::{FileTimesExt, OpenOptionsExt};
    use std::time::{Duration, SystemTime};

    use super::*;

    fn none(_: &Path, _: bool) -> bool {
        false
    }

    fn unix_now() -> i64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64)
    }

    // The case the parent's listing gets wrong: adding a file to an old
    // folder changes the folder's own time, not the copy in its parent.
    #[test]
    fn a_folder_with_a_new_child_is_found_by_its_own_time() {
        let dir = tempfile::TempDir::new().unwrap();
        let old = dir.path().join("old");
        std::fs::create_dir(&old).unwrap();
        let past = SystemTime::now() - Duration::from_secs(3600);
        let times = std::fs::FileTimes::new()
            .set_modified(past)
            .set_created(past);
        // FILE_FLAG_BACKUP_SEMANTICS: a directory opens only with it.
        std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(0x0200_0000)
            .open(&old)
            .and_then(|f| f.set_times(times))
            .unwrap();
        let threshold = unix_now() - 2;
        let before = changed_since(&[dir.path().to_path_buf()], threshold, &none).unwrap();
        assert!(!before.contains(&(old.clone(), true)), "{before:?}");
        std::fs::write(old.join("new.txt"), b"x").unwrap();

        let found = changed_since(&[dir.path().to_path_buf()], threshold, &none).unwrap();
        assert!(found.contains(&(old.clone(), true)), "{found:?}");
        assert!(found.contains(&(old.join("new.txt"), false)), "{found:?}");
    }

    #[test]
    fn everything_under_a_root_is_listed_once_parents_first() {
        let dir = tempfile::TempDir::new().unwrap();
        let a = dir.path().join("a");
        std::fs::create_dir_all(a.join("b")).unwrap();
        std::fs::write(a.join("b").join("f.txt"), b"x").unwrap();
        std::fs::write(dir.path().join(".hidden"), b"x").unwrap();

        let found = changed_since(&[dir.path().to_path_buf()], i64::MIN, &none).unwrap();
        let paths: Vec<&Path> = found.iter().map(|(p, _)| p.as_path()).collect();
        assert_eq!(
            paths,
            [
                dir.path(),
                &dir.path().join(".hidden"),
                &a,
                &a.join("b"),
                &a.join("b").join("f.txt"),
            ]
        );
    }

    #[test]
    fn skipped_folders_are_neither_listed_nor_reported() {
        let dir = tempfile::TempDir::new().unwrap();
        let skipped = dir.path().join("skipped");
        std::fs::create_dir(&skipped).unwrap();
        std::fs::write(skipped.join("f.txt"), b"x").unwrap();
        let skip = |p: &Path, _: bool| p.ends_with("skipped");

        let found = changed_since(&[dir.path().to_path_buf()], i64::MIN, &skip).unwrap();
        assert!(
            found.iter().all(|(p, _)| !p.starts_with(&skipped)),
            "{found:?}"
        );
    }

    #[test]
    fn a_path_past_max_path_is_listed() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut deep = dir.path().to_path_buf();
        for _ in 0..12 {
            deep.push("a-folder-name-of-twenty-five");
        }
        let long = PathBuf::from(format!(r"\\?\{}", deep.display()));
        std::fs::create_dir_all(&long).unwrap();
        std::fs::write(long.join("f.txt"), b"x").unwrap();
        assert!(deep.as_os_str().len() > 260);

        let found = changed_since(&[dir.path().to_path_buf()], i64::MIN, &none).unwrap();
        assert!(found.contains(&(deep.join("f.txt"), false)), "{found:?}");
    }
}
