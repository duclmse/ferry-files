# `.zidx` vs `.dzx` - comparison and combination analysis

Both are mmap-backed binary sidecar files that make ZIP archives servable over
HTTP without a full unzip. They solve different problems at different
granularities and currently ship as two independent formats (`zidx/` and
`archive/dzx.rs`). This note compares them precisely and asks whether merging
them into one file format is worth doing.

## 1. What each one is for

|                     | `.zidx`                                                                             | `.dzx`                                                                                                                      |
| ------------------- | ----------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------- |
| Question it answers | "What files/dirs does this ZIP contain, and where does each entry start?"           | "Where inside _this one_ compressed entry can I resume DEFLATE decoding, mid-stream, without decompressing from the start?" |
| Scope               | Whole archive                                                                       | One ZIP entry                                                                                                               |
| Built for           | Every registered archive                                                            | Only deflate entries ≥ `dzx_min_entry_bytes` (default 10 MiB)                                                               |
| Count per ZIP       | Exactly 1                                                                           | 0..N (one per qualifying large entry)                                                                                       |
| Built by            | `ZidxBuilder::build` - reads only ZIP central-directory metadata, no decompression  | `DzxBuilder::build` - decompresses the entire entry once to record block boundaries + dictionary snapshots                  |
| Consumed for        | Directory listing, path→node resolution, search, resolving a node ID to `EntryMeta` | `Range:` reads on one large entry (`GET /archive/:id/file`)                                                                 |
| Path keying         | `blake3(zip_path)` → `{zidx_dir}/<hash>.zidx`                                       | `blake3(zip_path + "#" + local_header_offset)` → `{dzx_dir}/<hash>.dzx`                                                     |

In short: `.zidx` is the archive's **table of contents**; `.dzx` is a
**per-entry seek table** for the (rare) entries too large to fully inflate just
to serve a byte range.

## 2. Binary layout, side by side

### `.zidx` (`crates/filesearch/src/zidx/mod.rs`)

```
[Header - 20 bytes]
[Node Table - node_count × 48 bytes]
[String Pool - string_bytes bytes, UTF-8]
```

- Header: magic `ZIDX`, version (u32, currently 2), `node_count`,
  `string_bytes`, `root_node` (always 0).
