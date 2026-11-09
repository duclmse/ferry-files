use super::reader::decompress_block;
use super::*;

// ---------------------------------------------------------------------------
// Ring-buffer sliding window (32 KB)
// ---------------------------------------------------------------------------

pub(super) struct Window {
    buf: Box<[u8; WINDOW_SIZE]>,
    head: usize,
    filled: bool,
}

impl Window {
    pub(super) fn new() -> Self {
        Self {
            buf: Box::new([0u8; WINDOW_SIZE]),
            head: 0,
            filled: false,
        }
    }

    pub(super) fn push_slice(&mut self, data: &[u8]) {
        let mut remaining = data;
        while !remaining.is_empty() {
            let space = WINDOW_SIZE - self.head;
            let n = remaining.len().min(space);
            self.buf[self.head..self.head + n].copy_from_slice(&remaining[..n]);
            self.head += n;
            remaining = &remaining[n..];
            if self.head == WINDOW_SIZE {
                self.head = 0;
                self.filled = true;
            }
        }
    }

    pub(super) fn snapshot(&self) -> [u8; WINDOW_SIZE] {
        let mut out = [0u8; WINDOW_SIZE];
        if !self.filled {
            // Back-references measure distance from the *end* of the fed
            // preamble, so the real (short) history must sit at the tail -
            // matching the filled case's "newest byte last" layout - with
            // zero padding at the front. A match can never reach past `head`
            // bytes back anyway, so the padding is never addressed.
            out[WINDOW_SIZE - self.head..].copy_from_slice(&self.buf[..self.head]);
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
        source: &dyn ByteSource,
        meta: &EntryMeta,
        out_path: &Path,
    ) -> Result<u64, DzxError> {
        if meta.compression != 8 {
            return Err(DzxError::UnsupportedMethod(meta.compression));
        }

        let data_start = parse_local_header_offset(source, meta.local_header_offset).await?;
        let compressed = source
            .read_range(data_start, meta.compressed_size as usize)
            .await?;

        let block_size = self.block_size;
        let uncompressed_size = meta.uncompressed_size;
        let out_path = out_path.to_path_buf();

        tokio::task::spawn_blocking(move || {
            build_blocking(&compressed, uncompressed_size, block_size, &out_path)
        })
        .await
        .map_err(|e| DzxError::Io(io::Error::other(e.to_string())))?
    }
}

// ---------------------------------------------------------------------------
// Blocking build helper - runs inside spawn_blocking
// ---------------------------------------------------------------------------

struct RawBlock {
    comp_offset: u64,
    uncomp_offset: u64,
    snapshot_idx: u32,
}

fn build_blocking(
    compressed: &[u8],
    uncompressed_size: u64,
    block_size: u32,
    out_path: &Path,
) -> Result<u64, DzxError> {
    use flate2::{Decompress, FlushDecompress, Status};

    let mut d = Decompress::new(false); // raw deflate
    let mut window = Window::new();
    let mut raw_blocks: Vec<RawBlock> = Vec::new();
    let mut snap_data: Vec<[u8; WINDOW_SIZE]> = Vec::new();

    // Block 0 always exists; its snapshot is the empty window.
    snap_data.push(window.snapshot());
    raw_blocks.push(RawBlock {
        comp_offset: 0,
        uncomp_offset: 0,
        snapshot_idx: 0,
    });

    let mut out_buf = [0u8; 8_192];
    let mut next_boundary = block_size as u64;
    // Bytes produced for the block currently being accumulated; reset at
    // every boundary. Bounded by `block_size`, used to verify each boundary
    // against an isolated re-decode (`verify_block_boundary`) before trusting it.
    let mut cur_block_out: Vec<u8> = Vec::new();

    // Feed one compressed byte per `decompress()` call. `flate2` parses ahead
    // into a following block's header/Huffman table whenever input is
    // available, since that needs no output space - handing it the whole
    // remaining buffer (as this loop used to) lets `total_in` overshoot past
    // the true boundary byte by an unpredictable amount, corrupting the
    // recorded `comp_offset`. Restricting `avail_in` to one byte at a time
    // makes overshoot impossible: the decoder cannot consume bytes it hasn't
    // been given.
    loop {
        let start = d.total_in() as usize;
        if start > compressed.len() {
            return Err(DzxError::Corrupt(
                "decoder consumed past end of input".into(),
            ));
        }
        let end = (start + 1).min(compressed.len());
        let in_slice = &compressed[start..end];
        let before_out = d.total_out();

        let status = d
            .decompress(in_slice, &mut out_buf, FlushDecompress::None)
            .map_err(|e| DzxError::Inflate(e.to_string()))?;

        let out_n = (d.total_out() - before_out) as usize;
        if out_n > 0 {
            window.push_slice(&out_buf[..out_n]);
            cur_block_out.extend_from_slice(&out_buf[..out_n]);
        }

        // Record a new entry once the block boundary crossed is independently
        // decodable (see `verify_boundary_with_retry`'s doc comment for why the
        // recorded `comp_offset` usually isn't the real resync point yet).
        if d.total_out() >= next_boundary {
            let comp_offset = d.total_in();
            verify_boundary_with_retry(compressed, &mut raw_blocks, &snap_data, &cur_block_out)?;
            cur_block_out.clear();

            let snap_idx = snap_data.len() as u32;
            snap_data.push(window.snapshot());
            raw_blocks.push(RawBlock {
                comp_offset,
                uncomp_offset: next_boundary,
                snapshot_idx: snap_idx,
            });
            next_boundary += block_size as u64;
        }

        if matches!(status, Status::StreamEnd) {
            verify_boundary_with_retry(compressed, &mut raw_blocks, &snap_data, &cur_block_out)?;
            break;
        }
        if start == compressed.len() && out_n == 0 {
            break;
        }
    }

    let total_in = d.total_in();
    let total_out = d.total_out();
    let count = raw_blocks.len() as u64;

    let params = WriteDzxParams {
        out_path,
        block_size,
        uncompressed_size,
        block_count: count,
        raw_blocks: &raw_blocks,
        snap_data: &snap_data,
        total_in,
        total_out,
    };
    write_dzx(params)?;
    Ok(count)
}

/// Verify that the block most recently completed in `raw_blocks` can be
/// reproduced by feeding just its own compressed bytes and window snapshot
/// through `decompress_block` - the exact function `read_range` uses at
/// request time. A mismatch means the recorded boundary is not a genuine
/// deflate resync point (e.g. the entry is one continuous Huffman block with
/// no `SYNC_FLUSH` markers); building `.dzx` for it would silently corrupt
/// Range reads later, so the whole build is aborted instead.
fn verify_block_boundary(
    compressed: &[u8],
    raw_blocks: &[RawBlock],
    snap_data: &[[u8; WINDOW_SIZE]],
    expected_out: &[u8],
) -> Result<(), DzxError> {
    let prev = raw_blocks
        .last()
        .ok_or_else(|| DzxError::Corrupt("no blocks recorded".into()))?;
    // Slice to the end of the buffer: `decompress_block` stops once it has
    // produced `expected_out.len()` bytes of output regardless of how many
    // extra compressed bytes trail it, so only `prev.comp_offset` (the
    // candidate resync point actually under test) matters here.
    let comp_data = &compressed[prev.comp_offset as usize..];
    let window = &snap_data[prev.snapshot_idx as usize];

    let decoded = decompress_block(comp_data, window, prev.uncomp_offset, 0, expected_out.len())?;
    if decoded != expected_out {
        return Err(DzxError::Corrupt(
            "block boundary is not independently decodable (entry likely lacks SYNC_FLUSH resync points at fixed intervals)".into(),
        ));
    }
    Ok(())
}

/// The block most recently pushed onto `raw_blocks` gets its `comp_offset`
/// from the *previous* boundary crossing, at which point verifying it just
/// means "does the stream decode continuously from the start up to here" -
/// trivially true for almost any candidate (it's not a restart, just a
/// continuation), so that check provides no real signal that the position is
/// a genuine resync point. The real test happens one block later, when
/// `verify_block_boundary` tries to decode that block *in isolation* via the
/// STORED-block preamble trick. If that fails, the recorded `comp_offset` is
/// still sitting inside the Z_SYNC_FLUSH tail-padding + empty-stored-block
/// marker (BFINAL=0, LEN=0000, NLEN=FFFF), which produces no output - so
/// shift it forward a byte and retry, capped so genuinely non-flushed entries
/// (no resync points at all) still fail fast rather than scanning forever.
fn verify_boundary_with_retry(
    compressed: &[u8],
    raw_blocks: &mut [RawBlock],
    snap_data: &[[u8; WINDOW_SIZE]],
    cur_block_out: &[u8],
) -> Result<(), DzxError> {
    const MAX_PROBE_BYTES: u32 = 32;
    let mut attempts = 0u32;
    loop {
        match verify_block_boundary(compressed, raw_blocks, snap_data, cur_block_out) {
            Ok(()) => return Ok(()),
            Err(e) => {
                attempts += 1;
                let Some(last) = raw_blocks.last_mut() else {
                    return Err(e);
                };
                if attempts > MAX_PROBE_BYTES {
                    return Err(e);
                }
                last.comp_offset += 1;
            }
        }
    }
}

struct WriteDzxParams<'a> {
    out_path: &'a Path,
    block_size: u32,
    uncompressed_size: u64,
    block_count: u64,
    raw_blocks: &'a [RawBlock],
    snap_data: &'a [[u8; WINDOW_SIZE]],
    total_in: u64,
    total_out: u64,
}

