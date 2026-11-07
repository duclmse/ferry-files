use std::io;

use bytes::Bytes;

/// Streaming decompressor interface. One instance is created per ZIP entry
/// extraction; `feed` is called once per 64 KB chunk, `finish` drains any
/// internally buffered tail bytes.
pub trait Decoder: Send {
    /// Feed a compressed chunk; returns decompressed bytes (may be empty if
    /// the decompressor is buffering internally).
    fn feed(&mut self, input: &[u8]) -> io::Result<Bytes>;

    /// Flush any remaining buffered output and consume the decoder.
    fn finish(self: Box<Self>) -> io::Result<Bytes>;
}

// ---------------------------------------------------------------------------
// StoredDecoder - compression method 0 (no compression)
// ---------------------------------------------------------------------------

pub struct StoredDecoder;

impl Decoder for StoredDecoder {
    fn feed(&mut self, input: &[u8]) -> io::Result<Bytes> {
        Ok(Bytes::copy_from_slice(input))
    }

    fn finish(self: Box<Self>) -> io::Result<Bytes> {
        Ok(Bytes::new())
    }
}

// ---------------------------------------------------------------------------
// DeflateDecoder - compression method 8
// ---------------------------------------------------------------------------

/// Wraps `flate2::write::DeflateDecoder<Vec<u8>>`. Writing compressed bytes
/// decompresses them into the inner Vec; `get_mut()` lets us drain and clear
/// the Vec between chunks, so only the current chunk is in RAM at any time.
pub struct DeflateDecoder {
    inner: flate2::write::DeflateDecoder<Vec<u8>>,
}

impl DeflateDecoder {
    pub fn new() -> Self {
        Self {
            inner: flate2::write::DeflateDecoder::new(Vec::new()),
        }
    }
}

impl Default for DeflateDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder for DeflateDecoder {
    fn feed(&mut self, input: &[u8]) -> io::Result<Bytes> {
        use std::io::Write;
        self.inner.write_all(input)?;
        let buf = self.inner.get_mut();
        let out = Bytes::copy_from_slice(buf);
        buf.clear();
        Ok(out)
    }

    fn finish(mut self: Box<Self>) -> io::Result<Bytes> {
        self.inner
            .try_finish()
            .map_err(|e| io::Error::other(e.to_string()))?;
        Ok(Bytes::from(std::mem::take(self.inner.get_mut())))
    }
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/// Create the right decoder for a ZIP compression method code.
/// Returns `io::Error` with `ErrorKind::Unsupported` for unknown methods.
pub fn create_decoder(compression: u16) -> io::Result<Box<dyn Decoder>> {
    use super::inflate::SimdDeflateDecoder;
    match compression {
        0 => Ok(Box::new(StoredDecoder)),
        8 => Ok(Box::new(SimdDeflateDecoder::new())),
        m => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("unsupported ZIP compression method {m}"),
        )),
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn deflate_compress(data: &[u8]) -> Vec<u8> {
        let mut enc =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn stored_feed_returns_same_bytes() {
        let mut dec = StoredDecoder;
        let input = b"hello world";
        let out = dec.feed(input).unwrap();
        assert_eq!(&out[..], input);
    }

    #[test]
    fn stored_finish_is_empty() {
        let dec = Box::new(StoredDecoder);
        assert!(dec.finish().unwrap().is_empty());
    }

    #[test]
    fn deflate_single_feed_roundtrip() {
        let original = b"the quick brown fox jumps over the lazy dog".repeat(10);
        let compressed = deflate_compress(&original);

        let mut dec = DeflateDecoder::new();
        let mut out = Vec::new();
        out.extend_from_slice(&dec.feed(&compressed).unwrap());
        out.extend_from_slice(&Box::new(dec).finish().unwrap());

        assert_eq!(out, original);
    }

    #[test]
    fn deflate_two_chunks_same_as_one() {
        let original = b"repeat this data many times ".repeat(20);
        let compressed = deflate_compress(&original);
        let mid = compressed.len() / 2;

        let mut dec = DeflateDecoder::new();
        let mut out = Vec::new();
        out.extend_from_slice(&dec.feed(&compressed[..mid]).unwrap());
        out.extend_from_slice(&dec.feed(&compressed[mid..]).unwrap());
        out.extend_from_slice(&Box::new(dec).finish().unwrap());

        assert_eq!(out, original);
    }

    #[test]
    fn unsupported_method_returns_error() {
        match create_decoder(12) {
            Err(e) => assert_eq!(e.kind(), io::ErrorKind::Unsupported),
            Ok(_) => panic!("expected error for unsupported compression method"),
        }
    }
}
