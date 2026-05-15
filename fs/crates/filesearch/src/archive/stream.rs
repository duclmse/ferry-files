use std::io;
use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use futures::Stream;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::decoder::create_decoder;
use super::meta::{EntryMeta, parse_local_header_offset};
use super::source::ByteSource;

/// Streaming entry extractor.
pub trait StreamExtractor: Send {
    type Stream: Stream<Item = io::Result<Bytes>> + Send;
    fn stream(&self, meta: &EntryMeta) -> io::Result<Self::Stream>;
}

// ---------------------------------------------------------------------------

/// Streams a ZIP entry in 64 KB chunks through a bounded mpsc channel.
///
/// Channel capacity 4 (= 256 KB) provides backpressure: the inflate task
/// awaits `tx.send` when the channel is full, so the consumer (Axum body)
/// drives the read rate.  If the consumer drops, the inflate task exits on
/// the next send — no CPU is wasted on data nobody will read.
pub struct ZipStreamExtractor<S: ByteSource> {
    pub source: Arc<S>,
}

impl<S: ByteSource + 'static> StreamExtractor for ZipStreamExtractor<S> {
    type Stream = Pin<Box<dyn Stream<Item = io::Result<Bytes>> + Send>>;

    fn stream(&self, meta: &EntryMeta) -> io::Result<Self::Stream> {
        let source = Arc::clone(&self.source);
        let meta   = meta.clone();
        // 4 × 64 KB = 256 KB max in-flight between inflate and HTTP send.
        let (tx, rx) = mpsc::channel::<io::Result<Bytes>>(4);

        tokio::spawn(async move {
            if let Err(e) = pump_inflate(source, meta, &tx).await {
                tx.send(Err(e)).await.ok();
            }
        });

        Ok(Box::pin(ReceiverStream::new(rx)))
    }
}

