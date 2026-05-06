use std::collections::HashMap;
use std::io::{BufReader, Read};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::{ArchiveIndexConfig, ContentConfig};
use crate::error::{AppError, Result};
use crate::store::FileRecord;
use super::content::extract_content_from_bytes;

// ---------------------------------------------------------------------------

pub struct ArchiveCrawler<'cfg> {
    config:         &'cfg ArchiveIndexConfig,
    content_config: &'cfg ContentConfig,
}

pub struct ArchiveEntry {
    pub virtual_path:       String,
    pub outer_archive_path: String,
    pub entry_name:         String,
    pub extension:          Option<String>,
    pub size_bytes:         u64,
    pub mtime:              i64,
    pub is_dir:             bool,
    pub content:            Option<String>,
}

impl<'cfg> ArchiveCrawler<'cfg> {
    pub fn new(config: &'cfg ArchiveIndexConfig, content_config: &'cfg ContentConfig) -> Self {
        Self { config, content_config }
    }

    /// Stream all indexable entries from a ZIP file. Never writes to disk.
    /// Returns an iterator over results so the caller can handle errors per-entry.
    pub fn crawl(
        &self,
        archive_path: &Path,
        nesting_depth: u8,
    ) -> Result<impl Iterator<Item = Result<ArchiveEntry>>> {
        let file = std::fs::File::open(archive_path).map_err(AppError::Io)?;
        let reader = BufReader::with_capacity(65536, file);
        let mut archive = zip::ZipArchive::new(reader).map_err(|e| {
            AppError::Content(format!("zip open {}: {}", archive_path.display(), e))
        })?;

        let total = archive.len();
        let effective = if self.config.max_entries_per_archive == 0 {
            total
        } else {
            total.min(self.config.max_entries_per_archive)
        };

        if effective < total {
            tracing::warn!(
                "archive {} has {} entries; limiting to {}",
                archive_path.display(),
                total,
                effective
            );
        }

        let archive_str = archive_path.to_string_lossy().into_owned();
        let passwords    = self.config.passwords.clone();
        let inner_limits = self.config.inner_extension_limits.clone();
        let outer_limits = self.content_config.extension_limits.clone();
        let _depth       = nesting_depth;

        let mut entries: Vec<Result<ArchiveEntry>> = Vec::with_capacity(effective);
        for i in 0..effective {
            entries.push(process_entry(
                &mut archive,
                i,
                &archive_str,
                &passwords,
                &inner_limits,
                &outer_limits,
            ));
        }

        Ok(entries.into_iter())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn normalize_name(raw: &str) -> String {
    raw.replace('\\', "/")
}

fn entry_extension(name: &str) -> Option<String> {
    let stem = name.trim_end_matches('/');
    Path::new(stem)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
}

fn dos_to_unix(dt: Option<zip::DateTime>) -> i64 {
    let Some(dt) = dt else { return 0; };
    chrono::NaiveDate::from_ymd_opt(dt.year() as i32, dt.month() as u32, dt.day() as u32)
        .and_then(|d| {
            d.and_hms_opt(dt.hour() as u32, dt.minute() as u32, dt.second() as u32)
        })
        .map(|ndt| ndt.and_utc().timestamp())
        .unwrap_or(0)
}

fn resolve_limit(
    ext: &Option<String>,
    inner: &HashMap<String, u64>,
    outer: &HashMap<String, u64>,
) -> u64 {
    if let Some(e) = ext {
        if let Some(&v) = inner.get(e.as_str()) { return v; }
        if let Some(&v) = outer.get(e.as_str()) { return v; }
    }
    if let Some(&v) = inner.get("*") { return v; }
    if let Some(&v) = outer.get("*") { return v; }
    0
}

// ---------------------------------------------------------------------------

fn process_entry<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    index:   usize,
    archive_str: &str,
    passwords:   &[String],
    inner_limits: &HashMap<String, u64>,
    outer_limits: &HashMap<String, u64>,
) -> Result<ArchiveEntry> {
    // Phase 1: metadata only (borrow ends before re-access)
    let (entry_name, size_bytes, mtime, is_dir, encrypted) = {
        let f = archive.by_index(index).map_err(|e| {
            AppError::Content(format!("entry {index} in {archive_str}: {e}"))
        })?;
        (
            normalize_name(f.name()),
            f.size(),
            dos_to_unix(f.last_modified()),
            f.is_dir(),
            f.encrypted(),
        )
    };

    let extension   = entry_extension(&entry_name);
    let virtual_path = format!("{archive_str}!/{entry_name}");

    if is_dir {
        return Ok(ArchiveEntry {
            virtual_path,
            outer_archive_path: archive_str.to_owned(),
            entry_name,
            extension,
            size_bytes,
            mtime,
            is_dir: true,
            content: None,
        });
    }

    let limit = resolve_limit(&extension, inner_limits, outer_limits);
    let content = if limit == 0 || size_bytes == 0 {
        None
    } else if encrypted {
        read_encrypted(archive, index, archive_str, &entry_name, passwords, limit, size_bytes)
    } else {
        read_plain(archive, index, limit, size_bytes)
    };

    Ok(ArchiveEntry {
        virtual_path,
        outer_archive_path: archive_str.to_owned(),
        entry_name,
        extension,
        size_bytes,
        mtime,
        is_dir: false,
        content,
    })
}

fn read_plain<R: Read + std::io::Seek>(
    archive:    &mut zip::ZipArchive<R>,
    index:      usize,
    limit:      u64,
    size_bytes: u64,
) -> Option<String> {
    let mut f = archive.by_index(index).ok()?;
    let cap = limit.min(size_bytes) as usize;
    let mut buf = Vec::with_capacity(cap);
    let _ = f.by_ref().take(cap as u64).read_to_end(&mut buf);
    extract_content_from_bytes(&buf)
}

fn read_encrypted<R: Read + std::io::Seek>(
    archive:    &mut zip::ZipArchive<R>,
    index:      usize,
    archive_str: &str,
    entry_name:  &str,
    passwords:   &[String],
    limit:       u64,
    size_bytes:  u64,
) -> Option<String> {
    if passwords.is_empty() {
        tracing::warn!(
            "encrypted entry {entry_name} in {archive_str} — no passwords configured, skipping"
        );
        return None;
    }
    for pw in passwords {
        if let Ok(mut f) = archive.by_index_decrypt(index, pw.as_bytes()) {
            let cap = limit.min(size_bytes) as usize;
            let mut buf = Vec::with_capacity(cap);
            if f.by_ref().take(cap as u64).read_to_end(&mut buf).is_ok() {
                return extract_content_from_bytes(&buf);
            }
        }
    }
    tracing::warn!(
        "encrypted entry {entry_name} in {archive_str} — all {} passwords failed",
        passwords.len()
    );
    None
}

// ---------------------------------------------------------------------------

/// Convert an `ArchiveEntry` into a `FileRecord` for SQLite storage.
pub fn archive_entry_to_record(entry: &ArchiveEntry) -> FileRecord {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let name = entry.entry_name
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(&entry.entry_name)
        .to_owned();

    FileRecord {
        id:              None,
        path:            entry.virtual_path.clone(),
        name,
        extension:       entry.extension.clone(),
        size_bytes:      entry.size_bytes,
        mtime:           entry.mtime,
        ctime:           entry.mtime, // ZIP has no ctime
        is_dir:          entry.is_dir,
        is_hidden:       false,
        is_symlink:      false,
        attrib_bitmask:  0,
        content_indexed: entry.content.is_some(),
        tantivy_id:      None,
        indexed_at:      now,
        archive_path:    Some(entry.outer_archive_path.clone()),
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ArchiveIndexConfig, ContentConfig};
    use std::io::Write;
    use tempfile::NamedTempFile;
    use zip::write::{FileOptions, ZipWriter};
    use zip::CompressionMethod;

    fn make_zip_with_entries(entries: &[(&str, &[u8])]) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        let mut zw = ZipWriter::new(std::io::BufWriter::new(&mut f));
        let opts = FileOptions::<()>::default().compression_method(CompressionMethod::Stored);
        for (name, data) in entries {
            zw.start_file(*name, opts).unwrap();
            zw.write_all(data).unwrap();
        }
        zw.finish().unwrap();
        f
    }

