use std::io::{self, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use memmap2::Mmap;

/// Async random-access read over a byte range.
/// All implementations must be Send + Sync.
#[async_trait]
pub trait ByteSource: Send + Sync {
    async fn read_range(&self, offset: u64, len: usize) -> io::Result<Bytes>;

    /// Total byte length of the underlying resource, if known.
    fn size_hint(&self) -> Option<u64> {
        None
    }
}

// ---------------------------------------------------------------------------
// LocalFileSource
// ---------------------------------------------------------------------------

/// Holds an open file descriptor and serves byte ranges via seek+read inside
/// `spawn_blocking`.  Each `read_range` call calls `try_clone()` on the inner
/// `File` (a cheap OS-level `dup`) so the blocking read does not hold the
/// Mutex for its duration — concurrent reads proceed in parallel.
pub struct LocalFileSource {
    file: Mutex<std::fs::File>,
    size: u64,
}

impl LocalFileSource {
    pub fn new(path: PathBuf) -> io::Result<Self> {
        let file = std::fs::File::open(&path)?;
        let size = file.metadata()?.len();
        Ok(Self { file: Mutex::new(file), size })
    }
}

#[async_trait]
impl ByteSource for LocalFileSource {
    async fn read_range(&self, offset: u64, len: usize) -> io::Result<Bytes> {
        // dup the fd while holding the lock for only this one call, then release.
        let cloned = self.file
            .lock()
            .map_err(|_| io::Error::other("LocalFileSource lock poisoned"))?
            .try_clone()?;
        tokio::task::spawn_blocking(move || {
            use std::io::Read;
            let mut f = cloned;
            f.seek(SeekFrom::Start(offset))?;
            let mut buf = vec![0u8; len];
            f.read_exact(&mut buf)?;
            Ok(Bytes::from(buf))
        })
        .await
        .map_err(io::Error::other)?
    }

    fn size_hint(&self) -> Option<u64> {
        Some(self.size)
    }
}

// ---------------------------------------------------------------------------
// MmapSource
// ---------------------------------------------------------------------------

/// Serves byte ranges from an already-mapped file — zero-copy, zero syscalls
/// per read. Used by the `.zidx` reader and any pre-mapped archive case.
pub struct MmapSource {
    mmap: Arc<Mmap>,
}

impl MmapSource {
    pub fn new(mmap: Arc<Mmap>) -> Self {
        Self { mmap }
    }
}

#[async_trait]
impl ByteSource for MmapSource {
    async fn read_range(&self, offset: u64, len: usize) -> io::Result<Bytes> {
        let start = offset as usize;
        let end = start
            .checked_add(len)
            .filter(|&e| e <= self.mmap.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "read_range out of bounds"))?;
        Ok(Bytes::copy_from_slice(&self.mmap[start..end]))
    }

    fn size_hint(&self) -> Option<u64> {
        Some(self.mmap.len() as u64)
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;
    use std::io::Write as _;

    fn write_temp(data: &[u8]) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(data).unwrap();
        f.flush().unwrap();
        f
    }

    #[tokio::test]
    async fn local_read_range_correct_bytes() {
        let data = b"hello world this is a test";
        let f    = write_temp(data);
        let src  = LocalFileSource::new(f.path().to_path_buf()).unwrap();
        let got  = src.read_range(6, 5).await.unwrap();
        assert_eq!(&got[..], b"world");
    }

    #[tokio::test]
    async fn local_read_range_start() {
        let data = b"abcdefgh";
        let f    = write_temp(data);
        let src  = LocalFileSource::new(f.path().to_path_buf()).unwrap();
        let got  = src.read_range(0, 4).await.unwrap();
        assert_eq!(&got[..], b"abcd");
    }

    #[tokio::test]
    async fn local_read_range_out_of_bounds_is_error() {
        let data = b"short";
        let f    = write_temp(data);
        let src  = LocalFileSource::new(f.path().to_path_buf()).unwrap();
        assert!(src.read_range(100, 4).await.is_err());
    }

    #[tokio::test]
    async fn local_size_hint_matches_file_length() {
        let data = b"0123456789";
        let f    = write_temp(data);
        let src  = LocalFileSource::new(f.path().to_path_buf()).unwrap();
        assert_eq!(src.size_hint(), Some(10));
    }

    #[tokio::test]
    async fn mmap_read_range_correct_bytes() {
        let data = b"hello world this is a test";
        let f    = write_temp(data);
        let file = std::fs::File::open(f.path()).unwrap();
        let mmap = unsafe { memmap2::MmapOptions::new().map(&file).unwrap() };
        let src  = MmapSource::new(Arc::new(mmap));
        let got  = src.read_range(6, 5).await.unwrap();
        assert_eq!(&got[..], b"world");
    }

    #[tokio::test]
    async fn mmap_read_range_out_of_bounds_is_error() {
        let data = b"short";
        let f    = write_temp(data);
        let file = std::fs::File::open(f.path()).unwrap();
        let mmap = unsafe { memmap2::MmapOptions::new().map(&file).unwrap() };
        let src  = MmapSource::new(Arc::new(mmap));
        assert!(src.read_range(100, 4).await.is_err());
    }
}
