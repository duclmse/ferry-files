//! Catch-up by timestamp scan, for Linux and Windows
//! (docs/plans/everything-borrowing.md, phase 5).
//!
//! inotify keeps no history, so there is no log to replay; Windows replays
//! the USN journal (`usn.rs`) and comes here only when that is not possible
//! (no administrator rights, journal reset or wrapped, not NTFS). Instead the
//! stored position is the wall-clock time it was captured, and a warm start
//! walks the tree looking for entries whose times are not older than that.
//! This costs one `stat` per entry, but no index writes for unchanged files,
//! which is what makes a full crawl slow.
//!
//! Besides mtime, Unix checks ctime, which only the kernel sets: a file
//! extracted or copied with its old mtime preserved still has a fresh ctime.
//! Windows checks the creation time instead, which a copy or extraction sets
//! to now. A directory's mtime changes when an entry is added, removed or
//! renamed in it, so the children of each changed directory are compared with
//! the index: indexed ones that are gone are deleted and unindexed ones are
//! added. That catches entries moved in with all their times old (NTFS keeps
//! them on rename). A directory that is not in the index is walked in full,
//! because its contents can keep their old times.
//!
//! Missed: changes that leave every checked time older than the position,
//! which takes a clock set backwards or a tool that rewrites times.

// Not used on macOS, which replays FSEvents; built there so the tests run.
#![cfg_attr(target_os = "macos", allow(dead_code))]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use super::{changed_event, deleted_event, CatchUp, IndexLookup, Position, MAX_REPLAY_PATHS};

/// Timestamps have one-second resolution in the index and on some
/// filesystems, and the position is captured while changes may still be
/// landing; anything this close to it counts as changed.
const SLACK_SECS: i64 = 2;

pub(super) fn current_position(roots: &[PathBuf]) -> Option<Position> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let mut out = Vec::with_capacity(roots.len());
    for root in roots {
        out.push((root.to_str()?.to_owned(), volume_id(root)?));
    }
    Some(Position {
        event_id: now,
        roots: out,
        usn: Vec::new(),
    })
}

/// A different device means a different filesystem was mounted on the root:
/// its timestamps say nothing about what the index holds.
#[cfg(unix)]
fn volume_id(root: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    Some(format!("dev:{}", std::fs::metadata(root).ok()?.dev()))
}

// The volume serial number needs an open handle (std's accessor is unstable);
// remounting a different volume at a watched path is rare on Windows.
#[cfg(not(unix))]
fn volume_id(root: &Path) -> Option<String> {
    std::fs::metadata(root).ok().map(|_| "scan".to_owned())
}

/// The latest of the times this platform's tools cannot set back, in Unix
/// seconds.
fn newest_time(meta: &std::fs::Metadata) -> i64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.mtime().max(meta.ctime())
    }
    #[cfg(not(unix))]
    {
        let secs = |t: std::io::Result<std::time::SystemTime>| {
            t.ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        };
        secs(meta.modified()).max(secs(meta.created()))
    }
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with('.'))
}

/// Paths under `roots` (not excluded by `skip`) whose mtime or ctime is at or
/// after `threshold`, with whether each is a directory. `None` past
/// `MAX_REPLAY_PATHS`.
fn changed_since(
    roots: &[PathBuf],
    threshold: i64,
    skip: &(dyn Fn(&Path, bool) -> bool + Sync),
) -> Option<Vec<(PathBuf, bool)>> {
    let first = roots.first()?;
    let mut wb = ignore::WalkBuilder::new(first);
    for r in &roots[1..] {
        wb.add(r);
    }
    let found = Mutex::new(Vec::new());
    let overflow = AtomicBool::new(false);
    wb.hidden(false).git_ignore(false).build_parallel().run(|| {
        Box::new(|entry| {
            let Ok(entry) = entry else {
                return ignore::WalkState::Continue;
            };
            let Ok(meta) = entry.metadata() else {
                return ignore::WalkState::Continue;
            };
            let path = entry.path();
            if skip(path, is_hidden(path)) {
                return if meta.is_dir() {
                    ignore::WalkState::Skip
                } else {
                    ignore::WalkState::Continue
                };
            }
            if newest_time(&meta) >= threshold {
                let Ok(mut found) = found.lock() else {
                    return ignore::WalkState::Quit;
                };
                found.push((path.to_path_buf(), meta.is_dir()));
                if found.len() > MAX_REPLAY_PATHS {
                    overflow.store(true, Ordering::Relaxed);
                    return ignore::WalkState::Quit;
                }
            }
            ignore::WalkState::Continue
        })
    });
    if overflow.load(Ordering::Relaxed) {
        return None;
    }
    let mut found = found.into_inner().ok()?;
    // Parents first, so a new directory's full walk covers its new children.
    found.sort();
    Some(found)
}

