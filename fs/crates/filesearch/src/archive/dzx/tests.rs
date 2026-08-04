use super::*;
use super::builder::Window;
use std::io::Write as _;

// -----------------------------------------------------------------------
// Fixture helpers
// -----------------------------------------------------------------------

/// Compress `data` with raw deflate using flate2.
fn deflate(data: &[u8]) -> Vec<u8> {
    use flate2::write::DeflateEncoder;
    let mut enc = DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

/// Compress `data` with Z_SYNC_FLUSH every `flush_every` uncompressed bytes.
/// Each flush emits a SYNC_FLUSH trailer (00 00 FF FF) making those stream
/// positions valid raw-deflate restart points - required by DZX block indexing.
fn deflate_with_sync_flush(data: &[u8], flush_every: usize) -> Vec<u8> {
    use flate2::write::DeflateEncoder;
    let mut enc = DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    for chunk in data.chunks(flush_every) {
        enc.write_all(chunk).unwrap();
        enc.flush().unwrap(); // Z_SYNC_FLUSH
    }
    enc.finish().unwrap()
}

/// Build a minimal ZIP local file header + compressed data in memory.
/// Returns (full_bytes, compressed_size, uncompressed_size, data_offset).
fn make_local_entry(data: &[u8]) -> (Vec<u8>, u64, u64, u64) {
    let compressed   = deflate(data);
    let fname        = b"test.bin";
    let extra: &[u8] = &[];

    let mut out = Vec::new();
    out.extend_from_slice(b"PK\x03\x04"); // signature
    out.extend_from_slice(&[0u8; 22]);     // fields up to filename_len
    out.extend_from_slice(&(fname.len() as u16).to_le_bytes());
    out.extend_from_slice(&(extra.len() as u16).to_le_bytes());
    out.extend_from_slice(fname);
    out.extend_from_slice(extra);
    let data_offset = out.len() as u64;
    out.extend_from_slice(&compressed);

    let comp_size   = compressed.len() as u64;
    let uncomp_size = data.len() as u64;
    (out, comp_size, uncomp_size, data_offset)
}

struct MemSource(Vec<u8>);

#[async_trait::async_trait]
impl ByteSource for MemSource {
    async fn read_range(&self, offset: u64, len: usize) -> io::Result<Bytes> {
        let s = offset as usize;
        let e = s.checked_add(len)
            .filter(|&e| e <= self.0.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "eof"))?;
        Ok(Bytes::copy_from_slice(&self.0[s..e]))
    }
}

fn make_source_and_meta(data: &[u8]) -> (MemSource, EntryMeta) {
    let (bytes, comp_size, uncomp_size, _) = make_local_entry(data);
    let meta = EntryMeta {
        compression:         8,
        compressed_size:     comp_size,
        uncompressed_size:   uncomp_size,
        local_header_offset: 0,
    };
    (MemSource(bytes), meta)
}

/// Like `make_source_and_meta` but uses SYNC_FLUSH every `block_size` bytes
/// so the recorded `comp_offset` values are valid deflate restart points.
fn make_source_and_meta_synced(data: &[u8], block_size: u32) -> (MemSource, EntryMeta) {
    let compressed = deflate_with_sync_flush(data, block_size as usize);
    let fname        = b"test.bin";
    let extra: &[u8] = &[];
    let mut out = Vec::new();
    out.extend_from_slice(b"PK\x03\x04");
    out.extend_from_slice(&[0u8; 22]);
    out.extend_from_slice(&(fname.len() as u16).to_le_bytes());
    out.extend_from_slice(&(extra.len() as u16).to_le_bytes());
    out.extend_from_slice(fname);
    out.extend_from_slice(extra);
    out.extend_from_slice(&compressed);
    let meta = EntryMeta {
        compression:         8,
        compressed_size:     compressed.len() as u64,
        uncompressed_size:   data.len() as u64,
        local_header_offset: 0,
    };
    (MemSource(out), meta)
}

// -----------------------------------------------------------------------
// Window ring-buffer tests
// -----------------------------------------------------------------------

