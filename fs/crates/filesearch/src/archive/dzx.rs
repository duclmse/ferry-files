//! Deflate random-access index (`.dzx`) - block builder and mmap-backed reader.
//!
//! # Binary format
//!
//! ```text
//! [Header - 32 bytes]
//! [Block Table - block_count × 56 bytes]
//! [Window Snapshots - block_count × 32768 bytes]
//! ```
//!
//! All integers are little-endian. See `docs/phases/11-deflate-random-access.md`.

use std::io;
use std::path::{Path, PathBuf};

use bytes::{Bytes, BytesMut};

use crate::archive::{parse_local_header_offset, ByteSource, EntryMeta};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const MAGIC: &[u8; 4] = b"DZX1";
const VERSION: u32 = 1;
const HEADER_SIZE: usize = 32;
const BLOCK_ENTRY_SIZE: usize = 56;
const WINDOW_SIZE: usize = 32_768;
pub const DEFAULT_BLOCK_SIZE: u32 = 65_536;

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(thiserror::Error, Debug)]
pub enum DzxError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("Inflate error: {0}")]
    Inflate(String),
    #[error("Unsupported compression method {0}; only deflate (8) is indexed")]
    UnsupportedMethod(u16),
    #[error("Invalid .dzx file: {0}")]
    Corrupt(String),
}

mod builder;
mod reader;
#[cfg(test)]
mod tests;

pub use builder::DzxBuilder;
pub use reader::{read_range, DzxBlockData, DzxIndex};

// ---------------------------------------------------------------------------
// Path convention
// ---------------------------------------------------------------------------

/// `{dzx_dir}/blake3(zip_path + "#" + local_header_offset).dzx`
pub fn dzx_path_for(dzx_dir: &Path, zip_path: &Path, local_header_offset: u64) -> PathBuf {
    let key  = format!("{}#{}", zip_path.to_string_lossy(), local_header_offset);
    let hash = blake3::hash(key.as_bytes());
    dzx_dir.join(format!("{}.dzx", hash.to_hex()))
}
