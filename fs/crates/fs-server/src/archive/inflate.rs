//! SIMD-accelerated inflate pipeline (Phase 13).
//!
//! Architecture:
//!   `RingBuffer` - double-buffer sliding window (2 × 32 KB), contiguous `slice_at`
//!   `copy_simd` - AVX2 bulk copy when compiled with target-feature=avx2; scalar otherwise
//!   `copy_match` - LZ77 overlap-safe copy built on `copy_simd`
//!   `HuffmanTable` - 9-bit fast lookup for the DEFLATE fixed literal/length tree
//!   `SimdDeflateDecoder` - `Decoder` impl: flate2 parses tokens, SIMD copies output bytes
//!   `InflateStats` - per-thread counters gated by `FS_INFLATE_STATS=1`

use bytes::Bytes;
use std::io;

use super::decoder::Decoder;

// ---------------------------------------------------------------------------
// RingBuffer - 2 × 32 KB double-buffer sliding window
// ---------------------------------------------------------------------------

const WINDOW: usize = 32 * 1024;

pub struct RingBuffer {
    buf: Box<[u8; WINDOW * 2]>,
    pos: usize,
}

impl RingBuffer {
    pub fn new() -> Self {
        Self {
            buf: Box::new([0u8; WINDOW * 2]),
            pos: 0,
        }
    }

    #[inline]
    pub fn write_byte(&mut self, b: u8) {
        self.buf[self.pos] = b;
        self.buf[self.pos + WINDOW] = b; // mirror keeps slice_at contiguous
        self.pos = (self.pos + 1) & (WINDOW - 1);
    }

    /// Return a contiguous slice of `len` bytes ending `back` bytes before the
    /// current write position. `back` must satisfy `1 <= back <= WINDOW` and
    /// `len <= back`.
    #[inline]
    pub fn slice_at(&self, back: usize, len: usize) -> &[u8] {
        debug_assert!((1..=WINDOW).contains(&back) && len <= back);
        let start = (self.pos + WINDOW - back) & (WINDOW - 1);
        &self.buf[start..start + len]
    }
}

impl Default for RingBuffer {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// copy_simd - SIMD bulk memcpy (AVX2 when available, scalar fallback)
// ---------------------------------------------------------------------------

/// Copy exactly `len` bytes from `src` into `dst` (appending).
/// Uses 256-bit AVX2 stores when compiled with `+avx2`; falls back to
/// `extend_from_slice` otherwise (the compiler will still auto-vectorise).
/// Does NOT zero-fill before writing - avoids the double-write that
/// `Vec::resize` + overwrite would produce.
pub fn copy_simd(dst: &mut Vec<u8>, src: &[u8], len: usize) {
    debug_assert_eq!(src.len(), len);

    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    // SAFETY: `reserve` ensures capacity; `set_len` is valid because
    // `copy_simd_avx2` initialises every byte in [start, start+len).
    unsafe {
        dst.reserve(len);
        let start = dst.len();
        copy_simd_avx2(dst.as_mut_ptr().add(start), src.as_ptr(), len);
        dst.set_len(start + len);
    }

    #[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
    dst.extend_from_slice(src);
}

#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
#[target_feature(enable = "avx2")]
unsafe fn copy_simd_avx2(dst: *mut u8, src: *const u8, len: usize) {
    use std::arch::x86_64::{__m256i, _mm256_loadu_si256, _mm256_storeu_si256};
    let mut i = 0;
    while i + 32 <= len {
        let v = _mm256_loadu_si256(src.add(i) as *const __m256i);
        _mm256_storeu_si256(dst.add(i) as *mut __m256i, v);
        i += 32;
    }
    // scalar tail
    let mut j = i;
    while j < len {
        *dst.add(j) = *src.add(j);
        j += 1;
    }
}

// ---------------------------------------------------------------------------
// copy_match - overlap-safe LZ77 back-reference copy
// ---------------------------------------------------------------------------

/// Copy a back-reference of `len` bytes at distance `dist` from `ring` into `dst`.
/// When `dist < len` the pattern expands (e.g. dist=1 produces a run of one byte).
pub fn copy_match(dst: &mut Vec<u8>, ring: &RingBuffer, dist: usize, len: usize) {
    if dist >= len {
        // Non-overlapping: single bulk copy.
        let src = ring.slice_at(dist, len);
        copy_simd(dst, src, len);
    } else {
        // Overlapping expanding run: copy `dist` bytes at a time.
        let mut copied = 0;
        while copied < len {
            let chunk = (len - copied).min(dist);
            let src = ring.slice_at(dist, chunk);
            copy_simd(dst, src, chunk);
            copied += chunk;
        }
    }
}

// ---------------------------------------------------------------------------
// HuffmanTable - 9-bit fast lookup (DEFLATE fixed literal/length tree)
// ---------------------------------------------------------------------------

/// 9-bit fast Huffman table. Each entry encodes `(symbol << 4) | code_len`
/// (u16). An entry of 0 means the slot is unfilled (code length > 9 or unused
/// symbol → caller falls back to canonical decode).
pub struct HuffmanTable {
    fast: Box<[u16; 512]>,
}

impl HuffmanTable {
    /// Build the DEFLATE fixed literal/length Huffman table (RFC 1951 §3.2.6).
    pub fn fixed_literal_length() -> Self {
        // Fixed code lengths: 0-143→8, 144-255→9, 256-279→7, 280-287→8
        let mut lengths = [0u8; 288];
        for l in lengths[0..=143].iter_mut() {
            *l = 8;
        }
        for l in lengths[144..=255].iter_mut() {
            *l = 9;
        }
        for l in lengths[256..=279].iter_mut() {
            *l = 7;
        }
        for l in lengths[280..=287].iter_mut() {
            *l = 8;
        }
        Self {
            fast: build_fast_table(&lengths),
        }
    }

