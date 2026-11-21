//! Everything file lists (`.efu`, see `fs_protocol::efu`) as searchable
//! entries: catalogues of drives, discs and machines that are not here.
//!
//! An entry keeps the path the list gives, so it reads like the real file in
//! results. Nothing in the store marks it as a list's, so each list's paths
//! are kept in `data_dir/file_lists/`; when the list changes or leaves the
//! config, that is how its old entries are found and removed. A list is
//! re-read only when its size, modification time or the watch paths change.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use fs_protocol::{attrib, efu};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, Result};
use crate::store::{FileRecord, FileStore};

const DIR: &str = "file_lists";
const BATCH: usize = 10_000;

/// What one list's entries were built from.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Stamp {
    source: PathBuf,
    /// Size, modification time and the watch paths: rows under a watch path
    /// are left out, so a change there changes which rows load.
    version: String,
    entries: usize,
}

#[derive(Debug, Default)]
pub struct Report {
    /// Lists read again: (list, entries now held, rows skipped, first problem).
    pub loaded: Vec<Loaded>,
    /// Lists that are configured but cannot be read; their entries stay.
    pub unreadable: Vec<(PathBuf, String)>,
    /// Lists that left the config, and how many entries went with them.
    pub dropped: Vec<(PathBuf, usize)>,
}

#[derive(Debug)]
pub struct Loaded {
    pub list: PathBuf,
    pub entries: usize,
    pub removed: usize,
    pub under_watch_paths: usize,
    pub skipped: usize,
    pub first_problem: Option<String>,
}

/// Brings the store in line with the configured lists. Blocking.
pub fn sync(
    store: &FileStore,
    lists: &[PathBuf],
    watch_paths: &[PathBuf],
    data_dir: &Path,
) -> Result<Report> {
    let dir = data_dir.join(DIR);
    if lists.is_empty() && !dir.exists() {
        return Ok(Report::default());
    }
    std::fs::create_dir_all(&dir).map_err(AppError::Io)?;
    let roots: Vec<String> = watch_paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let mut report = Report::default();

    for (key, stamp) in stamps(&dir)? {
        if lists.contains(&stamp.source) {
            continue;
        }
        let old = read_paths(&dir, &key)?;
        let others = owned_by_others(&dir, &key)?;
        let removes: Vec<String> = old
            .into_iter()
            .filter(|p| !others.contains(p) && !under_any(p, &roots))
            .collect();
        remove(store, &removes)?;
        // Saved before the record of this list goes: otherwise a crash could
        // bring the memory store's entries back with nothing left to own them.
        store.flush_to_disk(data_dir)?;
        forget(&dir, &key)?;
        report.dropped.push((stamp.source, removes.len()));
    }

    for list in lists {
        let key = key_of(list);
        let version = match version_of(list, &roots) {
            Ok(v) => v,
            Err(e) => {
                report.unreadable.push((list.clone(), e.to_string()));
                continue;
            }
        };
        let current = read_stamp(&dir, &key);
        if current.as_ref().is_some_and(|s| s.version == version) {
            continue;
        }
        let parsed = match std::fs::read(list)
            .map_err(|e| e.to_string())
            .and_then(|b| String::from_utf8(b).map_err(|_| "not UTF-8".to_owned()))
            .and_then(|text| efu::parse(&text))
        {
            Ok(parsed) => parsed,
            Err(e) => {
                report.unreadable.push((list.clone(), e));
                continue;
            }
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let mut under = 0;
        let mut records: Vec<FileRecord> = Vec::with_capacity(parsed.entries.len());
        for entry in &parsed.entries {
            let record = to_record(entry, now);
            if under_any(&record.path, &roots) {
                under += 1;
            } else {
                records.push(record);
            }
        }
        // Parents before children, so the memory store can share their paths.
        records.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        records.dedup_by(|a, b| a.path == b.path);
        let fresh: HashSet<&str> = records.iter().map(|r| r.path.as_str()).collect();

        let gone: Vec<String> = match current {
            Some(_) => read_paths(&dir, &key)?
                .into_iter()
                .filter(|p| !fresh.contains(p.as_str()))
                .collect(),
            None => Vec::new(),
        };
        let removes: Vec<String> = if gone.is_empty() {
            gone
        } else {
            let others = owned_by_others(&dir, &key)?;
            gone.into_iter()
                .filter(|p| !others.contains(p) && !under_any(p, &roots))
                .collect()
        };
        remove(store, &removes)?;
        for chunk in records.chunks(BATCH) {
            store.apply_changes(chunk, &[])?;
        }
        // The stamp says these entries are in the store, which for the memory
        // store is true only once they are saved.
        store.flush_to_disk(data_dir)?;
        write_paths(&dir, &key, records.iter().map(|r| r.path.as_str()))?;
        write_stamp(
            &dir,
            &key,
            &Stamp {
                source: list.clone(),
                version,
                entries: records.len(),
            },
        )?;
        report.loaded.push(Loaded {
            list: list.clone(),
            entries: records.len(),
            removed: removes.len(),
            under_watch_paths: under,
            skipped: parsed.skipped,
            first_problem: parsed.first_problem,
        });
    }
    Ok(report)
}

fn to_record(e: &efu::Entry, now: i64) -> FileRecord {
    let path = trim_separators(&e.path).to_owned();
    let name = match path.rfind(['/', '\\']) {
        Some(i) if i + 1 < path.len() => path[i + 1..].to_owned(),
        _ => path.clone(),
    };
    let windows = e.attributes.unwrap_or(0);
    let is_dir = e.is_dir();
    // As the crawler stores it: lowercased, none for folders and dot-files.
    let extension = match name.rsplit_once('.') {
        Some((stem, ext)) if !is_dir && !stem.is_empty() => Some(ext.to_lowercase()),
        _ => None,
    };
    FileRecord {
        id: None,
        is_hidden: windows & 0x2 != 0 || name.starts_with('.'),
        is_symlink: windows & 0x400 != 0,
        attrib_bitmask: attrib::from_windows(windows),
        name,
        extension,
        size_bytes: if is_dir { 0 } else { e.size.unwrap_or(0) },
        mtime: e.mtime.unwrap_or(0).max(0),
        ctime: e.ctime.unwrap_or(0).max(0),
        is_dir,
        content_indexed: false,
        tantivy_id: None,
        indexed_at: now,
        archive_path: None,
        path,
    }
}

/// `D:\Photos\` is `D:\Photos`, and `D:\` is `D:`, which is how the store
/// names a drive's top folder as a parent.
fn trim_separators(path: &str) -> &str {
    let trimmed = path.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        &path[..path.len().min(1)]
    } else {
        trimmed
    }
}

