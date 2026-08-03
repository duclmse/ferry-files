use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::store::FileRecord;

// ---------------------------------------------------------------------------
// Platform helpers
// ---------------------------------------------------------------------------

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn is_hidden_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.starts_with('.'))
        .unwrap_or(false)
}

/// Build platform-specific attribute bitmask from filesystem metadata.
#[cfg_attr(windows, allow(unused_variables))]
pub fn attrib_from_metadata(meta: &std::fs::Metadata, path: &Path) -> u32 {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use crate::store::{
            ATTRIB_ARCHIVE, ATTRIB_COMPRESSED, ATTRIB_DIRECTORY, ATTRIB_ENCRYPTED,
            ATTRIB_HIDDEN, ATTRIB_NORMAL, ATTRIB_OFFLINE, ATTRIB_READONLY,
            ATTRIB_REPARSE_POINT, ATTRIB_SPARSE, ATTRIB_SYSTEM, ATTRIB_TEMPORARY,
        };
        let a = meta.file_attributes();
        let mut m: u32 = 0;
        if a & 0x0020 != 0 { m |= ATTRIB_ARCHIVE; }
        if a & 0x0800 != 0 { m |= ATTRIB_COMPRESSED; }
        if a & 0x0010 != 0 { m |= ATTRIB_DIRECTORY; }
        if a & 0x4000 != 0 { m |= ATTRIB_ENCRYPTED; }
        if a & 0x0002 != 0 { m |= ATTRIB_HIDDEN; }
        if a & 0x0400 != 0 { m |= ATTRIB_REPARSE_POINT; }
        if a & 0x0080 != 0 { m |= ATTRIB_NORMAL; }
        if a & 0x1000 != 0 { m |= ATTRIB_OFFLINE; }
        if a & 0x0200 != 0 { m |= ATTRIB_SPARSE; }
        if a & 0x0001 != 0 { m |= ATTRIB_READONLY; }
        if a & 0x0004 != 0 { m |= ATTRIB_SYSTEM; }
        if a & 0x0100 != 0 { m |= ATTRIB_TEMPORARY; }
        return m;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        use crate::store::{ATTRIB_HIDDEN, ATTRIB_READONLY};
        let mut m: u32 = 0;
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with('.'))
            .unwrap_or(false)
        {
            m |= ATTRIB_HIDDEN;
        }
        if meta.mode() & 0o200 == 0 {
            m |= ATTRIB_READONLY;
        }
        return m;
    }

    #[allow(unreachable_code)]
    {
        use crate::store::{ATTRIB_HIDDEN, ATTRIB_READONLY};
        let _ = (meta, path, ATTRIB_HIDDEN, ATTRIB_READONLY);
        0
    }
}

pub(super) fn build_file_record(path: &Path, meta: &std::fs::Metadata, is_hidden: bool, now: i64) -> FileRecord {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let ctime = {
        #[cfg(unix)]
        { use std::os::unix::fs::MetadataExt; meta.ctime() }
        #[cfg(windows)]
        {
            meta.created()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(mtime)
        }
        #[cfg(not(any(unix, windows)))]
        { mtime }
    };

    let is_dir    = meta.is_dir();
    let is_symlink = meta.file_type().is_symlink();
    let size_bytes = if is_dir { 0 } else { meta.len() };
    let name       = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_owned();
    let extension  = if is_dir {
        None
    } else {
        path.extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase())
    };

    FileRecord {
        id:              None,
        path:            path.to_string_lossy().into_owned(),
        name,
        extension,
        size_bytes,
        mtime,
        ctime,
        is_dir,
        is_hidden,
        is_symlink,
        attrib_bitmask:  attrib_from_metadata(meta, path),
        content_indexed: false,
        tantivy_id:      None,
        indexed_at:      now,
        archive_path:    None,
    }
}
