//! Shared conversion helpers for API response DTOs.
//!
//! Both REST and gRPC map `FileRecord` → their own wire types. The archive
//! entry extraction and archive path handling live here once.

use crate::store::FileRecord;

/// Extract the inner-archive entry name from a virtual path.
///
/// For `/a/archive.zip!/src/main.rs`, returns `"src/main.rs"`.
/// Returns an empty string for real filesystem files.
pub fn archive_entry(r: &FileRecord) -> String {
    match r.archive_path {
        None => String::new(),
        Some(_) => r
            .path
            .find("!/")
            .map(|i| r.path[i + 2..].to_owned())
            .unwrap_or_default(),
    }
}

/// Same as `archive_entry` but returns `None` for non-archive files.
/// Preferred for REST responses that use `Option<String>`.
pub fn archive_entry_opt(r: &FileRecord) -> Option<String> {
    r.archive_path.as_ref().map(|_| {
        r.path
            .find("!/")
            .map(|i| r.path[i + 2..].to_owned())
            .unwrap_or_default()
    })
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn real_record(path: &str) -> FileRecord {
        FileRecord {
            id: None,
            path: path.into(),
            name: "f".into(),
            extension: None,
            size_bytes: 0,
            mtime: 0,
            ctime: 0,
            is_dir: false,
            is_hidden: false,
            is_symlink: false,
            attrib_bitmask: 0,
            content_indexed: false,
            tantivy_id: None,
            indexed_at: 0,
            archive_path: None,
        }
    }

    fn zip_record(path: &str, archive_path: &str) -> FileRecord {
        FileRecord {
            archive_path: Some(archive_path.into()),
            ..real_record(path)
        }
    }

    // ── archive_entry ─────────────────────────────────────────────────────────

    #[test]
    fn archive_entry_real_file_returns_empty() {
        let r = real_record("/some/file.txt");
        assert_eq!(archive_entry(&r), "");
    }

    #[test]
    fn archive_entry_extracts_inner_path() {
        let r = zip_record("/a/archive.zip!/src/main.rs", "/a/archive.zip");
        assert_eq!(archive_entry(&r), "src/main.rs");
    }

    #[test]
    fn archive_entry_missing_separator_returns_empty() {
        // archive_path set but virtual path has no !/ - defensive fallback
        let r = zip_record("/a/archive.zip", "/a/archive.zip");
        assert_eq!(archive_entry(&r), "");
    }

    // ── archive_entry_opt ─────────────────────────────────────────────────────

    #[test]
    fn archive_entry_opt_real_file_returns_none() {
        let r = real_record("/real.rs");
        assert_eq!(archive_entry_opt(&r), None);
    }

    #[test]
    fn archive_entry_opt_zip_entry_returns_some() {
        let r = zip_record("/a/arch.zip!/inner/lib.rs", "/a/arch.zip");
        assert_eq!(archive_entry_opt(&r), Some("inner/lib.rs".into()));
    }

    #[test]
    fn archive_entry_opt_missing_separator_returns_some_empty() {
        let r = zip_record("/a/arch.zip", "/a/arch.zip");
        assert_eq!(archive_entry_opt(&r), Some(String::new()));
    }
}
