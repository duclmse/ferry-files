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
        None    => String::new(),
        Some(_) => r.path.find("!/")
            .map(|i| r.path[i + 2..].to_owned())
            .unwrap_or_default(),
    }
}

/// Same as `archive_entry` but returns `None` for non-archive files.
/// Preferred for REST responses that use `Option<String>`.
pub fn archive_entry_opt(r: &FileRecord) -> Option<String> {
    r.archive_path.as_ref().map(|_| {
        r.path.find("!/")
            .map(|i| r.path[i + 2..].to_owned())
            .unwrap_or_default()
    })
}