    fn default_cfg() -> (ArchiveIndexConfig, ContentConfig) {
        (
            ArchiveIndexConfig {
                enabled:                 true,
                formats:                 vec!["zip".into()],
                inner_extension_limits:  [("txt".to_string(), 65536)].into(),
                max_entries_per_archive: 10_000,
                max_nesting_depth:       0,
                passwords:               vec![],
            },
            ContentConfig {
                extension_limits: [("*".to_string(), 0)].into(),
            },
        )
    }

    #[test]
    fn crawl_five_text_entries() {
        let entries = [
            ("a.txt", b"alpha".as_slice()),
            ("b.txt", b"beta"),
            ("c.txt", b"gamma"),
            ("d.txt", b"delta"),
            ("e.txt", b"epsilon"),
        ];
        let zip_file = make_zip_with_entries(&entries);
        let (arc_cfg, cnt_cfg) = default_cfg();
        let crawler = ArchiveCrawler::new(&arc_cfg, &cnt_cfg);
        let results: Vec<_> = crawler.crawl(zip_file.path(), 0).unwrap().collect();
        assert_eq!(results.len(), 5);
        let paths: Vec<_> = results.iter().map(|r| r.as_ref().unwrap().entry_name.clone()).collect();
        assert!(paths.contains(&"a.txt".to_string()));
    }

