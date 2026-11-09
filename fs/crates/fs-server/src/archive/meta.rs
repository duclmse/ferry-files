use std::io;

use bytes::Bytes;

use super::source::ByteSource;

/// Everything needed to extract a single ZIP entry, derived from the central
/// directory (.zidx node) plus the local file header.
#[derive(Debug, Clone)]
pub struct EntryMeta {
    pub compression: u16,
    pub compressed_size: u64,
    pub uncompressed_size: u64,
    pub local_header_offset: u64,
}

/// Parse the ZIP local file header at `source[local_header_offset..]` and
/// return the byte offset where compressed data begins.
///
/// Local header layout (PKZIP spec §4.3.7):
///   signature(4) version_needed(2) flags(2) method(2) mod_time(2) mod_date(2)
///   crc32(4) compressed_size(4) uncompressed_size(4)
///   filename_len(2) extra_len(2) filename(n) extra(m)
///
/// Data starts at: local_header_offset + 30 + filename_len + extra_len
pub async fn parse_local_header_offset(
    source: &dyn ByteSource,
    local_header_offset: u64,
) -> io::Result<u64> {
    let header: Bytes = source
        .read_range(local_header_offset, 30)
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::UnexpectedEof, e))?;

    if &header[0..4] != b"PK\x03\x04" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid local file header signature",
        ));
    }

    let fname_len = u16::from_le_bytes([header[26], header[27]]) as u64;
    let extra_len = u16::from_le_bytes([header[28], header[29]]) as u64;
    Ok(local_header_offset + 30 + fname_len + extra_len)
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal synthetic local file header in memory.
    fn make_local_header(fname: &[u8], extra: &[u8]) -> Vec<u8> {
        let mut hdr = Vec::new();
        hdr.extend_from_slice(b"PK\x03\x04"); // signature
        hdr.extend_from_slice(&[0u8; 22]); // version..uncompressed_size (22 bytes)
        hdr.extend_from_slice(&(fname.len() as u16).to_le_bytes()); // filename_len
        hdr.extend_from_slice(&(extra.len() as u16).to_le_bytes()); // extra_len
        hdr.extend_from_slice(fname);
        hdr.extend_from_slice(extra);
        hdr
    }

    struct SliceSource(Vec<u8>);

    #[async_trait::async_trait]
    impl ByteSource for SliceSource {
        async fn read_range(&self, offset: u64, len: usize) -> io::Result<Bytes> {
            let start = offset as usize;
            let end = start
                .checked_add(len)
                .filter(|&e| e <= self.0.len())
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "eof"))?;
            Ok(Bytes::copy_from_slice(&self.0[start..end]))
        }
    }

    #[tokio::test]
    async fn valid_header_returns_correct_offset() {
        let fname = b"hello.txt";
        let extra = b"EX";
        let data = make_local_header(fname, extra);
        let src = SliceSource(data);
        let off = parse_local_header_offset(&src, 0).await.unwrap();
        assert_eq!(off, 30 + fname.len() as u64 + extra.len() as u64);
    }

    #[tokio::test]
    async fn valid_header_at_nonzero_offset() {
        let fname = b"f";
        let extra = b"";
        let padding = vec![0u8; 100];
        let mut data = padding.clone();
        data.extend_from_slice(&make_local_header(fname, extra));
        let src = SliceSource(data);
        let off = parse_local_header_offset(&src, 100).await.unwrap();
        assert_eq!(off, 100 + 30 + 1);
    }

    #[tokio::test]
    async fn wrong_signature_is_invalid_data() {
        let mut data = vec![0u8; 30];
        data[0] = b'X'; // wrong signature
        let src = SliceSource(data);
        let err = parse_local_header_offset(&src, 0).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn truncated_header_is_eof_error() {
        let src = SliceSource(vec![0u8; 10]); // less than 30 bytes
        let err = parse_local_header_offset(&src, 0).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }
}