    pub fn new_empty() -> Self {
        Self {
            fast: Box::new([0u16; 512]),
        }
    }

    /// Look up up to 9 bits. Returns `(symbol, code_len)` or `None` for long
    /// codes (>9 bits) that fall back to canonical decoding.
    #[inline]
    pub fn decode(&self, bits: u32) -> Option<(u16, u8)> {
        let entry = self.fast[(bits & 0x1FF) as usize];
        if entry == 0 {
            return None;
        }
        Some((entry >> 4, (entry & 0xF) as u8))
    }
}

/// Canonical Huffman code generation (RFC 1951) → 9-bit fast table.
/// Bits are reversed for DEFLATE's LSB-first bit order.
fn build_fast_table(lengths: &[u8]) -> Box<[u16; 512]> {
    // Count codes of each length.
    let mut bl_count = [0u32; 16];
    for &l in lengths {
        if l > 0 {
            bl_count[l as usize] += 1;
        }
    }

    // Compute starting code for each length.
    let mut next_code = [0u32; 16];
    let mut code = 0u32;
    for bits in 1usize..=15 {
        code = (code + bl_count[bits - 1]) << 1;
        next_code[bits] = code;
    }

    let mut table = Box::new([0u16; 512]);
    for (sym, &len) in lengths.iter().enumerate() {
        if len == 0 || len > 9 {
            continue;
        }
        let c = next_code[len as usize];
        next_code[len as usize] += 1;

        // Reverse bits for LSB-first reading.
        let rev = reverse_bits(c, len);
        let reps = 1usize << (9 - len as usize);
        for i in 0..reps {
            let idx = (rev as usize) | (i << len as usize);
            if idx < 512 {
                // (0 << 4) | len = len ≠ 0 for any len ≥ 1, so symbol 0 is safe.
                table[idx] = ((sym as u16) << 4) | (len as u16);
            }
        }
    }
    table
}

fn reverse_bits(val: u32, len: u8) -> u32 {
    let mut result = 0u32;
    let mut v = val;
    for _ in 0..len {
        result = (result << 1) | (v & 1);
        v >>= 1;
    }
    result
}

// ---------------------------------------------------------------------------
// SimdDeflateDecoder - approach (b): flate2 + SIMD output copy
// ---------------------------------------------------------------------------

/// SIMD-accelerated deflate decoder. Uses `flate2::Decompress` for Huffman
/// parsing and LZ77 match-copy (approach b from §13.4), then `copy_simd` for
/// the bulk transfer of each chunk's decompressed output.
pub struct SimdDeflateDecoder {
    inner: flate2::Decompress,
    out_buf: Vec<u8>,
}

impl SimdDeflateDecoder {
    pub fn new() -> Self {
        Self {
            inner: flate2::Decompress::new(false), // raw deflate
            out_buf: Vec::with_capacity(65_536),
        }
    }
}

impl Default for SimdDeflateDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder for SimdDeflateDecoder {
    fn feed(&mut self, input: &[u8]) -> io::Result<Bytes> {
        use flate2::{FlushDecompress, Status};

        let t0 = stats_now();
        let mut pos = 0usize;

        while pos < input.len() {
            // Reserve output headroom so decompress_vec is never blocked by a
            // full buffer - compressed data can expand by 10× or more.
            self.out_buf.reserve(65_536);

            let before = self.inner.total_in();
            let status = self
                .inner
                .decompress_vec(&input[pos..], &mut self.out_buf, FlushDecompress::None)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            let consumed = (self.inner.total_in() - before) as usize;
            pos += consumed;
            if matches!(status, Status::StreamEnd) || consumed == 0 {
                break;
            }
        }

        let decode_ns = stats_elapsed(t0);

        // Swap out_buf with a fresh Vec to hand the data to the caller with
        // zero copies. The replacement retains the same capacity so the next
        // feed call does not need to reallocate.
        let t1 = stats_now();
        let cap = self.out_buf.capacity();
        let out_data = std::mem::replace(&mut self.out_buf, Vec::with_capacity(cap));
        let copy_ns = stats_elapsed(t1);

        record_stats(decode_ns, copy_ns, out_data.len() as u64);
        Ok(Bytes::from(out_data))
    }