    #[test]
    fn directory_entries_have_is_dir_true() {
        let mut f = NamedTempFile::new().unwrap();
        {
            let mut zw = ZipWriter::new(std::io::BufWriter::new(&mut f));
            let opts = FileOptions::<()>::default().compression_method(CompressionMethod::Stored);
            zw.add_directory("subdir/", opts).unwrap();
            zw.start_file("subdir/file.txt", opts).unwrap();
            zw.write_all(b"content").unwrap();
            zw.finish().unwrap();
        }
        let (arc_cfg, cnt_cfg) = default_cfg();
        let crawler = ArchiveCrawler::new(&arc_cfg, &cnt_cfg);
        let results: Vec<_> = crawler.crawl(f.path(), 0).unwrap().collect();
        let dir_entry = results
            .iter()
            .find(|r| r.as_ref().unwrap().is_dir)
            .unwrap()
            .as_ref()
            .unwrap();
        assert!(dir_entry.is_dir);
        assert!(dir_entry.content.is_none());
    }

    #[test]
    fn max_entries_per_archive_limits_results() {
        let entries: Vec<(String, Vec<u8>)> =
            (0..20).map(|i| (format!("{i}.txt"), b"x".to_vec())).collect();
        let entry_refs: Vec<(&str, &[u8])> = entries.iter().map(|(n, d)| (n.as_str(), d.as_slice())).collect();
        let zip_file = make_zip_with_entries(&entry_refs);

        let mut arc_cfg = default_cfg().0;
        arc_cfg.max_entries_per_archive = 5;
        let cnt_cfg = default_cfg().1;
        let crawler = ArchiveCrawler::new(&arc_cfg, &cnt_cfg);
        let results: Vec<_> = crawler.crawl(zip_file.path(), 0).unwrap().collect();
        assert_eq!(results.len(), 5);
    }

    #[test]
    fn binary_entry_content_is_none() {
        let binary = vec![0u8; 200]; // all-null → binary
        let zip_file = make_zip_with_entries(&[("img.png", &binary)]);
        let mut arc_cfg = default_cfg().0;
        arc_cfg.inner_extension_limits = [("png".to_string(), 65536)].into();
        let cnt_cfg = default_cfg().1;
        let crawler = ArchiveCrawler::new(&arc_cfg, &cnt_cfg);
        let results: Vec<_> = crawler.crawl(zip_file.path(), 0).unwrap().collect();
        let entry = results[0].as_ref().unwrap();
        assert!(entry.content.is_none());
        assert_eq!(entry.size_bytes, 200);
    }

    #[test]
    fn virtual_path_format_correct() {
        let zip_file = make_zip_with_entries(&[("src/main.rs", b"fn main(){}")]);
        let (arc_cfg, cnt_cfg) = default_cfg();
        let mut arc_cfg = arc_cfg;
        arc_cfg.inner_extension_limits = [("rs".to_string(), 65536)].into();
        let crawler = ArchiveCrawler::new(&arc_cfg, &cnt_cfg);
        let results: Vec<_> = crawler.crawl(zip_file.path(), 0).unwrap().collect();
        let entry = results[0].as_ref().unwrap();
        let expected_prefix = format!("{}!/src/main.rs", zip_file.path().display());
        assert_eq!(entry.virtual_path, expected_prefix);
    }

    #[test]
    fn archive_entry_to_record_sets_archive_path() {
        let entry = ArchiveEntry {
            virtual_path:       "/data/a.zip!/foo.txt".into(),
            outer_archive_path: "/data/a.zip".into(),
            entry_name:         "foo.txt".into(),
            extension:          Some("txt".into()),
            size_bytes:         42,
            mtime:              0,
            is_dir:             false,
            content:            Some("hello".into()),
        };
        let rec = archive_entry_to_record(&entry);
        assert_eq!(rec.archive_path, Some("/data/a.zip".into()));
        assert_eq!(rec.path, "/data/a.zip!/foo.txt");
        assert!(rec.content_indexed);
        assert!(!rec.is_hidden);
        assert!(!rec.is_symlink);
    }
}