#[test]
fn window_empty_snapshot_is_all_zeros() {
    let w = Window::new();
    assert!(w.snapshot().iter().all(|&b| b == 0));
}

#[test]
fn window_short_fill_snapshot_prefix() {
    let mut w = Window::new();
    w.push_slice(&[1, 2, 3]);
    let snap = w.snapshot();
    assert_eq!(&snap[WINDOW_SIZE - 3..], &[1, 2, 3]);
    assert!(snap[..WINDOW_SIZE - 3].iter().all(|&b| b == 0));
}

#[test]
fn window_wrap_around_snapshot_is_linearised() {
    let mut w = Window::new();
    let first  = vec![0u8; WINDOW_SIZE];
    let second = vec![1u8; 100];
    w.push_slice(&first);
    w.push_slice(&second);
    let snap = w.snapshot();
    // After wrapping: last 100 bytes should be 1s, rest 0s.
    assert!(snap[..WINDOW_SIZE - 100].iter().all(|&b| b == 0));
    assert!(snap[WINDOW_SIZE - 100..].iter().all(|&b| b == 1));
}

// -----------------------------------------------------------------------
// DzxBuilder tests
// -----------------------------------------------------------------------

#[tokio::test]
async fn build_single_block_entry() {
    let data          = b"hello world, this is test content for dzx".repeat(10);
    let (src, meta)   = make_source_and_meta(&data);
    let tmp           = tempfile::NamedTempFile::new().unwrap();
    let builder       = DzxBuilder::new(DEFAULT_BLOCK_SIZE);
    let block_count   = builder.build(&src, &meta, tmp.path()).await.unwrap();
    assert_eq!(block_count, 1, "data smaller than block_size → single block");
}

#[tokio::test]
async fn build_multi_block_correct_count() {
    let block_size    = 1024u32;
    let data          = vec![0xABu8; 3500]; // ceil(3500/1024) = 4 blocks
    let (src, meta)   = make_source_and_meta_synced(&data, block_size);
    let tmp           = tempfile::NamedTempFile::new().unwrap();
    let builder       = DzxBuilder::new(block_size);
    let count         = builder.build(&src, &meta, tmp.path()).await.unwrap();
    assert_eq!(count, 4);
}

#[tokio::test]
async fn build_and_open_find_first_block() {
    let data          = b"abcdefghij".repeat(200);
    let (src, meta)   = make_source_and_meta(&data);
    let tmp           = tempfile::NamedTempFile::new().unwrap();
    let builder       = DzxBuilder::new(DEFAULT_BLOCK_SIZE);
    builder.build(&src, &meta, tmp.path()).await.unwrap();

    let idx   = DzxIndex::open(tmp.path()).unwrap();
    let block = idx.find_block(0).unwrap();
    assert_eq!(block.comp_offset, 0);
    assert_eq!(block.uncomp_offset, 0);
}

#[tokio::test]
async fn find_block_mid_offset() {
    let block_size    = 512u32;
    let data          = vec![42u8; 2000];
    let (src, meta)   = make_source_and_meta_synced(&data, block_size);
    let tmp           = tempfile::NamedTempFile::new().unwrap();
    let builder       = DzxBuilder::new(block_size);
    builder.build(&src, &meta, tmp.path()).await.unwrap();

    let idx   = DzxIndex::open(tmp.path()).unwrap();
    let block = idx.find_block(700).unwrap();
    assert!(block.uncomp_offset <= 700);
    assert!(block.uncomp_offset + block.uncomp_size as u64 > 700);
}

#[tokio::test]
async fn find_block_last_offset() {
    let block_size    = 512u32;
    let data          = vec![7u8; 1500];
    let (src, meta)   = make_source_and_meta_synced(&data, block_size);
    let tmp           = tempfile::NamedTempFile::new().unwrap();
    let builder       = DzxBuilder::new(block_size);
    builder.build(&src, &meta, tmp.path()).await.unwrap();

    let idx   = DzxIndex::open(tmp.path()).unwrap();
    let last  = data.len() as u64 - 1;
    let block = idx.find_block(last).unwrap();
    assert!(block.uncomp_offset <= last);
}