    fn finish(mut self: Box<Self>) -> io::Result<Bytes> {
        use flate2::FlushDecompress;
        let prev_len = self.out_buf.len();
        // Drain any internally buffered tail bytes.
        self.inner
            .decompress_vec(&[], &mut self.out_buf, FlushDecompress::Finish)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let new_slice = &self.out_buf[prev_len..];
        let mut out = Vec::with_capacity(new_slice.len());
        copy_simd(&mut out, new_slice, new_slice.len());
        Ok(Bytes::from(out))
    }
}

// ---------------------------------------------------------------------------
// InflateStats - thread-local counters, zero cost when env var is unset
// ---------------------------------------------------------------------------

#[derive(Default, Clone, Debug)]
pub struct InflateStats {
    pub io_ns: u64,
    pub decode_ns: u64,
    pub copy_ns: u64,
    pub bytes_out: u64,
}

std::thread_local! {
    static INFLATE_STATS: std::cell::RefCell<InflateStats> =
        std::cell::RefCell::new(InflateStats::default());
}

pub fn stats_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("FS_INFLATE_STATS").is_ok())
}

/// Read and reset the thread-local stats. Returns `None` when stats are disabled.
pub fn take_stats() -> Option<InflateStats> {
    if !stats_enabled() {
        return None;
    }
    INFLATE_STATS.with(|s| {
        let snapshot = s.borrow().clone();
        *s.borrow_mut() = InflateStats::default();
        Some(snapshot)
    })
}

