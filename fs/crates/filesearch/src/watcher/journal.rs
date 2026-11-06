//! Catch-up for changes made while the daemon was stopped
//! (docs/plans/everything-borrowing.md, phase 5).
//!
//! The live watcher only sees changes from the moment it starts. Like
//! Everything's USN journal replay, a position in the OS change log is stored
//! with the index and the log is replayed from it on the next start. Only
//! macOS (FSEvents history) is implemented; elsewhere `current_position`
//! returns `None` and the daemon keeps its previous behaviour.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::indexer::crawler::IndexEvent;
use crate::store::is_under;

/// Store metadata key holding the serialized [`Position`].
pub const META_KEY: &str = "journal_position";

/// Distinct changed paths replayed as individual events. Each event does its
/// own stat and sibling sweep, so past this a reconcile crawl is cheaper.
const MAX_REPLAY_PATHS: usize = 200_000;

// FSEvents flag bits (CoreServices/FSEvents.h). Kept here rather than taken
// from `fsevent-sys` so `plan` and its tests build on every platform.
const MUST_SCAN_SUBDIRS: u32 = 0x0000_0001;
const USER_DROPPED: u32 = 0x0000_0002;
const KERNEL_DROPPED: u32 = 0x0000_0004;
const EVENT_IDS_WRAPPED: u32 = 0x0000_0008;
const ROOT_CHANGED: u32 = 0x0000_0020;
const ITEM_IS_DIR: u32 = 0x0002_0000;

/// A point in the OS change log up to which the index is known to be current.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub event_id: u64,
    /// `(watch root, change-log identity of its volume)`. A different list
    /// means the roots changed or the volume's history was reset; either way
    /// replay cannot be trusted.
    pub roots: Vec<(String, String)>,
}

impl Position {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn from_json(s: &str) -> Option<Self> {
        serde_json::from_str(s).ok()
    }
}

/// What the daemon should do on a warm start.
#[derive(Debug)]
pub enum CatchUp {
    UpToDate,
    Events(Vec<IndexEvent>),
    FullCrawl(String),
}

/// Current change-log position for `roots`, or `None` when this platform or
/// one of the volumes has no replayable history.
pub fn current_position(roots: &[PathBuf]) -> Option<Position> {
    #[cfg(target_os = "macos")]
    {
        fsevents::current_position(roots)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = roots;
        None
    }
}

/// Replays the change log from `stored` and turns it into index events.
/// Blocking; run it on a blocking thread.
pub fn catch_up(
    stored:      &Position,
    now:         &Position,
    formats:     &[String],
    paths_under: impl Fn(&str) -> Vec<String>,
) -> CatchUp {
    if stored.roots != now.roots {
        return CatchUp::FullCrawl("watch roots or volume history changed".into());
    }
    if stored.event_id >= now.event_id {
        return CatchUp::UpToDate;
    }
    let roots: Vec<PathBuf> = stored.roots.iter().map(|(r, _)| PathBuf::from(r)).collect();
    #[cfg(target_os = "macos")]
    let changed = fsevents::replay(&roots, stored.event_id);
    #[cfg(not(target_os = "macos"))]
    let changed: Result<HashMap<PathBuf, u32>, String> = Err("no change log on this platform".into());
    match changed {
        Ok(changed) => plan(changed, &roots, formats, paths_under),
        Err(reason) => CatchUp::FullCrawl(reason),
    }
}

/// Turns replayed `path -> OR of event flags` into index events. The flags
/// say *that* a path changed; its current state is read from disk, so the
/// order of the original events does not matter.
fn plan(
    changed:     HashMap<PathBuf, u32>,
    roots:       &[PathBuf],
    formats:     &[String],
    paths_under: impl Fn(&str) -> Vec<String>,
) -> CatchUp {
    if changed.is_empty() {
        return CatchUp::UpToDate;
    }
    let lost = USER_DROPPED | KERNEL_DROPPED | EVENT_IDS_WRAPPED | ROOT_CHANGED;
    if let Some((p, _)) = changed.iter().find(|(_, f)| *f & lost != 0) {
        return CatchUp::FullCrawl(format!("change history incomplete at {}", p.display()));
    }

    let mut events = Vec::new();
    for (path, flags) in changed {
        let Some(path_str) = path.to_str() else { continue };
        if !roots.iter().any(|r| under_or_eq(path_str, r)) {
            continue;
        }
        if flags & MUST_SCAN_SUBDIRS != 0 {
            if roots.iter().any(|r| r.as_path() == path) {
                return CatchUp::FullCrawl(format!("rescan of watch root {path_str}"));
            }
            rescan(&path, formats, &paths_under, &mut events);
        } else if path.symlink_metadata().is_ok() {
            events.push(changed_event(path, formats));
        } else {
            if flags & ITEM_IS_DIR != 0 {
                for child in paths_under(path_str) {
                    events.push(deleted_event(PathBuf::from(child), formats));
                }
            }
            events.push(deleted_event(path, formats));
        }
        if events.len() > MAX_REPLAY_PATHS {
            return CatchUp::FullCrawl(format!("more than {MAX_REPLAY_PATHS} changed paths"));
        }
    }
    if events.is_empty() {
        CatchUp::UpToDate
    } else {
        CatchUp::Events(events)
    }
}