fn write_dzx(p: WriteDzxParams<'_>) -> Result<(), DzxError> {
    let WriteDzxParams {
        out_path,
        block_size,
        uncompressed_size,
        block_count,
        raw_blocks,
        snap_data,
        total_in,
        total_out,
    } = p;
    use io::Write as _;
    let mut f = std::fs::File::create(out_path)?;

    // Header - 32 bytes
    let mut hdr = [0u8; HEADER_SIZE];
    hdr[0..4].copy_from_slice(MAGIC);
    hdr[4..8].copy_from_slice(&VERSION.to_le_bytes());
    hdr[8..12].copy_from_slice(&block_size.to_le_bytes());
    hdr[12..16].copy_from_slice(&1u32.to_le_bytes()); // flags: bit 0 = has_window_snapshots
    hdr[16..24].copy_from_slice(&uncompressed_size.to_le_bytes());
    hdr[24..32].copy_from_slice(&block_count.to_le_bytes());
    f.write_all(&hdr)?;

    // Block table - block_count × 56 bytes
    for (i, rb) in raw_blocks.iter().enumerate() {
        let next = raw_blocks.get(i + 1);
        let comp_size = (next.map_or(total_in, |nb| nb.comp_offset) - rb.comp_offset) as u32;
        let uncomp_size = (next.map_or(total_out, |nb| nb.uncomp_offset) - rb.uncomp_offset) as u32;

        let mut entry = [0u8; BLOCK_ENTRY_SIZE];
        entry[0..8].copy_from_slice(&rb.comp_offset.to_le_bytes());
        entry[8..16].copy_from_slice(&rb.uncomp_offset.to_le_bytes());
        entry[16..20].copy_from_slice(&comp_size.to_le_bytes());
        entry[20..24].copy_from_slice(&uncomp_size.to_le_bytes());
        // [24] bit_count=0, [25..28] _pad=0, [28..32] bit_buffer=0 - all zero by default
        entry[32..36].copy_from_slice(&rb.snapshot_idx.to_le_bytes());
        // [36..56] _pad2=0 - already zeroed
        f.write_all(&entry)?;
    }

    // Window snapshots - block_count × 32768 bytes
    for snap in snap_data {
        f.write_all(snap.as_ref())?;
    }

    f.flush()?;
    Ok(())
}
