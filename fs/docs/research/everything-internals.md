# How voidtools Everything Stays Small and Fast

> **Status:** research note (Nov 2026). Feeds
> [plans/everything-borrowing.md](../plans/everything-borrowing.md). Facts
> marked *documented* come from voidtools' FAQ or the developer's forum posts;
> facts marked *inferred* are reasoned from published numbers and are not
> confirmed by voidtools.

## 1. Published numbers

| Index                              | RAM       | Disk (db) | Initial index |
| ---------------------------------- | --------- | --------- | ------------- |
| Fresh Windows 11 (~250,000 files)  | ~35 MB    | < 14 MB   | ~5 s          |
| 1,000,000 files                    | ~100 MB   | ~45 MB    | ~1 min        |

*Documented* ([FAQ](https://www.voidtools.com/faq/)). That is roughly
**100-140 bytes of RAM per file** and **~45 bytes of disk per file**,
including process overhead, with the default "fast sort" extras enabled.

For comparison, our daemon on 2.37M files / 191k dirs (macOS, Nov 2026):

| Store  | Idle RAM | Bytes/entry | Disk    | Initial crawl |
| ------ | -------- | ----------- | ------- | ------------- |
| SQLite | 205 MB   | ~80         | 1.8 GB  | 136 s         |
| Memory | 2.1 GB   | ~850        | 487 MB  | 20 s          |

## 2. The six techniques

### 2.1 Read the volume's own table instead of walking directories

*Documented:* "Everything reads the NTFS Master File Table (MFT) to quickly
index your files." The MFT is one file per volume listing every file record
(name, parent record number, size, timestamps, attributes). Reading it is a
handful of large sequential reads (`FSCTL_ENUM_USN_DATA` / raw MFT reads),
instead of one `FindFirstFile`/`stat` system call per directory and per file.

Consequences:

- No per-file system calls, no directory recursion, no path building during the
  scan; each record already carries its parent's record number.
- Requires administrator rights. Everything runs a small elevated **service**
  that does the privileged reads; the UI and database run as the normal user
  (*documented*).
- NTFS-only. FAT/exFAT/network shares use slower "folder indexing" (normal
  directory walks plus periodic rescans) (*documented*).

### 2.2 Follow the filesystem's change journal

*Documented:* "Everything uses the USN Journal to keep your indexes up to
date", and "changes will not be missed when Everything is not running as the
system maintains the NTFS USN Journal."

The USN (update sequence number) journal is an append-only log maintained by
the NTFS driver itself. Every create/delete/rename/attribute change gets a
record with a monotonically increasing USN. Everything stores the last USN it
processed together with the journal ID; on restart it reads only records after
that USN. If the journal ID changed or the USN fell off the end of the journal
(it is a bounded ring), it rebuilds that volume.

Consequences:

- **Restart cost is proportional to what changed while it was off, not to the
  size of the volume.** There is no "rescan on startup".
- No per-directory watch handles; one journal per volume covers everything.

### 2.3 Keep a minimal copy of the table in memory

*Documented:* "Everything makes a very light copy of this MFT and keeps it in
memory." Content is not indexed; by default only name, path, size, and dates
(*documented*).

*Inferred* from ~100 B/file: each entry cannot hold its full path. The natural
layout, matching the MFT itself, is:

```
entry { parent: u32, name: offset into a shared name buffer,
        size: u64, mtime: u64, attributes: u32 }
```

The full path is reconstructed by walking `parent` links only for rows that
are displayed. A file at depth 10 costs one name, not ten.

### 2.4 Sort indexes are optional, precomputed arrays

*Documented:* version 1.4 "indexes file sizes and dates and also stores extra
information for faster sorting by default", and voidtools publishes a guide to
turning these off for the smallest footprint. A sort index is an array of
entry numbers in sorted order (4 bytes/file per sort key), maintained
incrementally. Sorting a result set is then a filter over a presorted array,
not a sort.

### 2.5 Searches are compiled, then scan the in-memory table

*Documented:* "Searches are compiled into byte code and executed." There is no
B-tree or n-gram index for substring search. Each query is compiled once into
a small program, then run over every entry's name, which is contiguous memory
and cache-friendly, and spread across cores. With ~100 B/entry, 1M entries is
~100 MB, which a modern CPU scans in tens of milliseconds.

### 2.6 Results are references, rendered lazily

*Inferred* from the UI behaviour (millions of results scroll instantly): a
result set is an array of entry numbers. Only the visible rows are turned into
strings (full path, formatted size/date). Nothing is copied per match.

### 2.7 Persistence is a snapshot, not a transactional database

*Documented:* Everything "updates the database when it is started" and keeps
it current from the journal. The on-disk database is a compact dump of the
in-memory table, loaded with sequential reads, and updated at the journal
position it was saved at. There is no write-ahead log, no per-row B-tree
maintenance, and no secondary index churn during indexing.

## 3. What does not transfer directly

| Technique            | Windows (NTFS)         | macOS (APFS)                         | Linux (ext4/xfs/btrfs)                  |
| -------------------- | ---------------------- | ------------------------------------ | --------------------------------------- |
| Bulk enumeration     | MFT / `FSCTL_ENUM_USN_DATA` | `getattrlistbulk` per directory (1 call per dir, not per file) | `getdents64` + `statx`; no volume table |
| Persistent change log | USN journal           | FSEvents event IDs (`sinceWhen`), per volume, persisted by the OS | none; `fanotify` is live-only           |
| Privilege needed     | admin (service)        | none for FSEvents; Full Disk Access for some paths | root for `fanotify` marks              |

So "catch up instead of rescan" works on Windows and macOS. Linux has no
persistent journal; the best available is a rescan that skips re-reading
directories whose `mtime` is unchanged (still one `stat` per directory).

## 4. Mapping to our implementation

| Everything                          | filesearch today                                   | Gap |
| ----------------------------------- | -------------------------------------------------- | --- |
| Path stored once (parent + name)    | Memory store keeps the full path 3x (`records`, `by_path` key, `crawl_info` key) plus a watch-root copy per entry, and separate `name`/`extension` strings | ~850 B/entry |
| Results are entry numbers           | `fuzzy:` copies up to 50,000 full `FileRecord`s per query | +600 MB RSS under 8 concurrent fuzzy queries |
| Snapshot + journal position on restart | Memory-store snapshot drops `meta`, so `last_crawl_time` is lost and every restart recrawls | 52 CPU-s and 2 GB churn per restart |
| No transactional DB during indexing | SQLite with 7 secondary indexes, per-connection 2 MB cache on writer connections | 136 s crawl, kernel-time bound |
| USN / FSEvents catch-up             | `notify` live events only; `usn_checkpoint` field exists but nothing reads a journal | changes while stopped are missed (SQLite) or cost a full recrawl (memory) |
| MFT enumeration                     | `ignore` parallel walk, one `stat` per entry       | platform work |

## Sources

- [voidtools FAQ](https://www.voidtools.com/faq/)
- [Technical questions about Everything (developer post)](https://www.voidtools.com/forum/viewtopic.php?p=6515)
- [why everything so quick?](https://voidtools.com/forum/viewtopic.php?p=50915)
- [How "Everything" doesn't miss changes when not running](https://voidtools.com/forum/viewtopic.php?p=486)
- [USN Journal (Wikipedia)](https://en.wikipedia.org/wiki/USN_Journal)