- Node (48 bytes, fixed size, one per file **and** implicit directory):
  `parent`, `first_child`, `next_sibling` (u32 tree links, `u32::MAX` = null),
  `name_off`/`name_len` (into the string pool), `kind` (0=file, 1=dir),
  `compression`, `compressed_size`, `uncompressed_size`, `local_header_offset`
  (u64 - where this entry's ZIP local header starts).
- String pool: deduplicated, referenced by offset/length from nodes.

Every node's byte offset is `HEADER_SIZE + id * 48` - direct pointer arithmetic
over the mmap, no parsing, no variable-length records in the node table itself.

### `.dzx` (`crates/filesearch/src/archive/dzx.rs`)

```
[Header - 32 bytes]
[Block Table - block_count × 56 bytes]
[Window Snapshots - block_count × 32768 bytes]
```

- Header: magic `DZX1`, version (u32, currently 1), `block_size`, `flags` (bit 0
  = has_window_snapshots), `uncompressed_size` (u64), `block_count` (u64).
- Block entry (56 bytes): `comp_offset`/`uncomp_offset` (u64, position within
  the entry's compressed/uncompressed streams), `comp_size`/ `uncomp_size`
  (u32), `bit_count`/`bit_buffer` (reserved for partial-byte resume; currently
  always zero because blocks are recorded at Z_SYNC_FLUSH byte boundaries),
  `snapshot_idx` (u32), padding to 56.
- Window snapshots: one raw **32 KB** blob per block - the DEFLATE sliding
  window's dictionary contents at that block's start, needed because DEFLATE
  back-references can point up to 32 KB behind the current position, and you
  can't have those bytes without decompressing from the entry's start unless you
  snapshot the window.

The window snapshots dominate the file size: a 100 MB entry indexed at the
default 64 KB block size produces roughly 1,600 blocks × 32 KB ≈ 50 MB of
snapshot data - the index can be a substantial fraction of the entry itself for
finely-grained blocking. `.zidx` has no equivalent cost; its size is
`O(entry count)`, not `O(entry size)`.

## 3. How they're used together on the read path

```
GET /archive/:id/file?node=<id>  (with optional Range: header)
        │
        ▼
ArchiveRegistry.index(id)            → Arc<ZidxIndex>  (cached, mmap)
ZidxIndex.node(node_id)              → NodeData { local_header_offset, sizes, compression, ... }
        │
        ├─ no Range, or Range but no .dzx for this entry:
        │      ZipStreamExtractor streams the whole entry through pump_inflate
        │
        └─ Range header present:
               dzx_path_for(dzx_dir, zip_path, local_header_offset)
               DzxIndex::open(dzx_path)   → find nearest block ≤ requested offset
               seed decoder from that block's window snapshot
               decode forward only as far as needed
               respond 206 Partial Content
```

`.zidx` is on the critical path for _every_ archive request (it's how a `node`
query param resolves to an `EntryMeta` at all). `.dzx` is only consulted, and
only exists, for the subset of large deflated entries where full-entry
decompression would be wasteful for a small Range request. `DzxBuilder` itself
depends on `.zidx` at build time - `build_dzx_sidecars` in `indexer/crawler.rs`
iterates the just-built `.zidx` tree to find qualifying nodes before building
`.dzx` for each one - but that's a build-time dependency, not a read-time one.

## 4. Can they be combined into one file format?

Technically yes - for example, by adding a variable-length "extra data" region
per node (an offset + length into a new section) and inlining each qualifying
entry's block table + window snapshots there instead of in a separate `.dzx`
file. It's not recommended, for reasons that follow directly from the
differences in §1–§3.

### Why keeping them separate is the better trade-off

1. **Cardinality mismatch makes a fixed node layout hard to keep.** `.zidx`'s
   48-byte fixed-size node record is what makes `node(id)` an O(1) mmap offset
   computation with no parsing. The overwhelming majority of nodes (small files,
   all directories, all Stored entries) will never have `.dzx` data. Embedding
   it inline means either a sparse offset/length field on every node (paid by
   every request that never uses it) or a variable-length side table that
   reintroduces the parsing `.zidx` was designed to avoid.

2. **Wildly different build costs and independent invalidation.** `.zidx` build
   only reads ZIP central-directory metadata - cheap, always done on crawl.
   `.dzx` build decompresses the _entire_ entry once, is rate-limited by
   `dzx_max_concurrent_builds`, and can take seconds for a large file. They're
   independent async tasks today (`indexer/crawler.rs`) precisely so a slow
   `.dzx` build for one huge entry never blocks the (fast, always-needed)
   `.zidx` from being ready. Merging the files means a `.dzx` rebuild - e.g.
   after changing `dzx_min_entry_bytes` or `block_size` - would require
   rewriting the entire combined file, including directory metadata that didn't
   change.

3. **mmap lifetime and concurrent-writer safety.** `ZidxIndex` and `DzxIndex`
   are both opened once and mmap'd; in-flight requests hold a live mmap. `.dzx`
   sidecars are built asynchronously, potentially while the archive's `.zidx` is
   already open and being read by concurrent requests. Keeping them as separate
   files means a `.dzx` write never touches bytes an existing `.zidx` mmap
   reader can see. A combined file would need in-place patching of a file that
   other readers may have mapped, or a full atomic rewrite-and-swap for what is
   otherwise a small, isolated, per-entry addition.

4. **Failure isolation.** `.dzx` build can legitimately fail (e.g. no
   Z_SYNC_FLUSH resync point found within `MAX_PROBE_BYTES`) without affecting
   archive browsing at all - the entry just falls back to full streaming. That
   failure is invisible to `.zidx` today. A combined format would make every
   corruption or partial-write scenario in the rare `.dzx` path a risk to the
   common `.zidx` path.

5. **The actual overlap is a single `u64`.** The only thing `.dzx` fundamentally
   needs from `.zidx` is `local_header_offset` (to key the sidecar) plus
   `compressed_size`/`uncompressed_size`/`compression` (to know whether/how to
   build it) - all of which `build_dzx_sidecars` already has in hand from the
   `.zidx` node it's iterating, without needing the two files to share physical
   storage.

### What merging would actually buy

- One fewer keying scheme/sidecar directory to reason about (today: two
  independent `blake3`-based path functions, `zidx_path_for` and
  `dzx_path_for`).
- One fewer `mmap`/file-open per Range request that currently opens a `.dzx` in
  addition to the already-cached `.zidx`.

Both are minor, fixed, one-time-per-request costs, not proportional to data size
- and they don't offset the added complexity of a variable-length node format,
harder build-invalidation, and weaker failure isolation described above.

## 5. Recommendation

Keep `.zidx` and `.dzx` as separate files. They are different indexes with
different cardinalities (1-per-archive vs 0..N-per-archive), different build
costs (metadata-only vs full-decompress), and different consumers (every request
vs Range-on-large-deflate-entry only). The current design already shares the
only piece of state that actually needs to be shared - `local_header_offset` as
the join key - without paying for a merged file format's downsides.

If the per-request `.dzx` open ever shows up as measurable overhead, the cheaper
fix is caching open `DzxIndex` handles in `ArchiveRegistry` (mirroring the
existing `index_cache`/`source_cache` pattern), not merging the file formats.