pub(super) fn reconcile(
    roots: &[PathBuf],
    since: u64,
    formats: &[String],
    skip: &(dyn Fn(&Path, bool) -> bool + Sync),
    index: &dyn IndexLookup,
) -> CatchUp {
    if let Some(r) = roots.iter().find(|r| r.symlink_metadata().is_err()) {
        return CatchUp::FullCrawl(format!("watch root {} is missing", r.display()));
    }
    let threshold = i64::try_from(since)
        .unwrap_or(i64::MAX)
        .saturating_sub(SLACK_SECS);
    let Some(found) = changed_since(roots, threshold, skip) else {
        return CatchUp::FullCrawl(format!("more than {MAX_REPLAY_PATHS} changed paths"));
    };

    let too_many = || CatchUp::FullCrawl(format!("more than {MAX_REPLAY_PATHS} changed paths"));
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut events = Vec::new();
    let mut walked: Vec<PathBuf> = Vec::new();
    let mut added: Vec<PathBuf> = Vec::new();
    for (path, is_dir) in found {
        if is_dir && !walked.iter().any(|w| path.starts_with(w)) {
            let Some(s) = path.to_str() else { continue };
            if index.contains(s) {
                let indexed: HashSet<String> = index.children_of(s).into_iter().collect();
                for child in &indexed {
                    let child_path = PathBuf::from(child);
                    if child_path.symlink_metadata().is_ok() || !seen.insert(child_path.clone()) {
                        continue;
                    }
                    for p in index.paths_under(child) {
                        events.push(deleted_event(PathBuf::from(p), formats));
                    }
                    events.push(deleted_event(child_path, formats));
                }
                for entry in std::fs::read_dir(&path).into_iter().flatten().flatten() {
                    let child = entry.path();
                    let unindexed = child.to_str().is_some_and(|c| !indexed.contains(c));
                    let is_child_dir = entry.file_type().is_ok_and(|t| t.is_dir());
                    if unindexed && !skip(&child, is_hidden(&child)) {
                        if is_child_dir {
                            added.push(child);
                        } else if seen.insert(child.clone()) {
                            events.push(changed_event(child, formats));
                        }
                    }
                }
            } else {
                added.push(path.clone());
            }
            for dir in added.drain(..) {
                if walked.iter().any(|w| dir.starts_with(w)) {
                    continue;
                }
                let Some(all) = changed_since(std::slice::from_ref(&dir), i64::MIN, skip) else {
                    return too_many();
                };
                for (p, _) in all {
                    if seen.insert(p.clone()) {
                        events.push(changed_event(p, formats));
                    }
                }
                walked.push(dir);
            }
        }
        if seen.insert(path.clone()) {
            events.push(changed_event(path, formats));
        }
        if events.len() > MAX_REPLAY_PATHS {
            return too_many();
        }
    }
    if events.is_empty() {
        CatchUp::UpToDate
    } else {
        CatchUp::Events(events)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    struct Fake(HashSet<String>);

    impl IndexLookup for Fake {
        fn contains(&self, path: &str) -> bool {
            self.0.contains(path)
        }
        fn children_of(&self, dir: &str) -> Vec<String> {
            self.0
                .iter()
                .filter(|p| Path::new(p).parent() == Some(Path::new(dir)))
                .cloned()
                .collect()
        }
        fn paths_under(&self, dir: &str) -> Vec<String> {
            let prefix = format!("{dir}/");
            self.0
                .iter()
                .filter(|p| p.starts_with(&prefix))
                .cloned()
                .collect()
        }
    }

    fn events(c: &CatchUp) -> Vec<String> {
        let CatchUp::Events(ev) = c else {
            return vec![format!("{c:?}")];
        };
        let mut v: Vec<String> = ev.iter().map(|e| format!("{e:?}")).collect();
        v.sort();
        v
    }

    #[test]
    fn finds_changes_since_the_position_and_nothing_older() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("root");
        let elsewhere = dir.path().join("elsewhere");
        for d in [
            root.join("keep"),
            root.join("gone"),
            root.join("node_modules"),
            elsewhere.join("old"),
        ] {
            std::fs::create_dir_all(d).unwrap();
        }
        for f in [
            root.join("keep/a.txt"),
            root.join("keep/old_unindexed.txt"),
            root.join("gone/b.txt"),
            root.join("edited.txt"),
            elsewhere.join("old/inner.txt"),
        ] {
            std::fs::write(f, b"x").unwrap();
        }
        let s = |p: PathBuf| p.to_str().unwrap().to_owned();
        let index = Fake(HashSet::from([
            s(root.clone()),
            s(root.join("keep")),
            s(root.join("keep/a.txt")),
            s(root.join("gone")),
            s(root.join("gone/b.txt")),
            s(root.join("edited.txt")),
        ]));
        // Older than the position by more than the slack.
        std::thread::sleep(std::time::Duration::from_millis(3100));
        let pos = current_position(std::slice::from_ref(&root)).unwrap();

        std::fs::write(root.join("new.txt"), b"x").unwrap();
        std::fs::write(root.join("keep/new2.txt"), b"x").unwrap();
        std::fs::write(root.join("edited.txt"), b"xy").unwrap();
        std::fs::remove_dir_all(root.join("gone")).unwrap();
        std::fs::rename(elsewhere.join("old"), root.join("moved")).unwrap();
        std::fs::write(root.join("node_modules/skip.js"), b"x").unwrap();

        let skip = |p: &Path, _hidden: bool| p.ends_with("node_modules");
        let out = reconcile(
            std::slice::from_ref(&root),
            pos.event_id,
            &[],
            &skip,
            &index,
        );
        let ev = events(&out);
        let has =
            |kind: &str, name: &str| ev.iter().any(|e| e.starts_with(kind) && e.contains(name));
        assert!(has("Modified", "new.txt"), "{ev:?}");
        assert!(has("Modified", "edited.txt"), "{ev:?}");
        assert!(has("Deleted", "gone\")"), "{ev:?}");
        assert!(has("Deleted", "b.txt"), "{ev:?}");
        // Moved in with its old ctime; found because `moved` is not indexed.
        assert!(has("Modified", "inner.txt"), "{ev:?}");
        // Old times and not indexed, in a directory that changed: stands in
        // for a file moved in on a filesystem that keeps times on rename.
        assert!(has("Modified", "old_unindexed.txt"), "{ev:?}");
        assert!(has("Modified", "new2.txt"), "{ev:?}");
        assert!(!has("", "a.txt"), "{ev:?}");
        assert!(!has("", "node_modules"), "{ev:?}");
        let unique: HashSet<&String> = ev.iter().collect();
        assert_eq!(unique.len(), ev.len(), "{ev:?}");
    }

    #[test]
    fn missing_root_forces_a_crawl() {
        let out = reconcile(
            &[PathBuf::from("/no/such/root")],
            0,
            &[],
            &|_, _| false,
            &Fake(HashSet::new()),
        );
        assert!(matches!(out, CatchUp::FullCrawl(_)));
    }
}
