//! `watcher.backend = "usn"`: the NTFS change journal, read as it grows,
//! the way Everything follows a volume.
//!
//! The journal is on disk and read from a position, so a burst cannot
//! overflow it: the reader falls behind and catches up. Changes are lost
//! only when the journal wraps past the position or is deleted, which the
//! read reports.
//!
//! A record names a file by its parent folder's file reference number
//! (FRN) and its own name. Folders are resolved to paths with
//! `OpenFileById` and kept in a cache, which also learns each folder this
//! reader sees created, so the deletes inside a folder deleted moments
//! later still resolve. When nothing resolves (a whole tree deleted before
//! its records are read), the children are skipped: the delete of the
//! topmost folder resolves through its parent, which still exists, and
//! deleting a folder deletes everything indexed below it.
//!
//! The journal covers the whole volume, so most records are for folders
//! outside the watch roots; those resolve once to "outside" and are cached.

#![cfg_attr(not(windows), allow(dead_code))]

use std::collections::HashMap;
use std::path::PathBuf;

use super::super::journal::usn::{
    Record, REASON_FILE_CREATE, REASON_FILE_DELETE, REASON_RENAME_NEW_NAME, REASON_RENAME_OLD_NAME,
};
use super::Batch;
use crate::indexer::crawler::IndexEvent;

/// Past this many cached folders the cache starts over, which costs one
/// lookup per folder seen again.
const MAX_CACHED_DIRS: usize = 500_000;

/// Records to index events, with the folder cache.
#[derive(Default)]
pub(super) struct Tracker {
    /// Folder FRN -> its path under a watch root, or `None` for one outside
    /// every root or gone.
    dirs: HashMap<u64, Option<PathBuf>>,
    /// The old path of a rename whose new name has not been read yet.
    renamed_from: HashMap<u64, PathBuf>,
}

impl Tracker {
    pub(super) fn apply(
        &mut self,
        r: &Record,
        mut resolve: impl FnMut(u64) -> Option<PathBuf>,
        batch: &mut Batch,
    ) {
        if self.dirs.len() > MAX_CACHED_DIRS {
            self.dirs.clear();
        }
        let parent = self
            .dirs
            .entry(r.parent)
            .or_insert_with(|| resolve(r.parent))
            .clone();
        let path = parent.map(|p| p.join(&r.name));
        if r.is_dir {
            if r.reason & (REASON_RENAME_OLD_NAME | REASON_RENAME_NEW_NAME) != 0 {
                // Every cached path below a renamed folder is stale, and a
                // folder moved in from outside a root was cached as outside.
                self.dirs.clear();
            } else if r.reason & REASON_FILE_DELETE != 0 {
                self.dirs.remove(&r.frn);
            } else if r.reason & REASON_FILE_CREATE != 0 {
                if let Some(p) = &path {
                    self.dirs.insert(r.frn, Some(p.clone()));
                }
            }
        }
        let Some(path) = path else {
            return;
        };
        if r.reason & REASON_FILE_DELETE != 0 {
            self.renamed_from.remove(&r.frn);
            batch.push(IndexEvent::Deleted(path));
        } else if r.reason & REASON_RENAME_OLD_NAME != 0 {
            self.renamed_from.insert(r.frn, path);
        } else if r.reason & REASON_RENAME_NEW_NAME != 0 {
            match self.renamed_from.remove(&r.frn) {
                Some(from) if from != path => batch.push(IndexEvent::Renamed { from, to: path }),
                Some(_) => batch.push(IndexEvent::Modified(path)),
                None => batch.push(IndexEvent::Created(path)),
            }
        } else if r.reason & REASON_FILE_CREATE != 0 {
            batch.push(IndexEvent::Created(path));
        } else {
            batch.push(IndexEvent::Modified(path));
        }
    }

    /// Old names whose new name never came: moved out of every root.
    pub(super) fn flush(&mut self, batch: &mut Batch) {
        for (_, old) in self.renamed_from.drain() {
            batch.push(IndexEvent::Deleted(old));
        }
    }
}

#[cfg(windows)]
pub(super) use imp::start;

#[cfg(windows)]
mod imp {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use tokio::sync::mpsc::UnboundedSender;
    use windows::Win32::Foundation::{
        ERROR_JOURNAL_DELETE_IN_PROGRESS, ERROR_JOURNAL_ENTRY_DELETED, ERROR_JOURNAL_NOT_ACTIVE,
    };
    use windows::Win32::System::Ioctl::{FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V0};
    use windows::Win32::System::IO::DeviceIoControl;

    use super::super::super::journal::usn::{
        by_volume, map_to_root, open_volume, parse, path_of, query, strip_verbatim, volume_of,
        Handle, Record,
    };
    use super::super::{pause, unix_now, wait_for, Batch, Msg, RETRY};
    use super::Tracker;

    const READ_BUF: usize = 1 << 20;

    enum ReadError {
        /// The journal no longer reaches the position: changes were lost.
        Lost(String),
        Other(String),
    }

