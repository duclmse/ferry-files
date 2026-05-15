//! `.zidx` index builder and mmap-backed reader.
//!
//! # Binary format (version 2)
//!
//! ```text
//! [Header — 20 bytes]
//! [Node Table — node_count × 48 bytes]
//! [String Pool — string_bytes bytes, UTF-8]
//! ```
//!
//! All integers are little-endian on disk.  Version 1 (40-byte nodes with
//! u32 sizes) is rejected at open time; rebuild with the current builder.

pub mod builder;
pub mod reader;

pub use builder::ZidxBuilder;
pub use reader::{ChildIter, NodeData, ZidxIndex};

use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------

#[derive(thiserror::Error, Debug)]
pub enum ZidxError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("ZIP error: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("Archive too large: {0} entries exceeds u32::MAX")]
    TooLarge(usize),
    #[error("Invalid .zidx file: {0}")]
    Corrupt(String),
}

// ---------------------------------------------------------------------------

/// Map a ZIP path to a collision-free `.zidx` sidecar path under `zidx_dir`.
///
/// Uses blake3(zip_path_as_str) as the filename stem so paths stay flat and
/// never collide regardless of the ZIP's directory depth.
pub fn zidx_path_for(zidx_dir: &Path, zip_path: &Path) -> PathBuf {
    let key  = zip_path.to_string_lossy();
    let hash = blake3::hash(key.as_bytes());
    zidx_dir.join(format!("{}.zidx", hash.to_hex()))
}
