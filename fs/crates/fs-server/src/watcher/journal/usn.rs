//! Catch-up by NTFS USN journal replay, for Windows
//! (docs/plans/everything-borrowing.md, phase 5 follow-up).
//!
//! Every NTFS volume keeps a change journal: one record per change, holding
//! the file's and its parent directory's file reference numbers (FRN), the
//! name, the attributes and a reason mask. The position stores the journal ID
//! and next USN of each root's volume; a warm start reads the records written
//! since and turns them into the same `path -> flags` map the FSEvents replay
//! produces, so `plan` reads the current state of each changed path.
//!
//! A record names its parent only by FRN, so the parent's *current* path is
//! looked up with `OpenFileById`. When the parent is gone too, the record is
//! skipped: the deletion of the parent (or of the nearest ancestor that still
//! resolves) is in the journal as well, and deleting a directory deletes
//! everything indexed below it.
//!
//! Reading the journal needs a volume handle, which needs administrator
//! rights (the service runs as LocalSystem). Without them, after the journal
//! was reset or wrapped past the stored USN, or on a non-NTFS volume, the
//! replay fails and catch-up falls back to the timestamp scan.

// The parser and the fold are platform-independent so their tests run
// everywhere; only Windows reads a journal.
#![cfg_attr(not(windows), allow(dead_code))]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{ITEM_IS_DIR, MAX_REPLAY_PATHS, MUST_SCAN_SUBDIRS};

pub(in crate::watcher) const REASON_FILE_CREATE: u32 = 0x0000_0100;
pub(in crate::watcher) const REASON_FILE_DELETE: u32 = 0x0000_0200;
pub(in crate::watcher) const REASON_RENAME_OLD_NAME: u32 = 0x0000_1000;
pub(in crate::watcher) const REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;
const ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;

