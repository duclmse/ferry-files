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

/// Maps Windows `FILE_ATTRIBUTE_*` bits onto the store's own bitmask.
pub(super) fn attrib_from_windows_flags(a: u32) -> u32 {
    use crate::store::{
        ATTRIB_ARCHIVE, ATTRIB_COMPRESSED, ATTRIB_DIRECTORY, ATTRIB_ENCRYPTED, ATTRIB_HIDDEN,
        ATTRIB_NORMAL, ATTRIB_OFFLINE, ATTRIB_READONLY, ATTRIB_REPARSE_POINT, ATTRIB_SPARSE,
        ATTRIB_SYSTEM, ATTRIB_TEMPORARY,
    };
    const MAP: [(u32, u32); 12] = [
        (0x0020, ATTRIB_ARCHIVE),
        (0x0800, ATTRIB_COMPRESSED),
        (0x0010, ATTRIB_DIRECTORY),
        (0x4000, ATTRIB_ENCRYPTED),
        (0x0002, ATTRIB_HIDDEN),
        (0x0400, ATTRIB_REPARSE_POINT),
        (0x0080, ATTRIB_NORMAL),
        (0x1000, ATTRIB_OFFLINE),
        (0x0200, ATTRIB_SPARSE),
        (0x0001, ATTRIB_READONLY),
        (0x0004, ATTRIB_SYSTEM),
        (0x0100, ATTRIB_TEMPORARY),
    ];
    MAP.iter()
        .filter(|(windows, _)| a & windows != 0)
        .fold(0, |mask, (_, ours)| mask | ours)
}

/// Build platform-specific attribute bitmask from filesystem metadata.
#[cfg_attr(windows, allow(unused_variables))]
pub fn attrib_from_metadata(meta: &std::fs::Metadata, path: &Path) -> u32 {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        return attrib_from_windows_flags(meta.file_attributes());
    }

    #[cfg(unix)]
    {
        use crate::store::{ATTRIB_HIDDEN, ATTRIB_READONLY};
        use std::os::unix::fs::MetadataExt;
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

pub(super) fn record_from_mft(
    path: &Path,
    file: &crate::indexer::mft::MftFile,
    is_hidden: bool,
    now: i64,
) -> FileRecord {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
    let extension = if file.is_dir {
        None
    } else {
        path.extension().map(|e| e.to_string_lossy().to_lowercase())
    };
    // On an NTFS directory, FILE_ATTRIBUTE_DIRECTORY is implied by the record
    // flag rather than stored in $STANDARD_INFORMATION.
    let directory_bit = if file.is_dir { 0x0010 } else { 0 };
    FileRecord {
        id: None,
        path: path.to_string_lossy().into_owned(),
        name: name.unwrap_or_default(),
        extension,
        size_bytes: file.size,
        mtime: file.mtime,
        ctime: file.ctime,
        is_dir: file.is_dir,
        is_hidden,
        is_symlink: file.is_symlink,
        attrib_bitmask: attrib_from_windows_flags(file.attributes | directory_bit),
        content_indexed: false,
        tantivy_id: None,
        indexed_at: now,
        archive_path: None,
    }
}

pub(super) fn build_file_record(
    path: &Path,
    meta: &std::fs::Metadata,
    is_hidden: bool,
    now: i64,
) -> FileRecord {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let ctime = {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            meta.ctime()
        }
        #[cfg(windows)]
        {
            meta.created()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(mtime)
        }
        #[cfg(not(any(unix, windows)))]
        {
            mtime
        }
    };

    let is_dir = meta.is_dir();
    let is_symlink = meta.file_type().is_symlink();
    let size_bytes = if is_dir { 0 } else { meta.len() };
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_owned();
    let extension = if is_dir {
        None
    } else {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
    };

    FileRecord {
        id: None,
        path: path.to_string_lossy().into_owned(),
        name,
        extension,
        size_bytes,
        mtime,
        ctime,
        is_dir,
        is_hidden,
        is_symlink,
        attrib_bitmask: attrib_from_metadata(meta, path),
        content_indexed: false,
        tantivy_id: None,
        indexed_at: now,
        archive_path: None,
    }
}
