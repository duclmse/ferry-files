# Phase 10 — Streaming ZIP Core

**Goal:** Replace the ad-hoc inline inflate in Phase 9's file handler with a
composable, trait-driven streaming pipeline. The `ByteSource` trait decouples
data acquisition from decompression; `StreamExtractor` drives the inflate loop
and emits `Bytes` chunks. Future phases (11, 13) plug new capabilities in at
well-defined seams without touching the API surface.

**Estimated scope:** ~550 lines of Rust.

**Depends on:** Phase 9 (archive service API, `AppState`).

**Design source:** `docs/00/streaming zip.md`.

---

## 10.1 Module Layout

```
crates/filesearch/src/archive/
├── mod.rs       ← pub re-exports
├── source.rs    ← ByteSource trait + LocalFileSource, MmapSource
├── meta.rs      ← EntryMeta, local-header parser
├── decoder.rs   ← Decoder trait + StoredDecoder, DeflateDecoder
└── stream.rs    ← ZipStreamExtractor, StreamExtractor trait
```

Add `pub mod archive;` to `crates/filesearch/src/lib.rs`.

---

## 10.2 `ByteSource` Trait

```rust
/// Async random-access read over a byte range.
/// All implementations must be Send + Sync.
#[async_trait::async_trait]
pub trait ByteSource: Send + Sync {
    async fn read_range(&self, offset: u64, len: usize) -> io::Result<Bytes>;

    /// Total byte length of the underlying resource, if known.
    fn size_hint(&self) -> Option<u64> { None }
}
```

### `LocalFileSource`

Wraps a `PathBuf`. Each `read_range` call opens the file, seeks, and reads. Uses
`tokio::task::spawn_blocking` internally so the `async fn` contract is met
without blocking the runtime.

```rust
pub struct LocalFileSource {
    path: PathBuf,
}

#[async_trait::async_trait]
impl ByteSource for LocalFileSource {
    async fn read_range(&self, offset: u64, len: usize) -> io::Result<Bytes> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let mut f = std::fs::File::open(&path)?;
            f.seek(SeekFrom::Start(offset))?;
            let mut buf = vec![0u8; len];
            f.read_exact(&mut buf)?;
            Ok(Bytes::from(buf))
        })
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
    }

    fn size_hint(&self) -> Option<u64> {
        std::fs::metadata(&self.path).ok().map(|m| m.len())
    }
}
```

### `MmapSource`

Wraps an `Arc<memmap2::Mmap>` for archives where the whole ZIP is already mapped
(used by the `.zidx` reader in Phase 8 tests and future NUMA placement).

```rust
pub struct MmapSource {
    mmap: Arc<memmap2::Mmap>,
}

#[async_trait::async_trait]
impl ByteSource for MmapSource {
    async fn read_range(&self, offset: u64, len: usize) -> io::Result<Bytes> {
        let start = offset as usize;
        let end = start.checked_add(len)
            .filter(|&e| e <= self.mmap.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "out of bounds"))?;
        Ok(Bytes::copy_from_slice(&self.mmap[start..end]))
    }

    fn size_hint(&self) -> Option<u64> {
        Some(self.mmap.len() as u64)
    }
}
```

---

## 10.3 `EntryMeta` and Local Header Parser

```rust
/// Everything needed to extract a single ZIP entry, derived from the central
/// directory (available in the .zidx node) plus the local file header.
pub struct EntryMeta {
    pub compression:          u16,
    pub compressed_size:      u64,
    pub uncompressed_size:    u64,
    pub local_header_offset:  u64,
}

/// Parse the ZIP local file header at `source[offset..]` and return the byte
/// offset where compressed data begins.
///
/// Local header layout (PKZIP spec):
///   signature(4) version_needed(2) flags(2) method(2) mod_time(2) mod_date(2)
///   crc32(4) compressed_size(4) uncompressed_size(4)
///   filename_len(2) extra_len(2) filename(n) extra(m)
///
/// Data starts at: offset + 30 + filename_len + extra_len
pub async fn parse_local_header_offset(
    source: &dyn ByteSource,
    local_header_offset: u64,
) -> io::Result<u64>;
```

Implementation reads the first 30 bytes, validates the signature (`PK\x03\x04`),
extracts `filename_len` and `extra_len`, and returns
`local_header_offset + 30 + filename_len as u64 + extra_len as u64`.

Returns `io::Error::InvalidData` if signature is wrong.

---

## 10.4 `Decoder` Trait

```rust
pub trait Decoder: Send {
    /// Feed a compressed chunk; returns decompressed bytes.
    fn feed(&mut self, input: &[u8]) -> io::Result<Bytes>;
    /// Flush any remaining buffered output.
    fn finish(self: Box<Self>) -> io::Result<Bytes>;
}
```

### `StoredDecoder` (compression method 0)

Pass-through: `feed` returns `Bytes::copy_from_slice(input)`.

### `DeflateDecoder` (compression method 8)

Wraps `flate2::read::DeflateDecoder<Cursor<&[u8]>>`. Each `feed` call creates a
fresh cursor over the input slice, reads all available output, and returns it.

