//! Deflate random-access index (`.dzx`) — block builder and mmap-backed reader.
//!
//! # Binary format
//!
//! ```text
//! [Header — 32 bytes]
//! [Block Table — block_count × 56 bytes]
//! [Window Snapshots — block_count × 32768 bytes]
//! ```
//!
//! All integers are little-endian. See `docs/01-phases/11-phase11-deflate-random-access.md`.

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

// ---------------------------------------------------------------------------
// Ring-buffer sliding window (32 KB)
// ---------------------------------------------------------------------------

struct Window {
    buf:    Box<[u8; WINDOW_SIZE]>,
    head:   usize,
    filled: bool,
}

impl Window {
    fn new() -> Self {
        Self { buf: Box::new([0u8; WINDOW_SIZE]), head: 0, filled: false }
    }

    fn push_slice(&mut self, data: &[u8]) {
        let mut remaining = data;
        while !remaining.is_empty() {
            let space   = WINDOW_SIZE - self.head;
            let n       = remaining.len().min(space);
            self.buf[self.head..self.head + n].copy_from_slice(&remaining[..n]);
            self.head  += n;
            remaining   = &remaining[n..];
            if self.head == WINDOW_SIZE {
                self.head   = 0;
                self.filled = true;
            }
        }
    }

    fn snapshot(&self) -> [u8; WINDOW_SIZE] {
        let mut out = [0u8; WINDOW_SIZE];
        if !self.filled {
            out[..self.head].copy_from_slice(&self.buf[..self.head]);
        } else {
            let (a, b) = self.buf.split_at(self.head);
            out[..b.len()].copy_from_slice(b);
            out[b.len()..].copy_from_slice(a);
        }
        out
    }
}

// ---------------------------------------------------------------------------
// DzxBuilder
// ---------------------------------------------------------------------------

pub struct DzxBuilder {
    pub block_size: u32,
}

impl DzxBuilder {
    pub fn new(block_size: u32) -> Self {
        Self { block_size }
    }

    /// Post-process a deflate-compressed ZIP entry, writing a `.dzx` sidecar.
    /// Returns the number of blocks recorded.
    pub async fn build(
        &self,
        source:   &dyn ByteSource,
        meta:     &EntryMeta,
        out_path: &Path,
    ) -> Result<u64, DzxError> {
        if meta.compression != 8 {
            return Err(DzxError::UnsupportedMethod(meta.compression));
        }

        let data_start = parse_local_header_offset(source, meta.local_header_offset).await?;
        let compressed = source.read_range(data_start, meta.compressed_size as usize).await?;

        let block_size        = self.block_size;
        let uncompressed_size = meta.uncompressed_size;
        let out_path          = out_path.to_path_buf();

        tokio::task::spawn_blocking(move || {
            build_blocking(&compressed, uncompressed_size, block_size, &out_path)
        })
        .await
        .map_err(|e| DzxError::Io(io::Error::other(e.to_string())))?
    }
}

// ---------------------------------------------------------------------------
// Blocking build helper — runs inside spawn_blocking
// ---------------------------------------------------------------------------

struct RawBlock {
    comp_offset:   u64,
    uncomp_offset: u64,
    snapshot_idx:  u32,
}

fn build_blocking(
    compressed:        &[u8],
    uncompressed_size: u64,
    block_size:        u32,
    out_path:          &Path,
) -> Result<u64, DzxError> {
    use flate2::{Decompress, FlushDecompress, Status};

    let mut d      = Decompress::new(false); // raw deflate
    let mut window = Window::new();
    let mut raw_blocks: Vec<RawBlock>          = Vec::new();
    let mut snap_data:  Vec<[u8; WINDOW_SIZE]> = Vec::new();

    // Block 0 always exists; its snapshot is the empty window.
    snap_data.push(window.snapshot());
    raw_blocks.push(RawBlock { comp_offset: 0, uncomp_offset: 0, snapshot_idx: 0 });

    let mut out_buf       = [0u8; 8_192];
    let mut next_boundary = block_size as u64;

    loop {
        let in_slice   = &compressed[d.total_in() as usize..];
        let before_out = d.total_out();

        let status = d
            .decompress(in_slice, &mut out_buf, FlushDecompress::None)
            .map_err(|e| DzxError::Inflate(e.to_string()))?;

        let out_n = (d.total_out() - before_out) as usize;
        if out_n > 0 {
            window.push_slice(&out_buf[..out_n]);
        }

        // Record a new entry for every block boundary crossed.
        while d.total_out() >= next_boundary {
            let snap_idx = snap_data.len() as u32;
            snap_data.push(window.snapshot());
            raw_blocks.push(RawBlock {
                comp_offset:   d.total_in(),
                uncomp_offset: next_boundary,
                snapshot_idx:  snap_idx,
            });
            next_boundary += block_size as u64;
        }

        match status {
            Status::StreamEnd => break,
            Status::Ok | Status::BufError => {
                if d.total_in() as usize >= compressed.len() && out_n == 0 {
                    break;
                }
            }
        }
    }

    let total_in  = d.total_in();
    let total_out = d.total_out();
    let count     = raw_blocks.len() as u64;

    let params = WriteDzxParams {
        out_path,
        block_size,
        uncompressed_size,
        block_count: count,
        raw_blocks: &raw_blocks,
        snap_data:  &snap_data,
        total_in,
        total_out,
    };
    write_dzx(params)?;
    Ok(count)
}