/// Re-reads one subtree: everything on disk is re-indexed, indexed paths that
/// are gone are deleted.
fn rescan(
    dir:         &Path,
    formats:     &[String],
    paths_under: &impl Fn(&str) -> Vec<String>,
    events:      &mut Vec<IndexEvent>,
) {
    let walker = ignore::WalkBuilder::new(dir).hidden(false).git_ignore(false).build();
    for entry in walker.flatten() {
        events.push(changed_event(entry.into_path(), formats));
    }
    if let Some(dir_str) = dir.to_str() {
        for p in paths_under(dir_str) {
            let p = PathBuf::from(p);
            if p.symlink_metadata().is_err() {
                events.push(deleted_event(p, formats));
            }
        }
    }
}

fn under_or_eq(path: &str, root: &Path) -> bool {
    root.to_str().is_some_and(|r| path == r || is_under(path, r))
}

fn is_archive(path: &Path, formats: &[String]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| formats.iter().any(|f| f.eq_ignore_ascii_case(e)))
}

fn changed_event(path: PathBuf, formats: &[String]) -> IndexEvent {
    if is_archive(&path, formats) {
        IndexEvent::ArchiveModified(path)
    } else {
        IndexEvent::Modified(path)
    }
}

fn deleted_event(path: PathBuf, formats: &[String]) -> IndexEvent {
    if is_archive(&path, formats) {
        IndexEvent::ArchiveDeleted(path)
    } else {
        IndexEvent::Deleted(path)
    }
}

#[cfg(target_os = "macos")]
mod fsevents {
    use std::collections::HashMap;
    use std::ffi::{c_char, c_void, CStr, CString};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use fsevent_sys as fse;
    use fse::core_foundation as cf;

    use super::Position;

    /// Upper bound on one history replay; FSEvents normally delivers weeks of
    /// history in seconds.
    const REPLAY_TIMEOUT: Duration = Duration::from_secs(120);

    #[link(name = "CoreServices", kind = "framework")]
    extern "C" {
        // Missing from fsevent-sys. Returns NULL for volumes without an
        // FSEvents store (e.g. network shares), which cannot be replayed.
        fn FSEventsCopyUUIDForDevice(dev: i32) -> cf::CFRef;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFUUIDCreateString(alloc: cf::CFRef, uuid: cf::CFRef) -> cf::CFStringRef;
        fn CFRunLoopRunInMode(mode: cf::CFStringRef, seconds: f64, return_after_source: u8) -> i32;
    }

    pub(super) fn current_position(roots: &[PathBuf]) -> Option<Position> {
        // Read the event id first: an event racing with the UUID lookups is
        // then replayed again rather than missed.
        // SAFETY: no arguments; returns a plain integer.
        let event_id = unsafe { fse::FSEventsGetCurrentEventId() };
        let mut out = Vec::with_capacity(roots.len());
        for root in roots {
            let dev = std::fs::metadata(root).ok()?.dev();
            out.push((root.to_str()?.to_owned(), volume_uuid(dev as i32)?));
        }
        Some(Position { event_id, roots: out })
    }

