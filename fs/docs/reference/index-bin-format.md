# `index.bin` file format

`index.bin` is the on-disk snapshot of the memory store
(`store.mode = "memory"`, or `"auto"`, the default). It lives at
`<data_dir>/index.bin`. This document describes **version 5**, the version
`fsd` writes today: a header, then the version 4 body compressed as one zstd
frame.

Source of truth: `crates/fs-server/src/store/memory_store/` (`crud.rs` for the
header, `snapshot.rs` for the body, `state.rs` for entries).

## Layout

```
offset  size       field
0       30         header
30      data_len   body: one zstd frame that decompresses to raw_len bytes of
  arena_len   varint
  arena       arena_len bytes of UTF-8
  entry_count varint
  entries     entry_count x Entry
  exts        seq<string>
  extra       seq<(varuint32, Extra)>
  meta        seq<(string, string)>
  crawl_gen   zigzag i64
```

There are no padding bytes, alignment or per-section offsets, so sections can
only be reached by reading in order. There is no checksum.

## Header

Fixed-width integers, little-endian.

| Offset | Size | Field            | Type  | Value                  |
| -----: | ---: | ---------------- | ----- | ---------------------- |
|      0 |    4 | `magic`          | bytes | `FSID` (`46 53 49 44`) |
|      4 |    2 | `version`        | u16   | `5`                    |
|      6 |    8 | `usn_checkpoint` | u64   | Legacy, see below      |
|     14 |    8 | `raw_len`        | u64   | Body length, decompressed |
|     22 |    8 | `data_len`       | u64   | Body length in the file (compressed) |

`usn_checkpoint` is still read and written, but nothing sets it, so it is `0` in
practice. The real change-log position is the `journal_position` key in `meta`.

## Encoding

`fsd` compresses at zstd level 1: paths repeat their folders' words, so the
fastest level gets most of the gain. Every offset and length below refers to
the decompressed body.