struct WriteDzxParams<'a> {
    out_path:          &'a Path,
    block_size:        u32,
    uncompressed_size: u64,
    block_count:       u64,
    raw_blocks:        &'a [RawBlock],
    snap_data:         &'a [[u8; WINDOW_SIZE]],
    total_in:          u64,
    total_out:         u64,
}

fn write_dzx(p: WriteDzxParams<'_>) -> Result<(), DzxError> {
    let WriteDzxParams { out_path, block_size, uncompressed_size, block_count, raw_blocks, snap_data, total_in, total_out } = p;
    use io::Write as _;
    let mut f = std::fs::File::create(out_path)?;

    // Header — 32 bytes
    let mut hdr = [0u8; HEADER_SIZE];
    hdr[0..4].copy_from_slice(MAGIC);
    hdr[4..8].copy_from_slice(&VERSION.to_le_bytes());
    hdr[8..12].copy_from_slice(&block_size.to_le_bytes());
    hdr[12..16].copy_from_slice(&1u32.to_le_bytes()); // flags: bit 0 = has_window_snapshots
    hdr[16..24].copy_from_slice(&uncompressed_size.to_le_bytes());
    hdr[24..32].copy_from_slice(&block_count.to_le_bytes());
    f.write_all(&hdr)?;

    // Block table — block_count × 56 bytes
    for (i, rb) in raw_blocks.iter().enumerate() {
        let next        = raw_blocks.get(i + 1);
        let comp_size   = (next.map_or(total_in,  |nb| nb.comp_offset)  - rb.comp_offset)  as u32;
        let uncomp_size = (next.map_or(total_out, |nb| nb.uncomp_offset) - rb.uncomp_offset) as u32;

        let mut entry = [0u8; BLOCK_ENTRY_SIZE];
        entry[0..8].copy_from_slice(&rb.comp_offset.to_le_bytes());
        entry[8..16].copy_from_slice(&rb.uncomp_offset.to_le_bytes());
        entry[16..20].copy_from_slice(&comp_size.to_le_bytes());
        entry[20..24].copy_from_slice(&uncomp_size.to_le_bytes());
        // [24] bit_count=0, [25..28] _pad=0, [28..32] bit_buffer=0 — all zero by default
        entry[32..36].copy_from_slice(&rb.snapshot_idx.to_le_bytes());
        // [36..56] _pad2=0 — already zeroed
        f.write_all(&entry)?;
    }

    // Window snapshots — block_count × 32768 bytes
    for snap in snap_data {
        f.write_all(snap.as_ref())?;
    }

    f.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// DzxBlockData
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DzxBlockData {
    pub comp_offset:   u64,
    pub uncomp_offset: u64,
    pub comp_size:     u32,
    pub uncomp_size:   u32,
    pub snapshot_idx:  u32,
}

// ---------------------------------------------------------------------------
// DzxIndex — mmap-backed reader
// ---------------------------------------------------------------------------

pub struct DzxIndex {
    mmap:            memmap2::Mmap,
    pub block_size:  u32,
    pub block_count: u64,
    blocks_off:      usize, // = HEADER_SIZE
    snaps_off:       usize, // = HEADER_SIZE + block_count * BLOCK_ENTRY_SIZE
}

impl DzxIndex {
    pub fn open(path: &Path) -> Result<Self, DzxError> {
        let file = std::fs::File::open(path)?;
        let mmap = unsafe { memmap2::MmapOptions::new().map(&file)? };

        if mmap.len() < HEADER_SIZE {
            return Err(DzxError::Corrupt("file too small for header".into()));
        }
        if &mmap[0..4] != MAGIC.as_ref() {
            return Err(DzxError::Corrupt("invalid magic bytes".into()));
        }

        let version = u32::from_le_bytes(mmap[4..8].try_into().unwrap());
        if version != VERSION {
            return Err(DzxError::Corrupt(format!("unsupported version {version}")));
        }

        let block_size  = u32::from_le_bytes(mmap[8..12].try_into().unwrap());
        let block_count = u64::from_le_bytes(mmap[24..32].try_into().unwrap());

        let blocks_off = HEADER_SIZE;
        let snaps_off  = blocks_off
            .checked_add(block_count as usize * BLOCK_ENTRY_SIZE)
            .ok_or_else(|| DzxError::Corrupt("block count overflow".into()))?;
        let expected   = snaps_off
            .checked_add(block_count as usize * WINDOW_SIZE)
            .ok_or_else(|| DzxError::Corrupt("snapshot section overflow".into()))?;

        if mmap.len() < expected {
            return Err(DzxError::Corrupt(format!(
                "truncated: need {expected} bytes, have {}",
                mmap.len()
            )));
        }

        Ok(Self { mmap, block_size, block_count, blocks_off, snaps_off })
    }

    fn block(&self, id: u64) -> Result<DzxBlockData, DzxError> {
        if id >= self.block_count {
            return Err(DzxError::Corrupt(format!(
                "block id {id} >= block_count {}",
                self.block_count
            )));
        }
        let off = self.blocks_off + id as usize * BLOCK_ENTRY_SIZE;
        let b   = &self.mmap[off..off + BLOCK_ENTRY_SIZE];
        Ok(DzxBlockData {
            comp_offset:   u64::from_le_bytes(b[0..8].try_into().unwrap()),
            uncomp_offset: u64::from_le_bytes(b[8..16].try_into().unwrap()),
            comp_size:     u32::from_le_bytes(b[16..20].try_into().unwrap()),
            uncomp_size:   u32::from_le_bytes(b[20..24].try_into().unwrap()),
            snapshot_idx:  u32::from_le_bytes(b[32..36].try_into().unwrap()),
        })
    }

    /// O(1) lookup: block containing `uncomp_offset`.
    pub fn find_block(&self, uncomp_offset: u64) -> Result<DzxBlockData, DzxError> {
        if self.block_count == 0 {
            return Err(DzxError::Corrupt("index has no blocks".into()));
        }
        let id = (uncomp_offset / self.block_size as u64).min(self.block_count - 1);
        self.block(id)
    }

    /// Return the 32 KB window snapshot for block `idx`.
    pub fn snapshot(&self, idx: u32) -> Result<&[u8], DzxError> {
        let off = self.snaps_off
            .checked_add(idx as usize * WINDOW_SIZE)
            .ok_or_else(|| DzxError::Corrupt("snapshot index overflow".into()))?;
        if off + WINDOW_SIZE > self.mmap.len() {
            return Err(DzxError::Corrupt(format!("snapshot {idx} out of bounds")));
        }
        Ok(&self.mmap[off..off + WINDOW_SIZE])
    }
}

// ---------------------------------------------------------------------------
// Random-access read (async, iterative — not recursive)
// ---------------------------------------------------------------------------

pub async fn read_range(
    source: &dyn ByteSource,
    meta:   &EntryMeta,
    dzx:    &DzxIndex,
    offset: u64,
    len:    usize,
) -> io::Result<Bytes> {
    if len == 0 {
        return Ok(Bytes::new());
    }

    let data_start = parse_local_header_offset(source, meta.local_header_offset).await?;
    let mut remaining  = len;
    let mut cur_offset = offset;
    let mut output     = BytesMut::with_capacity(len);

    while remaining > 0 {
        let block = dzx
            .find_block(cur_offset)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

        let window = dzx
            .snapshot(block.snapshot_idx)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?
            .to_vec();

        let comp_data = source
            .read_range(data_start + block.comp_offset, block.comp_size as usize)
            .await?;

        let skip           = (cur_offset - block.uncomp_offset) as usize;
        let available      = block.uncomp_size as usize - skip;
        let take           = available.min(remaining);
        let uncomp_offset  = block.uncomp_offset;

        let chunk = tokio::task::spawn_blocking(move || {
            decompress_block(&comp_data, &window, uncomp_offset, skip, take)
        })
        .await
        .map_err(io::Error::other)??;

        output.extend_from_slice(&chunk);
        remaining  -= take;
        cur_offset += take as u64;
    }

    Ok(output.freeze())
}

// ---------------------------------------------------------------------------
// decompress_block — sync, called from spawn_blocking
// ---------------------------------------------------------------------------

fn decompress_block(
    comp_data:     &[u8],
    window:        &[u8],
    uncomp_offset: u64,
    skip:          usize,
    take:          usize,
) -> io::Result<Vec<u8>> {
    use flate2::{Decompress, FlushDecompress, Status};

    let mut d       = Decompress::new(false); // raw deflate
    let mut tmp_buf = [0u8; 8_192];

    // Non-first blocks need the LZ77 history from prior output.  Seed it by
    // feeding the snapshot as a non-final STORED deflate block; the decompressor
    // outputs the 32 KB and loads it into its sliding window.  We discard that
    // output, then feed the actual compressed data.
    // (flate2::Decompress has no set_dictionary; the STORED-block trick is the
    // portable equivalent for raw-deflate streams.)
    if uncomp_offset > 0 {
        let preamble = make_stored_block(window);
        let mut pos  = 0usize;
        while pos < preamble.len() {
            let before_in = d.total_in();
            let status    = d
                .decompress(&preamble[pos..], &mut tmp_buf, FlushDecompress::None)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            let consumed  = (d.total_in() - before_in) as usize;
            pos += consumed;
            if matches!(status, Status::StreamEnd) || consumed == 0 {
                break;
            }
        }
    }

    // Decompress actual block bytes.  Track position in comp_data separately
    // because d.total_in() also counts the synthetic preamble bytes.
    let needed       = skip + take;
    let mut out      = Vec::with_capacity(needed);
    let mut comp_pos = 0usize;

    while out.len() < needed && comp_pos < comp_data.len() {
        let before_in  = d.total_in();
        let before_out = d.total_out() as usize;

        let status = d
            .decompress(&comp_data[comp_pos..], &mut tmp_buf, FlushDecompress::None)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

        let consumed     = (d.total_in() - before_in) as usize;
        let out_produced = d.total_out() as usize - before_out;
        comp_pos += consumed;
        out.extend_from_slice(&tmp_buf[..out_produced]);

        if matches!(status, Status::StreamEnd) || (consumed == 0 && out_produced == 0) {
            break;
        }
    }

    let start = skip.min(out.len());
    let end   = (skip + take).min(out.len());
    Ok(out[start..end].to_vec())
}

/// Wrap `data` as a non-final raw-deflate STORED block.
///
/// Processing this through a `Decompress` instance loads `data` into the
/// decoder's LZ77 history, enabling back-reference resolution across block
/// boundaries without `inflateSetDictionary` (not in flate2's public API).
fn make_stored_block(data: &[u8]) -> Vec<u8> {
    debug_assert!(data.len() <= 65_535);
    let len  = data.len() as u16;
    let nlen = !len;
    // First byte: BFINAL=0 (bit 0), BTYPE=00 (bits 1-2), padding bits=0
    let mut block = Vec::with_capacity(5 + data.len());
    block.push(0x00u8);
    block.extend_from_slice(&len.to_le_bytes());
    block.extend_from_slice(&nlen.to_le_bytes());
    block.extend_from_slice(data);
    block
}

// ---------------------------------------------------------------------------
// Path convention
// ---------------------------------------------------------------------------

/// `{dzx_dir}/blake3(zip_path + "#" + local_header_offset).dzx`
pub fn dzx_path_for(dzx_dir: &Path, zip_path: &Path, local_header_offset: u64) -> PathBuf {
    let key  = format!("{}#{}", zip_path.to_string_lossy(), local_header_offset);
    let hash = blake3::hash(key.as_bytes());
    dzx_dir.join(format!("{}.dzx", hash.to_hex()))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
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
    /// positions valid raw-deflate restart points — required by DZX block indexing.
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
        assert_eq!(&snap[..3], &[1, 2, 3]);
        assert!(snap[3..].iter().all(|&b| b == 0));
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
        let (src, meta)   = make_source_and_meta(&data);
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
        let (src, meta)   = make_source_and_meta(&data);
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
        let (src, meta)   = make_source_and_meta(&data);
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
}