/// A volume's journal position: a different journal ID means the journal
/// was deleted and recreated, and its USNs restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsnPos {
    pub journal: u64,
    pub next: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::watcher) struct Record {
    pub frn: u64,
    pub parent: u64,
    pub reason: u32,
    pub is_dir: bool,
    pub name: String,
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// Parses one `FSCTL_READ_USN_JOURNAL` output buffer: the USN to continue
/// from, then `USN_RECORD_V2`s. Returns that USN, or `None` when the buffer
/// is malformed or holds a record version this parser does not know.
pub(in crate::watcher) fn parse(buf: &[u8], out: &mut Vec<Record>) -> Option<i64> {
    let next = i64::from_le_bytes(buf.get(..8)?.try_into().ok()?);
    let mut at = 8;
    while at < buf.len() {
        let len = u32_at(buf, at)? as usize;
        let rec = buf.get(at..at + len)?;
        if len < 60 || u16_at(rec, 4)? != 2 {
            return None;
        }
        let name_len = u16_at(rec, 56)? as usize;
        let name_off = u16_at(rec, 58)? as usize;
        let units: Vec<u16> = rec
            .get(name_off..name_off + name_len)?
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        out.push(Record {
            frn: u64_at(rec, 8)?,
            parent: u64_at(rec, 16)?,
            reason: u32_at(rec, 40)?,
            is_dir: u32_at(rec, 52)? & ATTRIBUTE_DIRECTORY != 0,
            name: String::from_utf16_lossy(&units),
        });
        // Records are 8-byte aligned; RecordLength already includes padding.
        at += len;
    }
    Some(next)
}

/// Maps a resolved path to the configured root it is under. `prefixes` holds
/// `(resolved root, configured root)`; resolved paths are compared ignoring
/// ASCII case, as NTFS does for the names Windows users type.
pub(in crate::watcher) fn map_to_root(
    path: &str,
    prefixes: &[(String, PathBuf)],
) -> Option<PathBuf> {
    prefixes.iter().find_map(|(real, root)| {
        let head = path.get(..real.len())?;
        if !head.eq_ignore_ascii_case(real) {
            return None;
        }
        match path[real.len()..].strip_prefix('\\') {
            _ if path.len() == real.len() => Some(root.clone()),
            Some(rest) => Some(root.join(rest)),
            None => None,
        }
    })
}

/// Folds records into `path -> flags` for `plan`. `resolve` returns the
/// current path of a directory FRN, already passed through `map_to_root`
/// (`None` when it is gone or outside every root).
pub(super) fn fold(
    records: &[Record],
    mut resolve: impl FnMut(u64) -> Option<PathBuf>,
) -> Result<HashMap<PathBuf, u32>, String> {
    let mut parents: HashMap<u64, Option<PathBuf>> = HashMap::new();
    let mut changed: HashMap<PathBuf, u32> = HashMap::new();
    for r in records {
        let parent = parents.entry(r.parent).or_insert_with(|| resolve(r.parent));
        let Some(parent) = parent else { continue };
        let mut flags = 0;
        if r.is_dir {
            flags |= ITEM_IS_DIR;
            // A directory moved in from elsewhere on the volume brings its
            // contents without a record for each of them.
            if r.reason & REASON_RENAME_NEW_NAME != 0 {
                flags |= MUST_SCAN_SUBDIRS;
            }
        }
        *changed.entry(parent.join(&r.name)).or_insert(0) |= flags;
        if changed.len() > MAX_REPLAY_PATHS {
            return Err(format!("more than {MAX_REPLAY_PATHS} changed paths"));
        }
    }
    Ok(changed)
}

/// Strips the `\\?\` that `GetFinalPathNameByHandleW` and `canonicalize`
/// add, so resolved paths compare with configured ones.
pub(in crate::watcher) fn strip_verbatim(p: &str) -> &str {
    p.strip_prefix(r"\\?\").unwrap_or(p)
}

/// Groups roots by the volume they live on, keeping each root's index.
pub(in crate::watcher) fn by_volume(
    roots: &[PathBuf],
    volume_of: impl Fn(&Path) -> Option<String>,
) -> Option<Vec<(String, Vec<usize>)>> {
    let mut out: Vec<(String, Vec<usize>)> = Vec::new();
    for (i, r) in roots.iter().enumerate() {
        let v = volume_of(r)?;
        match out
            .iter_mut()
            .find(|(name, _)| name.eq_ignore_ascii_case(&v))
        {
            Some((_, idx)) => idx.push(i),
            None => out.push((v, vec![i])),
        }
    }
    Some(out)
}

#[cfg(windows)]
pub(super) use imp::{current, replay};
#[cfg(windows)]
pub(in crate::watcher) use imp::{open_volume, path_of, query, volume_of, Handle};

#[cfg(windows)]
mod imp {
    use std::collections::HashMap;
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{
        CloseHandle, ERROR_JOURNAL_ENTRY_DELETED, GENERIC_READ, HANDLE,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FileIdType, GetFinalPathNameByHandleW, GetVolumePathNameW, OpenFileById,
        FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_DESCRIPTOR,
        FILE_ID_DESCRIPTOR_0, FILE_NAME_NORMALIZED, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, GETFINALPATHNAMEBYHANDLE_FLAGS, OPEN_EXISTING, VOLUME_NAME_DOS,
    };
    use windows::Win32::System::Ioctl::{
        FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V0,
        USN_JOURNAL_DATA_V0,
    };
    use windows::Win32::System::IO::DeviceIoControl;

    use super::{by_volume, fold, map_to_root, parse, strip_verbatim, Record, UsnPos};

    /// FSCTL_READ_USN_JOURNAL output buffer; one call returns as many whole
    /// records as fit.
    const READ_BUF: usize = 1 << 20;

    pub(in crate::watcher) struct Handle(pub(in crate::watcher) HANDLE);

    // SAFETY: a volume or file handle may be used from any thread; the
    // journal reader opens it on one and reads on its own.
    unsafe impl Send for Handle {}

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: the handle came from a successful open and is closed
            // only here.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    /// `C:\` for any path on drive C (or the mount point's path).
    pub(in crate::watcher) fn volume_of(path: &Path) -> Option<String> {
        let w = wide(path.as_os_str());
        let mut buf = [0u16; 1024];
        // SAFETY: `w` is NUL-terminated and outlives the call; `buf` is a
        // valid output slice.
        unsafe { GetVolumePathNameW(PCWSTR(w.as_ptr()), &mut buf) }.ok()?;
        let n = buf.iter().position(|&c| c == 0)?;
        Some(String::from_utf16_lossy(&buf[..n]))
    }

    pub(in crate::watcher) fn open_volume(volume: &str) -> Result<Handle, String> {
        // `C:\` -> `\\.\C:`; a volume mounted on a folder is opened by its
        // mount path without the trailing separator in the same way.
        let device = format!(r"\\.\{}", volume.trim_end_matches('\\'));
        let w = wide(std::ffi::OsStr::new(&device));
        // SAFETY: `w` is NUL-terminated and outlives the call.
        let h = unsafe {
            CreateFileW(
                PCWSTR(w.as_ptr()),
                GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        }
        .map_err(|e| format!("open {device}: {e}"))?;
        Ok(Handle(h))
    }

    pub(in crate::watcher) fn query(vol: &Handle) -> Result<USN_JOURNAL_DATA_V0, String> {
        let mut data = USN_JOURNAL_DATA_V0::default();
        let mut got = 0u32;
        // SAFETY: `data` is a valid output buffer of the size passed.
        unsafe {
            DeviceIoControl(
                vol.0,
                FSCTL_QUERY_USN_JOURNAL,
                None,
                0,
                Some((&raw mut data).cast()),
                size_of::<USN_JOURNAL_DATA_V0>() as u32,
                Some(&mut got),
                None,
            )
        }
        .map_err(|e| format!("query USN journal: {e}"))?;
        Ok(data)
    }

    /// One position per root; `None` where the volume has no readable
    /// journal.
    pub(in crate::watcher::journal) fn current(roots: &[PathBuf]) -> Vec<Option<UsnPos>> {
        let mut by_vol: HashMap<String, Option<UsnPos>> = HashMap::new();
        roots
            .iter()
            .map(|r| {
                let v = volume_of(r)?.to_ascii_lowercase();
                *by_vol.entry(v.clone()).or_insert_with(|| {
                    let vol = open_volume(&v).ok()?;
                    let d = query(&vol).ok()?;
                    Some(UsnPos {
                        journal: d.UsnJournalID,
                        next: d.NextUsn,
                    })
                })
            })
            .collect()
    }

    fn read(vol: &Handle, journal: u64, from: i64, to: i64) -> Result<Vec<Record>, String> {
        let mut req = READ_USN_JOURNAL_DATA_V0 {
            StartUsn: from,
            ReasonMask: u32::MAX,
            ReturnOnlyOnClose: 0,
            Timeout: 0,
            BytesToWaitFor: 0,
            UsnJournalID: journal,
        };
        let mut buf = vec![0u8; READ_BUF];
        let mut records = Vec::new();
        while req.StartUsn < to {
            let mut got = 0u32;
            // SAFETY: `req` and `buf` are valid for the sizes passed and
            // outlive the synchronous call.
            let res = unsafe {
                DeviceIoControl(
                    vol.0,
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
                return Err(if e.code() == ERROR_JOURNAL_ENTRY_DELETED.to_hresult() {
                    "USN journal no longer holds the stored position".into()
                } else {
                    format!("read USN journal: {e}")
                });
            }
            let before = records.len();
            let next =
                parse(&buf[..got as usize], &mut records).ok_or("unexpected USN record format")?;
            if records.len() == before || next <= req.StartUsn {
                break;
            }
            req.StartUsn = next;
        }
        Ok(records)
    }

    pub(in crate::watcher) fn path_of(vol: &Handle, frn: u64) -> Option<String> {
        let id = FILE_ID_DESCRIPTOR {
            dwSize: size_of::<FILE_ID_DESCRIPTOR>() as u32,
            Type: FileIdType,
            Anonymous: FILE_ID_DESCRIPTOR_0 { FileId: frn as i64 },
        };
        // SAFETY: `id` is a valid descriptor that outlives the call.
        let h = unsafe {
            OpenFileById(
                vol.0,
                &id,
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                FILE_FLAG_BACKUP_SEMANTICS,
            )
        }
        .ok()?;
        let h = Handle(h);
        let mut buf = vec![0u16; 512];
        loop {
            // SAFETY: `buf` is a valid output slice.
            let n = unsafe {
                GetFinalPathNameByHandleW(
                    h.0,
                    &mut buf,
                    GETFINALPATHNAMEBYHANDLE_FLAGS(VOLUME_NAME_DOS.0 | FILE_NAME_NORMALIZED.0),
                )
            } as usize;
            if n == 0 {
                return None;
            }
            // On a too-small buffer the result is the size needed,
            // including the terminating NUL.
            if n < buf.len() {
                return Some(String::from_utf16_lossy(&buf[..n]));
            }
            buf.resize(n, 0);
        }
    }

    /// Changes under `roots` since `stored`, as `path -> flags` for `plan`.
    pub(in crate::watcher::journal) fn replay(
        roots: &[PathBuf],
        stored: &[Option<UsnPos>],
        now: &[Option<UsnPos>],
    ) -> Result<HashMap<PathBuf, u32>, String> {
        if stored.len() != roots.len() || now.len() != roots.len() {
            return Err("no USN position stored".into());
        }
        let prefixes: Vec<(String, PathBuf)> = roots
            .iter()
            .filter_map(|r| {
                let c = r.canonicalize().ok()?;
                Some((strip_verbatim(c.to_str()?).to_owned(), r.clone()))
            })
            .collect();
        if prefixes.len() != roots.len() {
            return Err("a watch root cannot be resolved".into());
        }
        let volumes = by_volume(roots, volume_of).ok_or("a watch root has no volume")?;
        let mut changed = HashMap::new();
        for (volume, idx) in volumes {
            let mut from = i64::MAX;
            let mut to = i64::MIN;
            let mut journal = None;
            for &i in &idx {
                let (Some(s), Some(n)) = (stored[i], now[i]) else {
                    return Err(format!("no USN journal on {volume}"));
                };
                if s.journal != n.journal || journal.is_some_and(|j| j != s.journal) {
                    return Err(format!("USN journal on {volume} was recreated"));
                }
                journal = Some(s.journal);
                from = from.min(s.next);
                to = to.max(n.next);
            }
            let Some(journal) = journal else { continue };
            let vol = open_volume(&volume)?;
            let data = query(&vol)?;
            if data.UsnJournalID != journal || from < data.FirstUsn {
                return Err(format!(
                    "USN journal on {volume} no longer reaches the stored position"
                ));
            }
            let records = read(&vol, journal, from, to)?;
            let part = fold(&records, |frn| {
                let p = path_of(&vol, frn)?;
                map_to_root(strip_verbatim(&p), &prefixes)
            })?;
            for (p, f) in part {
                *changed.entry(p).or_insert(0) |= f;
            }
        }
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(parent: u64, reason: u32, attrs: u32, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let name_len = units.len() * 2;
        let len = (60 + name_len).next_multiple_of(8);
        let mut r = vec![0u8; len];
        r[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        r[4..6].copy_from_slice(&2u16.to_le_bytes());
        r[8..16].copy_from_slice(&99u64.to_le_bytes());
        r[16..24].copy_from_slice(&parent.to_le_bytes());
        r[40..44].copy_from_slice(&reason.to_le_bytes());
        r[52..56].copy_from_slice(&attrs.to_le_bytes());
        r[56..58].copy_from_slice(&(name_len as u16).to_le_bytes());
        r[58..60].copy_from_slice(&60u16.to_le_bytes());
        for (i, u) in units.iter().enumerate() {
            r[60 + 2 * i..62 + 2 * i].copy_from_slice(&u.to_le_bytes());
        }
        r
    }

    #[test]
    fn parses_v2_records_and_rejects_others() {
        let mut buf = 777i64.to_le_bytes().to_vec();
        buf.extend(record(5, 0x100, 0, "a.txt"));
        buf.extend(record(
            6,
            REASON_RENAME_NEW_NAME,
            ATTRIBUTE_DIRECTORY,
            "dïr",
        ));
        let mut out = Vec::new();
        assert_eq!(parse(&buf, &mut out), Some(777));
        assert_eq!(
            out,
            vec![
                Record {
                    frn: 99,
                    parent: 5,
                    reason: 0x100,
                    is_dir: false,
                    name: "a.txt".into()
                },
                Record {
                    frn: 99,
                    parent: 6,
                    reason: REASON_RENAME_NEW_NAME,
                    is_dir: true,
                    name: "dïr".into()
                },
            ]
        );

        let mut v3 = buf.clone();
        v3[12..14].copy_from_slice(&3u16.to_le_bytes());
        assert_eq!(parse(&v3, &mut Vec::new()), None);
        assert_eq!(parse(&buf[..buf.len() - 1], &mut Vec::new()), None);
        assert_eq!(parse(&5i64.to_le_bytes(), &mut out), Some(5));
    }

    #[test]
    fn maps_resolved_paths_onto_configured_roots() {
        let prefixes = vec![(
            r"C:\Users\Me\Docs".to_owned(),
            PathBuf::from(r"c:\users\me\docs"),
        )];
        assert_eq!(
            map_to_root(r"C:\Users\Me\Docs", &prefixes),
            Some(PathBuf::from(r"c:\users\me\docs"))
        );
        assert!(map_to_root(r"c:\USERS\me\docs\x", &prefixes).is_some());
        assert_eq!(map_to_root(r"C:\Users\Me\Docs2", &prefixes), None);
        assert_eq!(map_to_root(r"D:\Users\Me\Docs", &prefixes), None);
        assert_eq!(strip_verbatim(r"\\?\C:\x"), r"C:\x");
    }

    #[test]
    fn fold_flags_moved_in_directories_and_skips_unresolvable_parents() {
        let recs = vec![
            Record {
                frn: 0,
                parent: 1,
                reason: 0x100,
                is_dir: false,
                name: "f".into(),
            },
            Record {
                frn: 0,
                parent: 1,
                reason: 0x2,
                is_dir: false,
                name: "f".into(),
            },
            Record {
                frn: 0,
                parent: 1,
                reason: REASON_RENAME_NEW_NAME,
                is_dir: true,
                name: "d".into(),
            },
            Record {
                frn: 0,
                parent: 1,
                reason: 0x200,
                is_dir: true,
                name: "gone".into(),
            },
            Record {
                frn: 0,
                parent: 2,
                reason: 0x200,
                is_dir: false,
                name: "orphan".into(),
            },
        ];
        let mut lookups = 0;
        let out = fold(&recs, |frn| {
            lookups += 1;
            (frn == 1).then(|| PathBuf::from("/w"))
        })
        .unwrap();
        assert_eq!(lookups, 2, "each parent is resolved once");
        assert_eq!(out.len(), 3);
        assert_eq!(out[Path::new("/w/f")], 0);
        assert_eq!(out[Path::new("/w/d")], ITEM_IS_DIR | MUST_SCAN_SUBDIRS);
        assert_eq!(out[Path::new("/w/gone")], ITEM_IS_DIR);
    }

    #[test]
    fn groups_roots_by_volume() {
        let roots = [PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c")];
        let g = by_volume(&roots, |p| {
            Some(if p == Path::new("b") { "D:\\" } else { "c:\\" }.into())
        })
        .unwrap();
        assert_eq!(
            g,
            vec![
                ("c:\\".to_owned(), vec![0, 2]),
                ("D:\\".to_owned(), vec![1])
            ]
        );
        assert!(by_volume(&roots, |_| None).is_none());
    }
}