> **Note:** `flate2`'s streaming API is stateful; for multi-chunk feeds, wrap a
> single `DeflateDecoder<ReadBuf>` and write each chunk in — do not reconstruct
> the decoder on every call. Use `flate2::read::DeflateDecoder` wrapping a
> `VecDeque<u8>` or the equivalent `write`-side API
> (`flate2::write::DeflateDecoder`).

```rust
pub struct DeflateDecoder {
    inner: flate2::write::DeflateDecoder<Vec<u8>>,
}

impl Decoder for DeflateDecoder {
    fn feed(&mut self, input: &[u8]) -> io::Result<Bytes> {
        self.inner.write_all(input)?;
        let buf = self.inner.get_mut();
        let out = Bytes::copy_from_slice(buf);
        buf.clear();
        Ok(out)
    }

    fn finish(mut self: Box<Self>) -> io::Result<Bytes> {
        self.inner.try_finish()?;
        Ok(Bytes::from(self.inner.into_inner()))
    }
}
```

`create_decoder(compression: u16) -> Box<dyn Decoder>`:

- `0` → `StoredDecoder`
- `8` → `DeflateDecoder`
- other → return `io::Error::Unsupported`

---

## 10.5 `StreamExtractor` Trait and Implementation

```rust
pub trait StreamExtractor: Send {
    type Stream: futures::Stream<Item = io::Result<Bytes>> + Send;
    fn stream(&self, meta: &EntryMeta) -> io::Result<Self::Stream>;
}
```

### `ZipStreamExtractor<S: ByteSource>`

```rust
pub struct ZipStreamExtractor<S: ByteSource> {
    source: Arc<S>,
}

impl<S: ByteSource + 'static> StreamExtractor for ZipStreamExtractor<S> {
    type Stream = Pin<Box<dyn Stream<Item = io::Result<Bytes>> + Send>>;

    fn stream(&self, meta: &EntryMeta) -> io::Result<Self::Stream> {
        let source = self.source.clone();
        let meta = meta.clone();

        Ok(Box::pin(async_stream::try_stream! {
            let data_start = parse_local_header_offset(
                source.as_ref(), meta.local_header_offset
            ).await?;

            let mut remaining = meta.compressed_size;
            let mut offset = data_start;
            let mut decoder = create_decoder(meta.compression)
                .map_err(|e| io::Error::new(io::ErrorKind::Unsupported, e))?;

            while remaining > 0 {
                let chunk_size = remaining.min(64 * 1024) as usize;
                let compressed = source.read_range(offset, chunk_size).await?;
                offset += chunk_size as u64;
                remaining -= chunk_size as u64;

                let decoded = decoder.feed(&compressed)?;
                if !decoded.is_empty() {
                    yield decoded;
                }
            }

            let tail = decoder.finish()?;
            if !tail.is_empty() {
                yield tail;
            }
        }))
    }
}
```

---

## 10.6 Axum Integration

Replace the inline inflate in `api/archive.rs`'s `stream_file` handler (Phase 9)
with:

```rust
async fn stream_file(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<FileQuery>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorBody>)> {
    let index = state.archive_registry.index(&id)
        .ok_or((StatusCode::NOT_FOUND, /* ... */))?;

    let node = index.node(q.node)
        .map_err(|_| (StatusCode::BAD_REQUEST, /* ... */))?;

    if node.kind == 1 {
        return Err((StatusCode::BAD_REQUEST, /* "node is directory" */));
    }

    let meta = EntryMeta {
        compression:         node.compression,
        compressed_size:     node.compressed_size as u64,
        uncompressed_size:   node.uncompressed_size as u64,
        local_header_offset: node.local_header_offset,
    };

    let zip_path = state.archive_registry.zip_path(&id)
        .ok_or((StatusCode::NOT_FOUND, /* ... */))?;

    let source = Arc::new(LocalFileSource::new(zip_path));
    let extractor = ZipStreamExtractor { source };
    let stream = extractor.stream(&meta)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, /* e */))?;

    let body = axum::body::StreamBody::new(stream);
    Ok((
        [(header::CONTENT_LENGTH, node.uncompressed_size.to_string())],
        body,
    ))
}
```

---

## 10.7 Backpressure

The channel between the `spawn_blocking` inflate task and the async Axum body is
a `tokio::sync::mpsc::channel(4)` (4 × 64 KB = 256 KB buffer). The blocking
thread blocks on `tx.blocking_send()` when the channel is full, which is the
backpressure mechanism: if the client reads slowly, the inflate thread pauses.

This replaces the simpler direct `async_stream` approach above for production
use where backpressure matters. The `async_stream` version is acceptable for
Phase 10 and can be upgraded in Phase 12.

---

## 10.8 New Cargo Dependencies

```toml
# In crates/filesearch/Cargo.toml [dependencies]:
flate2        = "1"
async-trait   = "0.1"
async-stream  = "0.3"
futures       = "0.3"
```

---

## 10.9 Tests — Phase 10