    fn volume_uuid(dev: i32) -> Option<String> {
        // SAFETY: both calls follow the CF Create/Copy rule; every non-null
        // object returned is released below.
        unsafe {
            let uuid = FSEventsCopyUUIDForDevice(dev);
            if uuid.is_null() {
                return None;
            }
            let s = CFUUIDCreateString(cf::kCFAllocatorDefault, uuid);
            cf::CFRelease(uuid);
            if s.is_null() {
                return None;
            }
            let mut buf = [0 as c_char; 64];
            let ok = cf::CFStringGetCString(s, buf.as_mut_ptr(), buf.len() as cf::CFIndex, cf::kCFStringEncodingUTF8);
            cf::CFRelease(s);
            ok.then(|| CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
        }
    }

    struct Collector {
        /// FSEvents reports resolved paths (`/private/tmp/x` for `/tmp/x`);
        /// `(resolved prefix, configured root)` maps them back to the paths
        /// the index stores.
        prefixes: Vec<(PathBuf, PathBuf)>,
        changed:  HashMap<PathBuf, u32>,
        done:     bool,
    }

    impl Collector {
        fn add(&mut self, path: &Path, flags: u32) {
            let path = self.prefixes.iter()
                .find_map(|(real, root)| path.strip_prefix(real).ok().map(|rest| {
                    if rest.as_os_str().is_empty() { root.clone() } else { root.join(rest) }
                }))
                .unwrap_or_else(|| path.to_path_buf());
            *self.changed.entry(path).or_insert(0) |= flags;
        }
    }

    extern "C" fn callback(
        _stream: fse::FSEventStreamRef,
        info:    *mut c_void,
        count:   usize,
        paths:   *mut c_void,
        flags:   *const fse::FSEventStreamEventFlags,
        _ids:    *const fse::FSEventStreamEventId,
    ) {
        // SAFETY: `info` is the `Collector` passed in the stream context and
        // outlives the stream; without kFSEventStreamCreateFlagUseCFTypes
        // `paths` is an array of `count` C strings and `flags` has `count`
        // entries, all valid for the duration of the callback.
        unsafe {
            let col = &mut *info.cast::<Collector>();
            let paths = paths.cast::<*const c_char>();
            for i in 0..count {
                let f = *flags.add(i);
                if f & fse::kFSEventStreamEventFlagHistoryDone != 0 {
                    col.done = true;
                    continue;
                }
                let p = CStr::from_ptr(*paths.add(i));
                col.add(Path::new(std::ffi::OsStr::from_bytes(p.to_bytes())), f);
            }
        }
    }

    /// Collects every change under `roots` since `since`, as path -> OR of
    /// event flags.
    pub(super) fn replay(roots: &[PathBuf], since: u64) -> Result<HashMap<PathBuf, u32>, String> {
        let mut col = Collector {
            prefixes: roots.iter()
                .filter_map(|r| Some((r.canonicalize().ok()?, r.clone())))
                .collect(),
            changed: HashMap::new(),
            done: false,
        };
        let ctx = fse::FSEventStreamContext {
            version: 0,
            info: (&raw mut col).cast(),
            retain: None,
            release: None,
            copy_description: None,
        };
        let deadline = Instant::now() + REPLAY_TIMEOUT;

        // SAFETY: standard FSEvents lifecycle on this thread's run loop. The
        // stream is stopped, invalidated and released before `col` and `ctx`
        // go out of scope, so the callback never sees a dangling pointer;
        // the path array and its strings are released after stream creation
        // (the stream keeps its own copy).
        unsafe {
            let array = cf::CFArrayCreateMutable(cf::kCFAllocatorDefault, 0, &cf::kCFTypeArrayCallBacks);
            for root in roots {
                let c = CString::new(root.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
                let s = cf::CFStringCreateWithCString(cf::kCFAllocatorDefault, c.as_ptr(), cf::kCFStringEncodingUTF8);
                cf::CFArrayAppendValue(array, s);
                cf::CFRelease(s);
            }
            let stream = fse::FSEventStreamCreate(
                cf::kCFAllocatorDefault,
                callback,
                &ctx,
                array,
                since,
                0.0,
                fse::kFSEventStreamCreateFlagFileEvents | fse::kFSEventStreamCreateFlagNoDefer,
            );
            cf::CFRelease(array);
            if stream.is_null() {
                return Err("FSEventStreamCreate failed".into());
            }
            fse::FSEventStreamScheduleWithRunLoop(stream, cf::CFRunLoopGetCurrent(), cf::kCFRunLoopDefaultMode);
            if fse::FSEventStreamStart(stream) == 0 {
                fse::FSEventStreamInvalidate(stream);
                fse::FSEventStreamRelease(stream);
                return Err("FSEventStreamStart failed".into());
            }
            while !col.done && Instant::now() < deadline {
                CFRunLoopRunInMode(cf::kCFRunLoopDefaultMode, 0.25, 0);
            }
            fse::FSEventStreamStop(stream);
            fse::FSEventStreamInvalidate(stream);
            fse::FSEventStreamRelease(stream);
        }
        if col.done {
            Ok(col.changed)
        } else {
            Err(format!("FSEvents history replay timed out after {}s", REPLAY_TIMEOUT.as_secs()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(c: &CatchUp) -> Vec<String> {
        let CatchUp::Events(ev) = c else { return vec![format!("{c:?}")] };
        let mut v: Vec<String> = ev.iter().map(|e| format!("{e:?}")).collect();
        v.sort();
        v
    }

    #[test]
    fn existing_paths_become_modified_and_missing_ones_deleted() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("kept.txt"), b"x").unwrap();
        std::fs::write(root.join("a.zip"), b"x").unwrap();
        let changed = HashMap::from([
            (root.join("kept.txt"), 0x1000),
            (root.join("a.zip"), 0x100),
            (root.join("gone.txt"), 0x200),
            (PathBuf::from("/elsewhere/x"), 0x100),
        ]);
        let out = plan(changed, std::slice::from_ref(&root), &["zip".into()], |_| vec![]);
        let k = kinds(&out);
        assert_eq!(k.len(), 3, "{k:?}");
        assert!(k.iter().any(|e| e.starts_with("ArchiveModified")));
        assert!(k.iter().any(|e| e.starts_with("Modified") && e.contains("kept.txt")));
        assert!(k.iter().any(|e| e.starts_with("Deleted") && e.contains("gone.txt")));
    }

    #[test]
    fn removed_directory_deletes_its_indexed_children() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        let sub = root.join("sub");
        let sub_s = sub.to_str().unwrap().to_owned();
        let children = vec![format!("{sub_s}/a"), format!("{sub_s}/b/c")];
        let out = plan(HashMap::from([(sub.clone(), 0x200 | ITEM_IS_DIR)]), &[root], &[], |d| {
            if d == sub_s { children.clone() } else { vec![] }
        });
        assert_eq!(kinds(&out).len(), 3);
    }

    #[test]
    fn dropped_events_or_root_rescan_force_a_crawl() {
        let root = PathBuf::from("/w");
        let dropped = plan(HashMap::from([(root.join("x"), KERNEL_DROPPED)]), std::slice::from_ref(&root), &[], |_| vec![]);
        assert!(matches!(dropped, CatchUp::FullCrawl(_)));
        let rescan = plan(HashMap::from([(root.clone(), MUST_SCAN_SUBDIRS)]), &[root], &[], |_| vec![]);
        assert!(matches!(rescan, CatchUp::FullCrawl(_)));
    }

    #[test]
    fn subdir_rescan_reindexes_and_prunes() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        let sub = root.join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("new.txt"), b"x").unwrap();
        let stale = format!("{}/old.txt", sub.display());
        let out = plan(HashMap::from([(sub.clone(), MUST_SCAN_SUBDIRS)]), &[root], &[], |_| vec![stale.clone()]);
        let k = kinds(&out);
        assert!(k.iter().any(|e| e.starts_with("Modified") && e.contains("new.txt")), "{k:?}");
        assert!(k.iter().any(|e| e.starts_with("Deleted") && e.contains("old.txt")), "{k:?}");
    }

    #[test]
    fn changed_roots_force_a_crawl() {
        let a = Position { event_id: 1, roots: vec![("/w".into(), "U1".into())] };
        let b = Position { event_id: 9, roots: vec![("/w".into(), "U2".into())] };
        assert!(matches!(catch_up(&a, &b, &[], |_| vec![]), CatchUp::FullCrawl(_)));
        assert!(matches!(catch_up(&b, &b, &[], |_| vec![]), CatchUp::UpToDate));
        assert_eq!(Position::from_json(&a.to_json()), Some(a));
    }

    /// End to end against the real FSEvents history: changes made after a
    /// stored position come back from a replay.
    #[cfg(target_os = "macos")]
    #[test]
    fn fsevents_replay_sees_offline_changes() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("before.txt"), b"x").unwrap();
        let start = current_position(std::slice::from_ref(&root)).expect("position");
        std::fs::write(root.join("after.txt"), b"x").unwrap();
        std::fs::remove_file(root.join("before.txt")).unwrap();
        // FSEvents writes history asynchronously; give it a moment.
        std::thread::sleep(std::time::Duration::from_secs(2));
        let now = current_position(std::slice::from_ref(&root)).expect("position");
        let out = catch_up(&start, &now, &[], |_| vec![]);
        let k = kinds(&out);
        assert!(k.iter().any(|e| e.starts_with("Modified") && e.contains("after.txt")), "{k:?}");
        assert!(k.iter().any(|e| e.starts_with("Deleted") && e.contains("before.txt")), "{k:?}");
    }
}