    struct Volume {
        name: String,
        handle: Handle,
        journal: u64,
        next: i64,
        /// `(resolved root, configured root)`, as for the replay.
        prefixes: Vec<(String, PathBuf)>,
    }

    /// Starts a reader for every volume whose journal can be read, and
    /// returns the roots they cover; the others are left to the caller.
    pub(in crate::watcher::live) fn start(
        roots: &[PathBuf],
        window: Duration,
        tx: &UnboundedSender<Msg>,
        stop: &Arc<AtomicBool>,
    ) -> (Vec<PathBuf>, Vec<std::thread::JoinHandle<()>>) {
        let mut covered = Vec::new();
        let mut threads = Vec::new();
        let Some(groups) = by_volume(roots, volume_of) else {
            tracing::warn!("watcher: a watch root has no volume; not using the USN journal");
            return (covered, threads);
        };
        for (name, idx) in groups {
            let prefixes: Option<Vec<(String, PathBuf)>> = idx
                .iter()
                .map(|&i| {
                    let c = roots[i].canonicalize().ok()?;
                    Some((strip_verbatim(c.to_str()?).to_owned(), roots[i].clone()))
                })
                .collect();
            let Some(prefixes) = prefixes else {
                tracing::warn!(
                    "watcher: a root on {name} cannot be resolved; not using its journal"
                );
                continue;
            };
            let opened = open_volume(&name).and_then(|h| Ok((query(&h)?, h)));
            let (data, handle) = match opened {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        "watcher: USN journal of {name} not readable ({e}); \
                         using ReadDirectoryChangesW for its roots"
                    );
                    continue;
                }
            };
            let vol = Volume {
                name: name.clone(),
                handle,
                journal: data.UsnJournalID,
                next: data.NextUsn,
                prefixes,
            };
            let (tx, stop) = (tx.clone(), Arc::clone(stop));
            match std::thread::Builder::new()
                .name("fs-usn".into())
                .spawn(move || run(vol, window, &tx, &stop))
            {
                Ok(t) => {
                    tracing::info!("watcher: following the USN journal of {name}");
                    covered.extend(idx.iter().map(|&i| roots[i].clone()));
                    threads.push(t);
                }
                Err(e) => tracing::warn!("watcher: USN thread for {name}: {e}"),
            }
        }
        (covered, threads)
    }

    fn read(vol: &Volume, buf: &mut [u8], out: &mut Vec<Record>) -> Result<i64, ReadError> {
        let req = READ_USN_JOURNAL_DATA_V0 {
            StartUsn: vol.next,
            ReasonMask: u32::MAX,
            ReturnOnlyOnClose: 0,
            Timeout: 0,
            BytesToWaitFor: 0,
            UsnJournalID: vol.journal,
        };
        let mut got = 0u32;
        // SAFETY: `req` and `buf` are valid for the sizes passed and outlive
        // the synchronous call.
        let res = unsafe {
            DeviceIoControl(
                vol.handle.0,
                FSCTL_READ_USN_JOURNAL,
                Some((&raw const req).cast()),
                size_of::<READ_USN_JOURNAL_DATA_V0>() as u32,
                Some(buf.as_mut_ptr().cast()),
                buf.len() as u32,
                Some(&mut got),
                None,
            )
        };
        if let Err(e) = res {
            let lost = [
                ERROR_JOURNAL_ENTRY_DELETED,
                ERROR_JOURNAL_DELETE_IN_PROGRESS,
                ERROR_JOURNAL_NOT_ACTIVE,
            ]
            .iter()
            .any(|c| e.code() == c.to_hresult());
            let msg = format!("read USN journal of {}: {e}", vol.name);
            return Err(if lost {
                ReadError::Lost(msg)
            } else {
                ReadError::Other(msg)
            });
        }
        parse(&buf[..got as usize], out)
            .ok_or_else(|| ReadError::Other("unexpected USN record format".into()))
    }

    fn run(mut vol: Volume, window: Duration, tx: &UnboundedSender<Msg>, stop: &AtomicBool) {
        let mut buf = vec![0u8; READ_BUF];
        let mut records = Vec::new();
        let mut tracker = Tracker::default();
        let mut batch = Batch::default();
        let mut since = unix_now();
        while !stop.load(Ordering::Relaxed) {
            records.clear();
            let read_at = unix_now();
            match read(&vol, &mut buf, &mut records) {
                Ok(next) => {
                    since = read_at;
                    for r in &records {
                        tracker.apply(
                            r,
                            |frn| {
                                let p = path_of(&vol.handle, frn)?;
                                map_to_root(strip_verbatim(&p), &vol.prefixes)
                            },
                            &mut batch,
                        );
                    }
                    let caught_up = next <= vol.next || records.is_empty();
                    vol.next = next;
                    if caught_up && !pause(wait_for(&batch, window), stop) {
                        return;
                    }
                }
                Err(err) => {
                    let (lost, msg) = match err {
                        ReadError::Lost(m) => (true, m),
                        ReadError::Other(m) => (false, m),
                    };
                    tracing::warn!("watcher: {msg}");
                    if !lost && !pause(RETRY, stop) {
                        return;
                    }
                    match query(&vol.handle) {
                        Ok(d) => {
                            vol.journal = d.UsnJournalID;
                            vol.next = d.NextUsn;
                            tracker = Tracker::default();
                            let _ = tx.send(Msg::Lost { since, reason: msg });
                            since = unix_now();
                        }
                        Err(e) => {
                            tracing::warn!("watcher: {e}");
                            if !pause(RETRY, stop) {
                                return;
                            }
                        }
                    }
                }
            }
            if batch.deadline(window).is_some_and(|d| d <= Instant::now()) {
                tracker.flush(&mut batch);
                if tx.send(Msg::Events(batch.take())).is_err() {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIR: bool = true;
    const FILE: bool = false;

    fn rec(frn: u64, parent: u64, reason: u32, is_dir: bool, name: &str) -> Record {
        Record {
            frn,
            parent,
            reason,
            is_dir,
            name: name.into(),
        }
    }

    fn run(records: &[Record], known: &[(u64, &str)]) -> (Vec<String>, usize) {
        let mut t = Tracker::default();
        let mut batch = Batch::default();
        let mut lookups = 0;
        for r in records {
            t.apply(
                r,
                |frn| {
                    lookups += 1;
                    known
                        .iter()
                        .find(|(f, _)| *f == frn)
                        .map(|(_, p)| PathBuf::from(p))
                },
                &mut batch,
            );
        }
        t.flush(&mut batch);
        (
            batch.take().iter().map(|e| format!("{e:?}")).collect(),
            lookups,
        )
    }

    fn ev(e: IndexEvent) -> String {
        format!("{e:?}")
    }

    #[test]
    fn a_deleted_tree_resolves_through_the_folders_it_saw_created() {
        let root = PathBuf::from("/w");
        // Folder 10 is created under the root, a file in it, then both are
        // deleted; by the time the records are read, 10 is gone and only
        // the root resolves.
        let records = [
            rec(10, 1, REASON_FILE_CREATE, DIR, "d"),
            rec(11, 10, REASON_FILE_CREATE, FILE, "f"),
            rec(11, 10, REASON_FILE_DELETE, FILE, "f"),
            rec(10, 1, REASON_FILE_DELETE, DIR, "d"),
        ];
        let (got, lookups) = run(&records, &[(1, "/w")]);
        assert_eq!(lookups, 1, "only the root is looked up");
        // The batch keeps the last change per path.
        assert_eq!(
            got,
            vec![
                ev(IndexEvent::Deleted(root.join("d").join("f"))),
                ev(IndexEvent::Deleted(root.join("d"))),
            ]
        );
    }

    #[test]
    fn renames_pair_by_file_and_moves_across_the_root_edge_become_create_or_delete() {
        let root = PathBuf::from("/w");
        let records = [
            rec(20, 1, REASON_RENAME_OLD_NAME, FILE, "a"),
            rec(20, 1, REASON_RENAME_NEW_NAME, FILE, "b"),
            // Moved in from outside (folder 2 is not under a root).
            rec(21, 2, REASON_RENAME_OLD_NAME, FILE, "x"),
            rec(21, 1, REASON_RENAME_NEW_NAME, FILE, "x"),
            // Moved out.
            rec(22, 1, REASON_RENAME_OLD_NAME, FILE, "y"),
            rec(22, 2, REASON_RENAME_NEW_NAME, FILE, "y"),
            rec(23, 1, 0x0000_0002, FILE, "edited"),
            // Outside every root: nothing.
            rec(24, 2, REASON_FILE_CREATE, FILE, "z"),
        ];
        let (got, _) = run(&records, &[(1, "/w")]);
        assert_eq!(
            got,
            vec![
                ev(IndexEvent::Renamed {
                    from: root.join("a"),
                    to: root.join("b")
                }),
                ev(IndexEvent::Created(root.join("x"))),
                ev(IndexEvent::Modified(root.join("edited"))),
                ev(IndexEvent::Deleted(root.join("y"))),
            ]
        );
    }

    #[test]
    fn a_folder_rename_forgets_cached_paths() {
        let records = [
            rec(30, 1, REASON_FILE_CREATE, DIR, "old"),
            rec(31, 30, REASON_FILE_CREATE, FILE, "f"),
            rec(30, 1, REASON_RENAME_OLD_NAME, DIR, "old"),
            rec(30, 1, REASON_RENAME_NEW_NAME, DIR, "new"),
            rec(31, 30, 0x0000_0002, FILE, "f"),
        ];
        // Folder 30 resolves to its current name.
        let (got, _) = run(&records, &[(1, "/w"), (30, "/w/new")]);
        assert_eq!(
            got.last(),
            Some(&ev(IndexEvent::Modified(PathBuf::from("/w/new").join("f"))))
        );
    }
}
