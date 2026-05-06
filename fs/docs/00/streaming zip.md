Below is a concrete design to **bring streaming/low-overhead into a
`zip`-compatible ecosystem** without forking yourself into a maintenance hole.
The right approach is to **split responsibilities** and introduce a **streaming
core** that both `zip` (for compatibility) and your backend (for performance)
can use.

---

# 0) Target end-state

```text
┌─────────────────────────────┐
│        API Layer            │ (dir, file, range)
└──────────┬──────────────────┘
           │
┌──────────▼──────────────────┐
│   Archive Engine (yours)    │
│  - mmap index (.zidx)       │
│  - scheduler                │
│  - streaming extractor      │
└───┬────────────────────┬────┘
    │                    │
┌───▼──────────┐  ┌──────▼────────┐
│  zip crate   │  │ streaming core│
│ (compat/tool)│  │ (new layer)   │
└──────────────┘  └───────────────┘
```

---

# 1) Principle: separate “format parsing” from “data execution”

## Today (zip crate)

```text
ZipArchive → ZipFile → read() → inflate internally
```

Problems:

- tightly coupled parsing + decoding
- no control over IO
- no streaming pipeline control

---

## New design

Split into 3 layers:

```text
[Parser] → [Entry Metadata] → [Streaming Executor]
```

---

# 2) Define a new core trait layer

This is the key abstraction that “adds s-zip strength”.

## 2.1 Entry metadata (zero-copy friendly)

```rust
pub struct EntryMeta {
    pub name: Arc<str>,
    pub compression: u16,
    pub compressed_size: u64,
    pub uncompressed_size: u64,
    pub local_header_offset: u64,
}
```

No buffers, no readers. Just metadata.

---

## 2.2 Byte source abstraction

```rust
#[async_trait::async_trait]
pub trait ByteSource: Send + Sync {
    async fn read_range(&self, offset: u64, len: usize) -> io::Result<Bytes>;
}
```

Implementations:

- local file or mmap-backed source
- HTTP Range client
- in-memory test source

This is where you gain flexibility.

---

## 2.3 Streaming extractor

```rust
pub trait StreamExtractor {
    type Stream: futures::Stream<Item = io::Result<Bytes>>;

    fn stream(&self, entry: &EntryMeta) -> io::Result<Self::Stream>;
}
```

---

# 3) Implement the “s-zip strength”: streaming pipeline

## 3.1 Pipeline model

```text
ByteSource → RangeReader → Decoder → Chunk Stream → Client
```

---

## 3.2 Concrete implementation

```rust
pub struct ZipStreamExtractor<S: ByteSource> {
    source: Arc<S>,
}

impl<S: ByteSource> StreamExtractor for ZipStreamExtractor<S> {
    type Stream = Pin<Box<dyn Stream<Item = io::Result<Bytes>> + Send>>;

    fn stream(&self, entry: &EntryMeta) -> io::Result<Self::Stream> {
        let source = self.source.clone();

        Ok(Box::pin(async_stream::try_stream! {
            // 1. read local header (note .await — read_range is async)
            let header = source.read_range(entry.local_header_offset, 64).await?;

            let data_offset = parse_local_header(&header)?;

            // 2. stream compressed data in chunks
            let mut remaining = entry.compressed_size;
            let mut offset = entry.local_header_offset + data_offset;

            let mut decoder = create_decoder(entry.compression);

            while remaining > 0 {
                let chunk_size = remaining.min(64 * 1024) as usize;
                let data = source.read_range(offset, chunk_size).await?;

                offset += chunk_size as u64;
                remaining -= chunk_size as u64;

                let decoded = decoder.feed(&data)?;
                yield decoded;
            }

            let final_chunk = decoder.finish()?;
            if !final_chunk.is_empty() {
                yield final_chunk;
            }
        }))
    }
}
```

---

# 4) Integrating with `zip` crate (without rewriting it)

## Strategy: use `zip` as a _backend adapter_

### 4.1 Adapter layer

```rust
pub struct ZipAdapter {
    archive: zip::ZipArchive<File>,
}

impl ZipAdapter {
    pub fn entries(&mut self) -> zip::result::ZipResult<Vec<EntryMeta>> {
        (0..self.archive.len())
            .map(|i| {
                let f = self.archive.by_index(i)?;

                Ok(EntryMeta {
                    name: Arc::from(f.name()),
                    compression: compression_to_u16(f.compression()),
                    compressed_size: f.compressed_size(),
                    uncompressed_size: f.size(),
                    // `zip` exposes the compressed data offset. If the engine
                    // needs the local header offset, store that separately from
                    // a custom central-directory parser.
                    local_header_offset: f.data_start(),
                })
            })
            .collect()
    }
}
```

`compression_to_u16` is a local adapter that maps `zip::CompressionMethod` to
the numeric method ID used by the archive engine.

---

### 4.2 Why this works

- you reuse `zip` for correctness
- you bypass its read path entirely
- you plug into your streaming engine

---

# 5) Decoder layer (pluggable)

## Interface

```rust
trait Decoder {
    fn feed(&mut self, input: &[u8]) -> io::Result<Bytes>;
    fn finish(&mut self) -> io::Result<Bytes>;
}
```

---

## Implementations

### Baseline

- flate2

### High-performance upgrade

- native miniz/libdeflate adapter if profiling justifies it
- platform SIMD copy paths where they are safe and measured

---

# 6) Advanced: true streaming (beyond zip crate)

To fully match “s-zip strength”:

## Add central directory bypass

- parse EOCD manually (you already designed this)
- avoid `zip::ZipArchive` entirely in production

So:

```text
zip crate → dev / validation
custom parser → production
```

---

# 7) Add backpressure-aware streaming

Integrate with your scheduler:

```rust
while let Some(chunk) = stream.next().await {
    scheduler.await_budget(cost_of(chunk));
    yield chunk;
}
```

This is where your system becomes _coherent_.

---

# 8) Optional: push further (true advantage)

## A. Chunk-aligned decoding

- reuse buffers
- reduce allocations

## B. Prefetching

```rust
spawn_prefetch(offset + next_chunk)
```

## C. Zero-copy to network

- use `Bytes`
- avoid `Vec<u8>`

---

# 9) What you gain

| Feature               | zip crate | Your design |
| --------------------- | --------- | ----------- |
| Streaming             | limited   | full        |
| Random access         | partial   | precise     |
| Backpressure          | none      | integrated  |
| Zero-copy             | no        | yes         |
| Scheduler integration | no        | yes         |

---

# 10) What you avoid

- forking `zip`
- maintaining full ZIP implementation
- rewriting edge-case handling

---

# 11) Recommended build order

1. Define `EntryMeta + ByteSource`
2. Implement streaming extractor
3. Plug `zip` adapter (quick win)
4. Replace parser with custom central dir
5. Add scheduler integration
6. Optimize decoder

---

# Bottom line

You don’t “merge s-zip into zip”.

You:

> **extract the streaming execution model and make `zip` just one metadata
> provider**

That gives you:

- correctness from `zip`
- performance and control from your architecture