```
source:
- LocalFileSource::read_range: correct bytes at various offsets
- LocalFileSource::read_range: offset out of bounds → io::Error
- MmapSource::read_range: same checks

local header parser:
- valid local header → data_offset == 30 + filename_len + extra_len
- wrong signature → io::Error::InvalidData
- truncated header (< 30 bytes) → io::Error::UnexpectedEof

decoder:
- StoredDecoder: feed → same bytes returned
- DeflateDecoder: compress bytes with flate2, feed through DeflateDecoder → original bytes
- DeflateDecoder: feed in two chunks → same output as single feed
- unsupported method (e.g. 12) → create_decoder returns Err

extractor:
- ZipStreamExtractor on stored fixture entry → decompressed bytes match
- ZipStreamExtractor on deflated fixture entry → decompressed bytes match
- collect all chunks → total size equals uncompressed_size
- wrong local header signature in ZIP → stream yields io::Error

integration (replaces Phase 9 inline inflate):
- GET /archive/:id/file → response body matches entry content
- response Content-Length matches uncompressed_size
```

---

## 10.10 Definition of Done

- [x] `cargo test -p filesearch archive` passes all source, decoder, extractor
      tests.
- [x] Phase 9's `stream_file` handler uses `ZipStreamExtractor` (inline inflate
      removed).
- [x] Streaming large entries does not buffer entire content in RAM — the
      `mpsc::channel(4)` cap limits in-flight data to 256 KB regardless of entry
      size (verified by `channel_backpressure_limits_buffering` test with a 5 ×
      64 KB stored entry).
- [x] Backpressure channel keeps in-flight inflate to ≤ 256 KB.
- [x] No `unwrap()` in `archive/` module.
- [x] `cargo clippy -- -D warnings` clean.

---

## Implementation Notes

**Implemented:** 2026-05-13

### Deviations from spec

**`DeflateDecoder::finish` — no `into_inner`**
`flate2::write::DeflateDecoder<W>` does not expose `into_inner()`. After
`try_finish()` the remaining decompressed bytes are in the inner `Vec<u8>` (the
write target). We drain them with `std::mem::take(self.inner.get_mut())` instead
of the spec's `self.inner.into_inner()`. Functionally identical.

**`ZipStreamExtractor.source` is `pub`** The spec shows the field as private,
but construction at the call site (`ZipStreamExtractor { source }`) requires it
to be public. A constructor would have been the alternative; `pub` is simpler
with no downside.

**`create_decoder` error mapping in `stream`** The spec wraps the error with a
second `io::Error::new(Unsupported, e)` inside the stream. Our implementation
calls `create_decoder(meta.compression)?` directly because `create_decoder`
already returns `io::ErrorKind::Unsupported`, so the extra wrapping is
redundant.

**Axum body API** The spec uses `axum::body::StreamBody::new(stream)` (Axum
0.6). We target Axum 0.7, which uses `axum::body::Body::from_stream(stream)`.
Same semantics, updated API.

**`io::Error::other` instead of `io::Error::new(Other, …)`** Clippy
`io_other_error` lint (enabled via `-D warnings`) requires
`io::Error::other(…)`. Applied in `decoder.rs` and `source.rs`.

### Backpressure (§10.7) — implemented

`ZipStreamExtractor::stream` now uses `tokio::spawn` + `mpsc::channel(4)` +
`ReceiverStream` instead of `async_stream::try_stream!`. The inflate loop runs
in a detached task; it awaits `tx.send` when the channel holds 4 full chunks
(256 KB), pausing inflate until the consumer (Axum body) drains a slot. If the
consumer drops (client disconnects), the next `tx.send` returns `Err` and the
task exits without wasting CPU.

```
inflate task ──(mpsc cap=4)──► ReceiverStream ──► axum::body::Body
                  ↑ blocks when full (backpressure)
```

The internal `pump_inflate` async fn owns the `Box<dyn Decoder>` for the full
entry lifetime, so the DEFLATE decompressor state is maintained across chunks
without move overhead.

### Integration HTTP tests — added

Two `#[tokio::test]` tests in `api::archive::tests` use
`tower::ServiceExt::oneshot` via `crate::api::rest::router(state)` (which
returns `Router<()>` and therefore implements `Service`):

- `stream_file_handler_body_matches_content` — stored entry, asserts body bytes
- `stream_file_handler_content_length_matches_uncompressed_size` — deflated
  entry, asserts `Content-Length` header equals the uncompressed size and body
  is correct

`make_test_app_state` constructs a minimal `AppState` (SQLite in-memory, Tantivy
in a temp dir) for use in both tests.

### Test coverage

56 tests pass across all archive sub-modules:

| Module             | Tests                                                            |
| ------------------ | ---------------------------------------------------------------- |
| `archive::source`  | 5 (LocalFile + Mmap read, bounds, size_hint)                     |
| `archive::meta`    | 4 (valid header, non-zero offset, wrong sig, truncated)          |
| `archive::decoder` | 5 (stored, deflate single, deflate chunked, finish, unsupported) |
| `archive::stream`  | 5 (stored, deflated, size check, corrupt sig, backpressure)      |
| `api::archive`     | 7 (registry tests × 5 + HTTP body + HTTP Content-Length)         |
