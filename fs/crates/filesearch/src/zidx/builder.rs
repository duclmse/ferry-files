use std::collections::{BTreeMap, HashMap};
use std::io::{BufWriter, Write};
use std::path::Path;

use crate::zidx::ZidxError;

// ---------------------------------------------------------------------------
// Internal build types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum NodeKind { File, Dir }

struct BuildNode {
    name:                String,
    kind:                NodeKind,
    children:            BTreeMap<String, BuildNode>,
    compression:         u16,
    compressed_size:     u64,
    uncompressed_size:   u64,
    local_header_offset: u64,
}

struct FlatNode {
    parent:              u32,
    name:                String,
    kind:                NodeKind,
    children:            Vec<u32>,
    compression:         u16,
    compressed_size:     u64,
    uncompressed_size:   u64,
    local_header_offset: u64,
    // filled in the linking phase
    first_child:         u32,
    next_sibling:        u32,
    // filled in the string-pool phase
    name_off:            u32,
    name_len:            u16,
}

struct StringPool {
    map: HashMap<String, u32>,
    buf: Vec<u8>,
}

impl StringPool {
    fn new() -> Self {
        Self { map: HashMap::new(), buf: Vec::new() }
    }

    fn intern(&mut self, s: &str) -> (u32, u16) {
        if let Some(&off) = self.map.get(s) {
            return (off, s.len() as u16);
        }
        let off = self.buf.len() as u32;
        self.buf.extend_from_slice(s.as_bytes());
        self.map.insert(s.to_string(), off);
        (off, s.len() as u16)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn compression_method_to_u16(m: zip::CompressionMethod) -> u16 {
    // Map known ZIP spec compression codes. zip doesn't expose a public
    // From/Into<u16> impl, so we match on the enum variants available.
    match m {
        zip::CompressionMethod::Stored   => 0,
        zip::CompressionMethod::Deflated => 8,
        _                                => 0,
    }
}

// ---------------------------------------------------------------------------
// Path handling
// ---------------------------------------------------------------------------

fn split_path(path: &str) -> Vec<&str> {
    path.split('/').filter(|s| !s.is_empty()).collect()
}

// ---------------------------------------------------------------------------
// Tree building
// ---------------------------------------------------------------------------

fn make_dir_node(name: &str) -> BuildNode {
    BuildNode {
        name:                name.to_owned(),
        kind:                NodeKind::Dir,
        children:            BTreeMap::new(),
        compression:         0,
        compressed_size:     0,
        uncompressed_size:   0,
        local_header_offset: 0,
    }
}

fn insert(
    root:                &mut BuildNode,
    parts:               &[&str],
    is_dir:              bool,
    compression:         u16,
    compressed_size:     u64,
    uncompressed_size:   u64,
    local_header_offset: u64,
) {
    if parts.is_empty() {
        return;
    }
    let mut cur = root;
    for (i, &part) in parts.iter().enumerate() {
        let is_last = i == parts.len() - 1;
        cur = cur.children.entry(part.to_owned()).or_insert_with(|| BuildNode {
            name:                part.to_owned(),
            kind:                if is_last && !is_dir { NodeKind::File } else { NodeKind::Dir },
            children:            BTreeMap::new(),
            compression:         0,
            compressed_size:     0,
            uncompressed_size:   0,
            local_header_offset: 0,
        });
        // Update metadata when we reach the leaf file node.
        if is_last && cur.kind == NodeKind::File {
            cur.compression         = compression;
            cur.compressed_size     = compressed_size;
            cur.uncompressed_size   = uncompressed_size;
            cur.local_header_offset = local_header_offset;
        }
    }
}

// ---------------------------------------------------------------------------
// DFS flatten — preorder so parent ID is always lower than children's IDs
// ---------------------------------------------------------------------------

fn flatten(node: &BuildNode, parent_id: u32, out: &mut Vec<FlatNode>) -> u32 {
    let id = out.len() as u32;
    out.push(FlatNode {
        parent:              parent_id,
        name:                node.name.clone(),
        kind:                node.kind,
        children:            Vec::new(),
        compression:         node.compression,
        compressed_size:     node.compressed_size,
        uncompressed_size:   node.uncompressed_size,
        local_header_offset: node.local_header_offset,
        first_child:         u32::MAX,
        next_sibling:        u32::MAX,
        name_off:            0,
        name_len:            0,
    });
    let mut child_ids = Vec::new();
    for child in node.children.values() {
        let cid = flatten(child, id, out);
        child_ids.push(cid);
    }
    out[id as usize].children = child_ids;
    id
}

// ---------------------------------------------------------------------------
// Write helpers — all fields written individually with to_le_bytes()
// ---------------------------------------------------------------------------

fn write_header(w: &mut impl Write, node_count: u32, string_bytes: u32) -> std::io::Result<()> {
    w.write_all(b"ZIDX")?;
    w.write_all(&2u32.to_le_bytes())?;         // version 2: u64 sizes
    w.write_all(&node_count.to_le_bytes())?;
    w.write_all(&string_bytes.to_le_bytes())?;
    w.write_all(&0u32.to_le_bytes())?;          // root_node = 0
    Ok(())
}

fn write_node(w: &mut impl Write, n: &FlatNode) -> std::io::Result<()> {
    w.write_all(&n.parent.to_le_bytes())?;           //  0.. 4  u32
    w.write_all(&n.first_child.to_le_bytes())?;      //  4.. 8  u32
    w.write_all(&n.next_sibling.to_le_bytes())?;     //  8..12  u32
    w.write_all(&n.name_off.to_le_bytes())?;         // 12..16  u32
    w.write_all(&n.name_len.to_le_bytes())?;         // 16..18  u16
    let kind_byte: u8 = if n.kind == NodeKind::Dir { 1 } else { 0 };
    w.write_all(&[kind_byte, 0])?;                   // 18 kind + 19 _pad
    w.write_all(&n.compression.to_le_bytes())?;      // 20..22  u16
    w.write_all(&[0u8; 2])?;                         // 22..24  _pad2
    w.write_all(&n.compressed_size.to_le_bytes())?;  // 24..32  u64
    w.write_all(&n.uncompressed_size.to_le_bytes())?;// 32..40  u64
    w.write_all(&n.local_header_offset.to_le_bytes())?; // 40..48 u64
    Ok(())
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub struct ZidxBuilder;

impl ZidxBuilder {
    /// Build a `.zidx` sidecar for the ZIP at `zip_path`.
    ///
    /// Writes to `out_path` (overwriting if it exists). Creates parent
    /// directories as needed. Returns the total number of nodes written
    /// (file nodes + implicit directory nodes + 1 for root).
    pub fn build(zip_path: &Path, out_path: &Path) -> Result<u32, ZidxError> {
        // ── Step 1: enumerate ZIP entries ──────────────────────────────────
        let file   = std::fs::File::open(zip_path)?;
        let reader = std::io::BufReader::with_capacity(65536, file);
        let mut archive = zip::ZipArchive::new(reader)?;

        let total = archive.len();
        if total > u32::MAX as usize {
            return Err(ZidxError::TooLarge(total));
        }

        // ── Step 2: build tree ─────────────────────────────────────────────
        let mut root = make_dir_node("/");

        for i in 0..total {
            let entry = archive.by_index(i)?;
            // Normalise path: replace backslashes used by some ZIP writers.
            let raw_name = entry.name().replace('\\', "/");
            let is_dir   = entry.is_dir();
            // zip 2.x exposes CompressionMethod as an opaque struct with no
            // public From<..> impl; map known constants to their ZIP spec values.
            let compression         = compression_method_to_u16(entry.compression());
            let compressed_size     = entry.compressed_size();
            let uncompressed_size   = entry.size();
            let local_header_offset = entry.header_start();
            drop(entry);

            let parts = split_path(&raw_name);
            insert(&mut root, &parts, is_dir,
                   compression, compressed_size, uncompressed_size, local_header_offset);
        }

        // ── Step 3: DFS flatten (preorder → parent ID < children IDs) ─────
        let mut flat: Vec<FlatNode> = Vec::new();
        flatten(&root, 0, &mut flat);

        let node_count = flat.len();
        if node_count > u32::MAX as usize {
            return Err(ZidxError::TooLarge(node_count));
        }

        // ── Step 4: convert children vec → first_child / next_sibling ─────
        // Collect all child ID lists first to avoid simultaneous mutable borrows.
        let child_lists: Vec<Vec<u32>> =
            flat.iter().map(|n| n.children.clone()).collect();

        for (i, children) in child_lists.iter().enumerate() {
            flat[i].first_child = children.first().copied().unwrap_or(u32::MAX);
            for w in children.windows(2) {
                flat[w[0] as usize].next_sibling = w[1];
            }
            if let Some(&last) = children.last() {
                flat[last as usize].next_sibling = u32::MAX;
            }
        }

        // ── Step 5: intern names into string pool ─────────────────────────
        let mut pool = StringPool::new();
        for node in &mut flat {
            let (off, len) = pool.intern(&node.name);
            node.name_off = off;
            node.name_len = len;
        }
        let string_bytes = pool.buf.len() as u32;

        // ── Step 6: write output ───────────────────────────────────────────
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let out_file = std::fs::File::create(out_path)?;
        let mut w    = BufWriter::with_capacity(256 * 1024, out_file);

        write_header(&mut w, node_count as u32, string_bytes)?;
        for node in &flat {
            write_node(&mut w, node)?;
        }
        w.write_all(&pool.buf)?;
        w.flush()?;

        Ok(node_count as u32)
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    // use std::io::Write as _;
    use tempfile::TempDir;

    fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        let mut zw = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::FileOptions::<()>::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, data) in entries {
            zw.start_file(*name, opts).unwrap();
            zw.write_all(data).unwrap();
        }
        zw.finish().unwrap();
        buf.into_inner()
    }

    fn build_and_open(entries: &[(&str, &[u8])]) -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir      = TempDir::new().unwrap();
        let zip_path = dir.path().join("test.zip");
        let idx_path = dir.path().join("test.zidx");
        std::fs::write(&zip_path, make_zip(entries)).unwrap();
        (dir, zip_path, idx_path)
    }

    #[test]
    fn node_count_includes_root_and_implicit_dirs() {
        // 5 file entries under src/ → 1 root + 1 src dir + 5 files = 7
        let (_dir, zip_path, idx_path) = build_and_open(&[
            ("src/a.rs",  b""),
            ("src/b.rs",  b""),
            ("src/c.rs",  b""),
            ("src/d.rs",  b""),
            ("src/e.rs",  b""),
        ]);
        let count = ZidxBuilder::build(&zip_path, &idx_path).unwrap();
        assert_eq!(count, 7, "root + src dir + 5 files");
    }

    #[test]
    fn magic_bytes_at_offset_zero() {
        let (_dir, zip_path, idx_path) = build_and_open(&[("a.txt", b"hello")]);
        ZidxBuilder::build(&zip_path, &idx_path).unwrap();
        let bytes = std::fs::read(&idx_path).unwrap();
        assert_eq!(&bytes[0..4], b"ZIDX");
    }

    #[test]
    fn root_node_is_dir_at_id_zero() {
        let (_dir, zip_path, idx_path) = build_and_open(&[("a.txt", b"")]);
        ZidxBuilder::build(&zip_path, &idx_path).unwrap();
        let idx = crate::zidx::ZidxIndex::open(&idx_path).unwrap();
        let root = idx.node(0).unwrap();
        assert_eq!(root.kind, 1, "root must be a directory (kind=1)");
        assert_eq!(root.parent, 0, "root parent points to itself");
    }

    #[test]
    fn file_node_metadata_correct() {
        let (_dir, zip_path, idx_path) = build_and_open(&[("data.txt", b"hello world")]);
        ZidxBuilder::build(&zip_path, &idx_path).unwrap();
        let idx = crate::zidx::ZidxIndex::open(&idx_path).unwrap();
        // Node 1 is the first (and only) file child of root.
        let file_node = idx.node(1).unwrap();
        assert_eq!(file_node.kind, 0, "file node kind=0");
        assert_eq!(file_node.uncompressed_size, 11, "uncompressed_size must match data length");
    }

    #[test]
    fn directory_node_sizes_are_zero() {
        let (_dir, zip_path, idx_path) = build_and_open(&[("subdir/file.txt", b"x")]);
        ZidxBuilder::build(&zip_path, &idx_path).unwrap();
        let idx = crate::zidx::ZidxIndex::open(&idx_path).unwrap();
        // Node 0=root, 1=subdir, 2=file.txt  (preorder DFS)
        let dir_node = idx.node(1).unwrap();
        assert_eq!(dir_node.kind, 1, "subdir must be dir (kind=1)");
        assert_eq!(dir_node.compressed_size, 0);
        assert_eq!(dir_node.uncompressed_size, 0);
    }

    #[test]
    fn empty_zip_produces_root_only() {
        let (_dir, zip_path, idx_path) = build_and_open(&[]);
        let count = ZidxBuilder::build(&zip_path, &idx_path).unwrap();
        assert_eq!(count, 1, "empty ZIP → root only");
    }

    #[test]
    fn nonexistent_zip_returns_io_error() {
        let dir      = TempDir::new().unwrap();
        let zip_path = dir.path().join("nonexistent.zip");
        let idx_path = dir.path().join("out.zidx");
        let err = ZidxBuilder::build(&zip_path, &idx_path).unwrap_err();
        assert!(matches!(err, ZidxError::Io(_)), "expected Io error, got {err}");
    }

    #[test]
    fn build_is_deterministic() {
        let (_dir, zip_path, idx_path) = build_and_open(&[
            ("b.txt", b"b"),
            ("a.txt", b"a"),
            ("c.txt", b"c"),
        ]);
        ZidxBuilder::build(&zip_path, &idx_path).unwrap();
        let first = std::fs::read(&idx_path).unwrap();

        ZidxBuilder::build(&zip_path, &idx_path).unwrap();
        let second = std::fs::read(&idx_path).unwrap();

        assert_eq!(first, second, "identical input must produce byte-identical output");
    }
}