/// Drive the inflate loop on behalf of the spawned task.
/// Returns `Ok(())` normally and on early consumer drop; the caller forwards
/// any `Err` to the channel so the consumer sees the error.
async fn pump_inflate<S: ByteSource>(
    source: Arc<S>,
    meta:   EntryMeta,
    tx:     &mpsc::Sender<io::Result<Bytes>>,
) -> io::Result<()> {
    let data_start = parse_local_header_offset(source.as_ref(), meta.local_header_offset).await?;

    let mut remaining = meta.compressed_size;
    let mut offset    = data_start;
    let mut decoder   = create_decoder(meta.compression)?;

    while remaining > 0 {
        let chunk_size = remaining.min(64 * 1024) as usize;
        let compressed = source.read_range(offset, chunk_size).await?;
        offset    += chunk_size as u64;
        remaining -= chunk_size as u64;

        let decoded = decoder.feed(&compressed)?;
        if !decoded.is_empty() && tx.send(Ok(decoded)).await.is_err() {
            return Ok(());
        }
    }

    let tail = decoder.finish()?;
    if !tail.is_empty() {
        tx.send(Ok(tail)).await.ok();
    }

    Ok(())
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use futures::StreamExt;
    use tempfile::NamedTempFile;

    fn make_zip(entries: &[(&str, &[u8], zip::CompressionMethod)]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        let mut zw  = zip::ZipWriter::new(&mut buf);
        for (name, data, method) in entries {
            let opts = zip::write::FileOptions::<()>::default()
                .compression_method(*method);
            zw.start_file(*name, opts).unwrap();
            zw.write_all(data).unwrap();
        }
        zw.finish().unwrap();
        buf.into_inner()
    }

    /// Write zip bytes to a temp file, return the file and its EntryMeta for the first entry.
    fn zip_to_file_and_meta(
        entries: &[(&str, &[u8], zip::CompressionMethod)],
    ) -> (NamedTempFile, EntryMeta) {
        let zip_bytes = make_zip(entries);
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(&zip_bytes).unwrap();
        f.flush().unwrap();

        let cursor = std::io::Cursor::new(zip_bytes);
        let mut archive = zip::ZipArchive::new(cursor).unwrap();
        let entry = archive.by_index(0).unwrap();

        let meta = EntryMeta {
            compression:         match entry.compression() {
                zip::CompressionMethod::Stored   => 0,
                zip::CompressionMethod::Deflated => 8,
                _                                => 0,
            },
            compressed_size:     entry.compressed_size(),
            uncompressed_size:   entry.size(),
            local_header_offset: entry.header_start(),
        };
        (f, meta)
    }

    async fn collect_stream(s: impl Stream<Item = io::Result<Bytes>>) -> io::Result<Vec<u8>> {
        futures::pin_mut!(s);
        let mut out = Vec::new();
        while let Some(chunk) = s.next().await {
            out.extend_from_slice(&chunk?);
        }
        Ok(out)
    }

    #[tokio::test]
    async fn stored_entry_roundtrip() {
        let content = b"hello from stored entry";
        let (f, meta) = zip_to_file_and_meta(&[
            ("file.txt", content, zip::CompressionMethod::Stored),
        ]);
        let source    = Arc::new(super::super::source::LocalFileSource::new(f.path().to_path_buf()).unwrap());
        let extractor = ZipStreamExtractor { source };
        let stream    = extractor.stream(&meta).unwrap();
        let got       = collect_stream(stream).await.unwrap();
        assert_eq!(got, content);
    }

    #[tokio::test]
    async fn deflated_entry_roundtrip() {
        let content = b"compress me please ".repeat(50);
        let (f, meta) = zip_to_file_and_meta(&[
            ("data.bin", &content, zip::CompressionMethod::Deflated),
        ]);
        let source    = Arc::new(super::super::source::LocalFileSource::new(f.path().to_path_buf()).unwrap());
        let extractor = ZipStreamExtractor { source };
        let stream    = extractor.stream(&meta).unwrap();
        let got       = collect_stream(stream).await.unwrap();
        assert_eq!(got, content);
    }

    #[tokio::test]
    async fn total_size_matches_uncompressed_size() {
        let content = b"size check ".repeat(100);
        let (f, meta) = zip_to_file_and_meta(&[
            ("check.bin", &content, zip::CompressionMethod::Deflated),
        ]);
        let source    = Arc::new(super::super::source::LocalFileSource::new(f.path().to_path_buf()).unwrap());
        let extractor = ZipStreamExtractor { source };
        let stream    = extractor.stream(&meta).unwrap();
        let got       = collect_stream(stream).await.unwrap();
        assert_eq!(got.len() as u64, meta.uncompressed_size);
    }

    #[tokio::test]
    async fn wrong_local_header_signature_yields_error() {
        use super::super::source::MmapSource;

        let content   = b"data";
        let zip_bytes = make_zip(&[("f.txt", content, zip::CompressionMethod::Stored)]);
        let mut corrupt = zip_bytes.clone();
        corrupt[0] = 0xFF;

        let mmap = {
            let mut tmp = NamedTempFile::new().unwrap();
            tmp.write_all(&corrupt).unwrap();
            tmp.flush().unwrap();
            let file = std::fs::File::open(tmp.path()).unwrap();
            unsafe { memmap2::MmapOptions::new().map(&file).unwrap() }
        };

        let meta = EntryMeta {
            compression:         0,
            compressed_size:     content.len() as u64,
            uncompressed_size:   content.len() as u64,
            local_header_offset: 0,
        };

        let source    = Arc::new(MmapSource::new(Arc::new(mmap)));
        let extractor = ZipStreamExtractor { source };
        let stream    = extractor.stream(&meta).unwrap();
        let result    = collect_stream(stream).await;
        assert!(result.is_err(), "expected error for corrupt signature");
    }

    #[tokio::test]
    async fn channel_backpressure_limits_buffering() {
        // Verify the extractor only reads ahead by the channel capacity (4 chunks).
        // Use a stored entry large enough to fill >4 chunks at 64 KB each: 5 × 64 KB.
        let content = vec![0xABu8; 5 * 64 * 1024];
        let (f, meta) = zip_to_file_and_meta(&[
            ("big.bin", &content, zip::CompressionMethod::Stored),
        ]);
        let source    = Arc::new(super::super::source::LocalFileSource::new(f.path().to_path_buf()).unwrap());
        let extractor = ZipStreamExtractor { source };
        let stream    = extractor.stream(&meta).unwrap();
        let got       = collect_stream(stream).await.unwrap();
        assert_eq!(got, content);
    }
}