// -----------------------------------------------------------------------
// read_range correctness (byte-identical vs full inflate)
// -----------------------------------------------------------------------

#[tokio::test]
async fn read_range_from_zero_matches_full_inflate() {
    let block_size    = 512u32;
    let data: Vec<u8> = (0u8..=255).cycle().take(2000).collect();
    let (src, meta)   = make_source_and_meta_synced(&data, block_size);
    let tmp           = tempfile::NamedTempFile::new().unwrap();
    DzxBuilder::new(block_size).build(&src, &meta, tmp.path()).await.unwrap();

    let dzx    = DzxIndex::open(tmp.path()).unwrap();
    let result = read_range(&src, &meta, &dzx, 0, data.len()).await.unwrap();
    assert_eq!(&result[..], &data[..]);
}

#[tokio::test]
async fn read_range_mid_matches_slice() {
    let block_size    = 256u32;
    let data: Vec<u8> = (0u8..=255).cycle().take(1000).collect();
    let (src, meta)   = make_source_and_meta_synced(&data, block_size);
    let tmp           = tempfile::NamedTempFile::new().unwrap();
    DzxBuilder::new(block_size).build(&src, &meta, tmp.path()).await.unwrap();

    let dzx    = DzxIndex::open(tmp.path()).unwrap();
    let result = read_range(&src, &meta, &dzx, 300, 200).await.unwrap();
    assert_eq!(&result[..], &data[300..500]);
}

#[tokio::test]
async fn read_range_crosses_block_boundary() {
    let block_size    = 256u32;
    let data: Vec<u8> = (0u8..=255).cycle().take(1000).collect();
    let (src, meta)   = make_source_and_meta_synced(&data, block_size);
    let tmp           = tempfile::NamedTempFile::new().unwrap();
    DzxBuilder::new(block_size).build(&src, &meta, tmp.path()).await.unwrap();

    let dzx    = DzxIndex::open(tmp.path()).unwrap();
    // 200..400 crosses the 256-byte boundary
    let result = read_range(&src, &meta, &dzx, 200, 200).await.unwrap();
    assert_eq!(&result[..], &data[200..400]);
}

#[tokio::test]
async fn read_range_zero_len_is_empty() {
    let data          = b"anything".to_vec();
    let (src, meta)   = make_source_and_meta(&data);
    let tmp           = tempfile::NamedTempFile::new().unwrap();
    DzxBuilder::new(DEFAULT_BLOCK_SIZE).build(&src, &meta, tmp.path()).await.unwrap();

    let dzx    = DzxIndex::open(tmp.path()).unwrap();
    let result = read_range(&src, &meta, &dzx, 0, 0).await.unwrap();
    assert!(result.is_empty());
}

// -----------------------------------------------------------------------
// DzxIndex edge-case tests
// -----------------------------------------------------------------------

#[tokio::test]
async fn open_corrupt_magic_is_error() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"BAAD\x00\x00\x00\x00").unwrap();
    assert!(DzxIndex::open(tmp.path()).is_err());
}

#[tokio::test]
async fn open_truncated_is_error() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"DZX1").unwrap();
    assert!(DzxIndex::open(tmp.path()).is_err());
}

// -----------------------------------------------------------------------
// dzx_path_for
// -----------------------------------------------------------------------

#[test]
fn dzx_path_has_dzx_extension() {
    use std::path::Path;
    let dir = Path::new("/tmp/dzx");
    let p   = dzx_path_for(dir, Path::new("/data/archive.zip"), 12345);
    assert_eq!(p.extension().and_then(|e| e.to_str()), Some("dzx"));
    assert!(p.parent() == Some(dir));
}

#[test]
fn dzx_path_different_offsets_differ() {
    use std::path::Path;
    let dir = Path::new("/tmp/dzx");
    let p1  = dzx_path_for(dir, Path::new("/data/archive.zip"), 0);
    let p2  = dzx_path_for(dir, Path::new("/data/archive.zip"), 999);
    assert_ne!(p1, p2);
}