/// At or below `root`, ignoring ASCII case, with either separator: the
/// crawl owns those paths.
fn under_any(path: &str, roots: &[String]) -> bool {
    roots.iter().any(|root| {
        let root = root.trim_end_matches(['/', '\\']).as_bytes();
        let p = path.as_bytes();
        p.len() >= root.len()
            && p[..root.len()].eq_ignore_ascii_case(root)
            && (p.len() == root.len() || matches!(p[root.len()], b'/' | b'\\') || root.is_empty())
    })
}

fn remove(store: &FileStore, paths: &[String]) -> Result<()> {
    for chunk in paths.chunks(BATCH) {
        store.apply_changes(&[], chunk)?;
    }
    Ok(())
}

fn version_of(list: &Path, roots: &[String]) -> std::io::Result<String> {
    let meta = std::fs::metadata(list)?;
    let mtime = meta
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let roots = blake3::hash(roots.join("\n").as_bytes()).to_hex();
    Ok(format!("{}:{mtime}:{}", meta.len(), &roots[..16]))
}

fn key_of(list: &Path) -> String {
    blake3::hash(list.to_string_lossy().as_bytes()).to_hex()[..32].to_owned()
}

fn stamp_file(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("{key}.json"))
}

fn paths_file(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("{key}.paths.zst"))
}

fn read_stamp(dir: &Path, key: &str) -> Option<Stamp> {
    let text = std::fs::read_to_string(stamp_file(dir, key)).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_stamp(dir: &Path, key: &str, stamp: &Stamp) -> Result<()> {
    let json = serde_json::to_string_pretty(stamp).map_err(|e| AppError::Index(e.to_string()))?;
    write_atomic(&stamp_file(dir, key), json.as_bytes())
}

/// Every stamp in `dir`, by key. A stamp that does not parse is skipped:
/// its list, if still configured, is then simply read again.
fn stamps(dir: &Path) -> Result<Vec<(String, Stamp)>> {
    let mut out = Vec::new();
    for item in std::fs::read_dir(dir).map_err(AppError::Io)? {
        let name = item.map_err(AppError::Io)?.file_name();
        let Some(key) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
            continue;
        };
        if let Some(stamp) = read_stamp(dir, key) {
            out.push((key.to_owned(), stamp));
        }
    }
    Ok(out)
}

/// Paths are NUL-separated: a Unix name may hold a line break, never a NUL.
fn read_paths(dir: &Path, key: &str) -> Result<Vec<String>> {
    let file = match std::fs::File::open(paths_file(dir, key)) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(AppError::Io(e)),
    };
    let bytes = zstd::decode_all(file).map_err(AppError::Io)?;
    let text = String::from_utf8(bytes)
        .map_err(|_| AppError::Index(format!("{DIR}/{key}: paths are not UTF-8")))?;
    Ok(text
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect())
}

fn write_paths<'a>(dir: &Path, key: &str, paths: impl Iterator<Item = &'a str>) -> Result<()> {
    let mut raw = Vec::new();
    for p in paths {
        raw.extend_from_slice(p.as_bytes());
        raw.push(0);
    }
    let packed = zstd::encode_all(&raw[..], 3).map_err(AppError::Io)?;
    write_atomic(&paths_file(dir, key), &packed)
}

fn owned_by_others(dir: &Path, key: &str) -> Result<HashSet<String>> {
    let mut owned = HashSet::new();
    for (other, _) in stamps(dir)? {
        if other != key {
            owned.extend(read_paths(dir, &other)?);
        }
    }
    Ok(owned)
}

fn forget(dir: &Path, key: &str) -> Result<()> {
    for file in [stamp_file(dir, key), paths_file(dir, key)] {
        match std::fs::remove_file(&file) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(AppError::Io(e)),
            _ => {}
        }
    }
    Ok(())
}

/// The stamp is written last, so a crash before it leaves the old version
/// and the list is read again.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(AppError::Io)?;
    std::fs::rename(&tmp, path).map_err(AppError::Io)
}

#[cfg(test)]
mod tests;