The decompressed body is the [postcard](https://postcard.jamesmunns.com/wire-format) encoding
of the snapshot:

| Type          | Encoding                                                  |
| ------------- | --------------------------------------------------------- |
| `u8`          | One raw byte                                              |
| `u32`, `u64`  | Unsigned LEB128 varint (at most 5 and 10 bytes)           |
| `i64`         | Zigzag `(n << 1) ^ (n >> 63)`, then as an unsigned varint |
| `string`      | varint byte length, then UTF-8 bytes                      |
| `seq<T>`      | varint element count, then the elements                   |
| `Option<T>`   | `00` for none; `01` followed by `T` for some              |
| tuple, struct | Fields in order, with no framing                          |

`arena` has the same bytes as a postcard `string`.

## Arena

All path tails concatenated. `fsd` writes them in entry order with no gaps and
no dead bytes, so each `tail_off` equals the sum of the preceding `tail_len`s.
Readers must use `tail_off` and not assume this.

## Entry

One entry per indexed file, directory, archive member or ghost. An entry's
**number** is its zero-based position in `entries`. Each encoded entry takes at
most 86 bytes (the reader buffers 96).

Fields, in order:

|   # | Field         | Type | Meaning                                                                  |
| --: | ------------- | ---- | ------------------------------------------------------------------------ |
|   1 | `tail_off`    | u64  | Byte offset of the tail in `arena`                                       |
|   2 | `tail_len`    | u32  | Byte length of the tail                                                  |
|   3 | `parent`      | u32  | Entry this path extends; `0xFFFFFFFF` (`NO_PARENT`) if the tail is whole |
|   4 | `name_off`    | u32  | Byte offset of the file name within the tail                             |
|   5 | `archive_len` | u32  | Length of the archive-path prefix of the full path; 0 if none            |
|   6 | `ext`         | u32  | 1-based index into `exts`; 0 = no extension                              |
|   7 | `size`        | u64  | Size in bytes                                                            |
|   8 | `mtime`       | i64  | Modification time, Unix seconds                                          |
|   9 | `ctime`       | i64  | Unix seconds: inode change time on Unix, creation time on Windows        |
|  10 | `indexed_at`  | i64  | When the entry was indexed, Unix seconds                                 |
|  11 | `attrib`      | u32  | `ATTRIB_*` bitmask (`store/mod.rs`)                                      |
|  12 | `flags`       | u8   | See below                                                                |
|  13 | `generation`  | u32  | Crawl that last saw the entry; 0 = never tagged by a crawl               |

### Flags

| Bit | Mask | Name        | Meaning                                                   |
| --: | ---: | ----------- | --------------------------------------------------------- |
|   0 |    1 | `F_DIR`     | Directory                                                 |
|   1 |    2 | `F_HIDDEN`  | Hidden                                                    |
|   2 |    4 | `F_SYMLINK` | Symbolic link                                             |
|   3 |    8 | `F_CONTENT` | Content has been indexed                                  |
|   4 |   16 | `F_EXTRA`   | Has a row in `extra`; readers ignore the row without this |
|   5 |   32 | `F_DEAD`    | Ghost: removed, kept only so descendants' paths resolve   |

Bits 6 and 7 are unused and written as zero.

### Rebuilding a path

Walk `parent` links to the root and concatenate the tails, root first:

```
path(i) = (parent == NO_PARENT ? "" : path(parent)) + arena[tail_off .. tail_off + tail_len]
```

A tail is one of:

- `/name` (or `\name`), for a child of an indexed directory.
- `!/inner`, for an archive's top-level member whose parent is the archive.
- The whole path, when the parent was not indexed yet. `parent` is then
  `NO_PARENT`.

Virtual archive paths use `!/` as the separator, for example
`/data/a.zip!/in/c.txt`.

### Derived fields

- **Name:** `extra.name` if present, otherwise the tail from `name_off` to its
  end. `name_off == tail_len` means an empty name.
- **Extension:** `exts[ext - 1]`, or none when `ext` is 0.
- **Archive path:** the first `archive_len` bytes of the full path, otherwise
  `extra.archive_path`.

### Ghosts

When an entry is removed while others still extend it, it stays as a ghost with
`flags == F_DEAD` and `generation == 0`. Its other fields are stale and it has
no `extra` row. Saving drops every dead entry that no live entry extends and
renumbers the rest, keeping their relative order. Ghosts must not be returned as
results, but their tails still count when rebuilding paths.

## Tables after the entries

### `exts`

The extension strings that `Entry.ext` indexes. Order has no meaning.

### `extra`

`(entry number, Extra)` pairs, sorted by entry number, for fields that are rare
or do not fit the path layout. `Extra` fields in order:

| Field          | Type             | Used for                                           |
| -------------- | ---------------- | -------------------------------------------------- |
| `id`           | `Option<i64>`    | Record id, if one was assigned                     |
| `tantivy_id`   | `Option<string>` | Document id in the full-text index                 |
| `name`         | `Option<string>` | Name that is not a suffix of the tail              |
| `archive_path` | `Option<string>` | Archive path that is not a `!/` prefix of the path |

### `meta`

String key/value pairs, sorted by key. Keys `fsd` uses:

| Key                | Value                                                     |
| ------------------ | --------------------------------------------------------- |
| `last_crawl_time`  | Unix seconds, in decimal. Its absence forces a full crawl |
| `total_files`      | Count from the last crawl, in decimal                     |
| `journal_position` | JSON [`Position`](#journal_position)                      |

Readers must keep keys they do not recognise.

#### `journal_position`

```json
{
  "event_id": 748574341,
  "roots": [["/data/watch", "96633AD9-2030-4FB7-9335-CDC24FE7821E"]],
  "usn": [{"...": "Windows only, omitted when empty"}]
}
```

`event_id` is the FSEvents event id on macOS and wall-clock time on Linux and
Windows. `roots` pairs each watch root with the identity of its volume's change
log; if the list differs from the current one, replay is not trusted.

### `crawl_gen`

Counter of crawls. An entry whose `generation` is behind it was not seen by the
latest crawl.

## Validity

A reader must reject the file if any of these hold. `fsd` then logs a warning,
starts with an empty index and crawls again.

| Check                                                                 |
| --------------------------------------------------------------------- |
| File shorter than the header, or `magic` is wrong                     |
| `version` is not one the reader supports                              |
| `30 + data_len` is past the end of the file                           |
| The body is not a valid zstd frame, or ends before `raw_len` bytes    |
| `arena_len > raw_len`, or `arena` is not valid UTF-8                  |
| `entry_count` exceeds `(raw_len - arena_len) / 12`                    |
| A tail runs past `arena`                                              |
| `tail_off`, its end, or `tail_off + name_off` is not a UTF-8 boundary |
| `name_off > tail_len`                                                 |
| `parent != NO_PARENT` and `parent >= ` the entry's own number         |
| `ext > exts.len`                                                      |
| An `extra` key is not a valid entry number                            |
| Two entries rebuild to the same path, ghosts included                 |

The `parent` rule guarantees a walk terminates and that one pass in entry order
can resolve every parent. Bytes after `30 + data_len` are ignored, and `fsd`
writes none. `archive_len` is not validated on load, so readers must
bounds-check it.

## Writing

1. Write the whole snapshot to `<data_dir>/index.bin.tmp`, streaming the body
   through the zstd encoder, with `raw_len` and `data_len` as 0.
2. Seek back, patch in `raw_len` and `data_len`, and `fsync`.
3. Rename over `index.bin`.

A crash at any point leaves the previous snapshot intact. `fsd` saves on clean
shutdown and, when entries have changed and no crawl is running, every
`store.snapshot_interval_secs` (default 300; 0 disables it).

## Versions

| Version | Header                    | Body                                                                    |
| ------: | ------------------------- | ----------------------------------------------------------------------- |
|       1 | 14 B: no `usn_checkpoint` | postcard `Vec<FileRecord>`                                              |
|       2 | 22 B                      | postcard `Vec<FileRecord>`                                              |
|       3 | 22 B                      | This body, but the arena holds whole paths and entries have no `parent` |
|       4 | 22 B                      | Path tails and `parent`, as described here, uncompressed                |
|       5 | 30 B: adds `raw_len`      | The version 4 body in one zstd frame                                    |

`fsd` reads all five and always writes the latest. Over the 1.43M entries of
C: on a CI runner, version 4 took 78 MB. In version 4, the header is the
first 22 bytes above without `raw_len`, and `data_len` is the plain body's
length. Any change to the bytes needs
a new `version`, a reader for the old one, and an update to this document.

In version 3, entry fields 1-3 are replaced by `path_off` (u64) and `path_len`
(u32); `name_off` is relative to the whole path.

## Example

The first 24 bytes of a real version 4 file with 771 body bytes and a 288-byte
arena. In version 5 the same body bytes are what the zstd frame decompresses
to:

```
46 53 49 44               magic "FSID"
04 00                     version 4
00 00 00 00 00 00 00 00   usn_checkpoint 0
03 03 00 00 00 00 00 00   data_len 771
a0 02                     arena_len 288 (varint)
```

An entry for `a.txt` under directory entry 1, with its tail `/a.txt` at arena
offset 248, starts:

```
f8 01   tail_off 248
06      tail_len 6
01      parent 1
01      name_off 1, so the name is "a.txt"
00      archive_len 0
01      ext 1, so exts[0] = "txt"
```