#[inline]
fn stats_now() -> u64 {
    if !stats_enabled() {
        return 0;
    }
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[inline]
fn stats_elapsed(start: u64) -> u64 {
    if start == 0 {
        return 0;
    }
    stats_now().saturating_sub(start)
}

fn record_stats(decode_ns: u64, copy_ns: u64, bytes_out: u64) {
    if !stats_enabled() {
        return;
    }
    INFLATE_STATS.with(|s| {
        let mut s = s.borrow_mut();
        s.decode_ns += decode_ns;
        s.copy_ns += copy_ns;
        s.bytes_out += bytes_out;
    });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── copy_simd ────────────────────────────────────────────────────────────

    #[test]
    fn copy_simd_various_lengths() {
        for &len in &[1usize, 31, 32, 33, 128, 255] {
            let src: Vec<u8> = (0..len).map(|i| i as u8).collect();
            let mut dst = Vec::new();
            copy_simd(&mut dst, &src, len);
            assert_eq!(dst, src, "copy_simd failed for len={len}");
        }
    }

    // ── copy_match ───────────────────────────────────────────────────────────

    fn fill_ring(ring: &mut RingBuffer, data: &[u8]) {
        for &b in data {
            ring.write_byte(b);
        }
    }

    #[test]
    fn copy_match_no_overlap_equals_naive() {
        let payload: Vec<u8> = (0..200).map(|i: u8| i).collect();
        let mut ring = RingBuffer::new();
        fill_ring(&mut ring, &payload);

        let dist = 100usize;
        let len = 50usize;
        let mut dst = Vec::new();
        copy_match(&mut dst, &ring, dist, len);

        // Naive reference: last `dist` bytes of payload, take `len`.
        let naive: Vec<u8> = payload[payload.len() - dist..][..len].to_vec();
        assert_eq!(dst, naive);
    }

    #[test]
    fn copy_match_overlap_expanding_run() {
        // dist=3, len=9 → pattern abcabcabc
        let mut ring = RingBuffer::new();
        for b in b"abc" {
            ring.write_byte(*b);
        }

        let mut dst = Vec::new();
        copy_match(&mut dst, &ring, 3, 9);
        assert_eq!(dst, b"abcabcabc");
    }

    #[test]
    fn copy_match_dist_one_run_of_byte() {
        // dist=1, len=8 → eight copies of the last byte
        let mut ring = RingBuffer::new();
        ring.write_byte(0x42);

        let mut dst = Vec::new();
        copy_match(&mut dst, &ring, 1, 8);
        assert_eq!(dst, vec![0x42u8; 8]);
    }

    // ── RingBuffer ───────────────────────────────────────────────────────────

    #[test]
    fn ring_write_32k_slice_at_wraps() {
        let mut ring = RingBuffer::new();
        // Fill exactly 32 KB with 0x00, then write 64 bytes of 0xFF.
        for _ in 0..WINDOW {
            ring.write_byte(0x00);
        }
        for _ in 0..64 {
            ring.write_byte(0xFF);
        }
        // The last 64 bytes are 0xFF.
        let s = ring.slice_at(64, 64);
        assert!(s.iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn ring_slice_at_is_contiguous() {
        let mut ring = RingBuffer::new();
        let data: Vec<u8> = (0..100u8).collect();
        for &b in &data {
            ring.write_byte(b);
        }
        // slice_at(100, 100) should be contiguous and equal data.
        let s = ring.slice_at(100, 100);
        assert_eq!(s, &data[..]);
    }

    // ── HuffmanTable ─────────────────────────────────────────────────────────

    #[test]
    fn huffman_fixed_symbol_256_length_7() {
        let table = HuffmanTable::fixed_literal_length();
        // Symbol 256 (end-of-block) has the first length-7 code (= 0b0000000,
        // reversed = 0).  All 9-bit patterns 0, 128, 256, 384 map to it.
        let (sym, len) = table.decode(0).expect("entry should be present");
        assert_eq!(sym, 256);
        assert_eq!(len, 7);
    }

    #[test]
    fn huffman_fixed_symbol_0_length_8() {
        let table = HuffmanTable::fixed_literal_length();
        // Symbol 0 has canonical code 48 (0b00110000), reversed for 8 bits = 12.
        let (sym, len) = table.decode(12).expect("entry should be present");
        assert_eq!(sym, 0);
        assert_eq!(len, 8);
    }

    #[test]
    fn huffman_empty_table_returns_none() {
        let table = HuffmanTable::new_empty();
        assert_eq!(table.decode(0), None);
        assert_eq!(table.decode(511), None);
    }

    // ── SimdDeflateDecoder ───────────────────────────────────────────────────

    fn deflate_compress(data: &[u8]) -> Vec<u8> {
        use flate2::write::DeflateEncoder;
        use std::io::Write;
        let mut enc = DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn simd_decoder_matches_flate2_reference() {
        let original = b"the quick brown fox jumps over the lazy dog".repeat(20);
        let compressed = deflate_compress(&original);

        let mut dec = SimdDeflateDecoder::new();
        let mut out = Vec::new();
        out.extend_from_slice(&dec.feed(&compressed).unwrap());
        out.extend_from_slice(&Box::new(dec).finish().unwrap());
        assert_eq!(out, original);
    }

    #[test]
    fn simd_decoder_one_byte_chunks_matches_single_chunk() {
        let original = b"hello world repeat ".repeat(30);
        let compressed = deflate_compress(&original);

        let mut dec_chunked = SimdDeflateDecoder::new();
        let mut chunked_out = Vec::new();
        for byte in &compressed {
            chunked_out.extend_from_slice(&dec_chunked.feed(std::slice::from_ref(byte)).unwrap());
        }
        chunked_out.extend_from_slice(&Box::new(dec_chunked).finish().unwrap());
        assert_eq!(chunked_out, original);
    }

    #[test]
    fn simd_decoder_large_input_correct_size() {
        let original: Vec<u8> = (0u8..=255).cycle().take(1024 * 1024).collect();
        let compressed = deflate_compress(&original);

        let mut dec = SimdDeflateDecoder::new();
        let mut out = Vec::new();
        // Feed in 64 KB chunks.
        for chunk in compressed.chunks(65_536) {
            out.extend_from_slice(&dec.feed(chunk).unwrap());
        }
        out.extend_from_slice(&Box::new(dec).finish().unwrap());
        assert_eq!(out.len(), original.len());
        assert_eq!(out, original);
    }
}
