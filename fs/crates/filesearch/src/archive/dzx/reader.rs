use super::*;

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
// DzxIndex - mmap-backed reader
// ---------------------------------------------------------------------------

fn read_le<const N: usize>(buf: &[u8], offset: usize) -> Result<[u8; N], DzxError> {
    buf.get(offset..offset + N)
        .and_then(|s| s.try_into().ok())
        .ok_or_else(|| DzxError::Corrupt(format!("truncated field at offset {offset}")))
}

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

        let version = u32::from_le_bytes(read_le::<4>(&mmap, 4)?);
        if version != VERSION {
            return Err(DzxError::Corrupt(format!("unsupported version {version}")));
        }

        let block_size  = u32::from_le_bytes(read_le::<4>(&mmap, 8)?);
        let block_count = u64::from_le_bytes(read_le::<8>(&mmap, 24)?);

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
            comp_offset:   u64::from_le_bytes(read_le::<8>(b, 0)?),
            uncomp_offset: u64::from_le_bytes(read_le::<8>(b, 8)?),
            comp_size:     u32::from_le_bytes(read_le::<4>(b, 16)?),
            uncomp_size:   u32::from_le_bytes(read_le::<4>(b, 20)?),
            snapshot_idx:  u32::from_le_bytes(read_le::<4>(b, 32)?),
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
// Random-access read (async, iterative - not recursive)
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
// decompress_block - sync, called from spawn_blocking
// ---------------------------------------------------------------------------

pub(super) fn decompress_block(
    comp_data:     &[u8],
    window:        &[u8],
    uncomp_offset: u64,
    skip:          usize,
    take:          usize,
) -> io::Result<Vec<u8>> {
    use flate2::{Decompress, FlushDecompress, Status};

    let mut d       = Decompress::new(false); // raw deflate
    // Must be >= WINDOW_SIZE: with FlushDecompress::None, flate2 can consume the
    // whole synthetic STORED-block preamble into its internal window in one call
    // while only copying what fits in `output` - it won't drain the rest on a
    // later call with empty input, so undersizing this stalls the preamble seed
    // partway through and desyncs the decoder from the real block data that follows.
    let mut tmp_buf = [0u8; WINDOW_SIZE];

    // Non-first blocks need the LZ77 history from prior output.  Seed it by
    // feeding the snapshot as a non-final STORED deflate block; the decompressor
    // outputs the 32 KB and loads it into its sliding window.  We discard that
    // output, then feed the actual compressed data.
    // (flate2::Decompress has no set_dictionary; the STORED-block trick is the
    // portable equivalent for raw-deflate streams.)
    if uncomp_offset > 0 {
        let preamble = make_stored_block(window);
        let mut pos   = 0usize;
        // flate2 may consume the whole STORED block into its internal window in
        // one call while only copying one tmp_buf's worth into `output` - input
        // being exhausted does not mean the window is fully seeded yet, so keep
        // pumping (with empty input once `pos` reaches the end) until all of
        // `window` has actually been produced as output.
        while (d.total_out() as usize) < window.len() {
            let before_in  = d.total_in();
            let before_out = d.total_out();
            let status     = d
                .decompress(&preamble[pos..], &mut tmp_buf, FlushDecompress::None)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            let consumed     = (d.total_in() - before_in) as usize;
            let out_produced = d.total_out() - before_out;
            pos += consumed;
            if matches!(status, Status::StreamEnd) {
                break;
            }
            if consumed == 0 && out_produced == 0 {
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
