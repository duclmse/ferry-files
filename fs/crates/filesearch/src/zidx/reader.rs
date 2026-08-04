use std::collections::HashMap;
use std::path::Path;

use memmap2::Mmap;

use crate::zidx::ZidxError;

// ---------------------------------------------------------------------------
// On-disk constants (format version 2)
// ---------------------------------------------------------------------------

const HEADER_SIZE: usize = 20;
const NODE_SIZE:   usize = 48;
const FORMAT_VERSION: u32 = 2;

// ---------------------------------------------------------------------------
// Decoded node (in-memory representation after from_le_bytes decoding)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct NodeData {
    pub parent:              u32,
    pub first_child:         u32,
    pub next_sibling:        u32,
    pub name_off:            u32,
    pub name_len:            u16,
    /// 0 = file, 1 = dir
    pub kind:                u8,
    pub compression:         u16,
    pub compressed_size:     u64,
    pub uncompressed_size:   u64,
    pub local_header_offset: u64,
}

// ---------------------------------------------------------------------------
// ChildIter - lazy linked-list walk over direct children
// ---------------------------------------------------------------------------

pub struct ChildIter<'a> {
    index:   &'a ZidxIndex,
    current: u32,
}

impl<'a> Iterator for ChildIter<'a> {
    type Item = Result<u32, ZidxError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current == u32::MAX {
            return None;
        }
        let id = self.current;
        match self.index.node(id) {
            Ok(n) => {
                self.current = n.next_sibling;
                Some(Ok(id))
            }
            Err(e) => {
                // Stop iteration on corrupt data.
                self.current = u32::MAX;
                Some(Err(e))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ZidxIndex
// ---------------------------------------------------------------------------

pub struct ZidxIndex {
    mmap:           Mmap,
    pub node_count: u32,
    nodes_offset:   usize, // = HEADER_SIZE (20)
    strings_offset: usize, // = HEADER_SIZE + node_count * NODE_SIZE (48)
    #[allow(dead_code)]
    string_bytes:   u32,
    /// O(1) lookup accelerator: (parent_id, name) → child_id.
    /// Built once in `open`; avoids linear child scans in `lookup`.
    lookup_index:   HashMap<(u32, Box<str>), u32>,
}

impl std::fmt::Debug for ZidxIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZidxIndex")
            .field("node_count", &self.node_count)
            .finish_non_exhaustive()
    }
}

impl ZidxIndex {
    /// Open and validate a `.zidx` file. Returns `Err(Corrupt)` if magic,
    /// version, or size checks fail.
    pub fn open(path: &Path) -> Result<Self, ZidxError> {
        let file = std::fs::File::open(path)?;
        // SAFETY: the file is read-only and we do not mutate the mapping.
        let mmap = unsafe { Mmap::map(&file)? };

        if mmap.len() < HEADER_SIZE {
            return Err(ZidxError::Corrupt("file too small for header".into()));
        }
        if &mmap[0..4] != b"ZIDX" {
            return Err(ZidxError::Corrupt("invalid magic bytes".into()));
        }
        let version = u32::from_le_bytes(mmap[4..8].try_into().unwrap());
        if version != FORMAT_VERSION {
            return Err(ZidxError::Corrupt(format!(
                "unsupported .zidx version {version} (expected {FORMAT_VERSION}); rebuild the index"
            )));
        }
        let node_count   = u32::from_le_bytes(mmap[8..12].try_into().unwrap());
        let string_bytes = u32::from_le_bytes(mmap[12..16].try_into().unwrap());

        let expected_len = HEADER_SIZE
            .saturating_add((node_count as usize).saturating_mul(NODE_SIZE))
            .saturating_add(string_bytes as usize);
        if mmap.len() < expected_len {
            return Err(ZidxError::Corrupt(format!(
                "file truncated: expected {expected_len} bytes, got {}",
                mmap.len()
            )));
        }

        let nodes_offset   = HEADER_SIZE;
        let strings_offset = HEADER_SIZE + node_count as usize * NODE_SIZE;

        let mut this = Self {
            mmap,
            node_count,
            nodes_offset,
            strings_offset,
            string_bytes,
            lookup_index: HashMap::with_capacity(node_count as usize),
        };

        // Build the (parent_id, name) → child_id map so lookup() is O(depth)
        // rather than O(depth × max_children_per_dir).
        let mut entries = Vec::with_capacity(node_count as usize);
        for id in 0..node_count {
            let node = this.node(id)?;
            let name: Box<str> = this.name(&node)?.into();
            entries.push((node.parent, name, id));
        }
        this.lookup_index = entries
            .into_iter()
            .map(|(parent, name, id)| ((parent, name), id))
            .collect();

        Ok(this)
    }

    /// Decode node `id` from the mmap. Returns `Err(Corrupt)` for out-of-range IDs.
    pub fn node(&self, id: u32) -> Result<NodeData, ZidxError> {
        let base = self.nodes_offset + id as usize * NODE_SIZE;
        if base + NODE_SIZE > self.mmap.len() {
            return Err(ZidxError::Corrupt(format!("node {id} out of range")));
        }
        let b = &self.mmap[base..base + NODE_SIZE];
        // Layout v2 (48 bytes):
        //  0.. 4  parent         u32
        //  4.. 8  first_child    u32
        //  8..12  next_sibling   u32
        // 12..16  name_off       u32
        // 16..18  name_len       u16
        // 18      kind           u8
        // 19      _pad
        // 20..22  compression    u16
        // 22..24  _pad2
        // 24..32  compressed_size   u64  ← was u32 in v1
        // 32..40  uncompressed_size u64  ← was u32 in v1
        // 40..48  local_header_offset u64
        Ok(NodeData {
            parent:              u32::from_le_bytes(b[0..4].try_into().unwrap()),
            first_child:         u32::from_le_bytes(b[4..8].try_into().unwrap()),
            next_sibling:        u32::from_le_bytes(b[8..12].try_into().unwrap()),
            name_off:            u32::from_le_bytes(b[12..16].try_into().unwrap()),
            name_len:            u16::from_le_bytes(b[16..18].try_into().unwrap()),
            kind:                b[18],
            // b[19] = _pad
            compression:         u16::from_le_bytes(b[20..22].try_into().unwrap()),
            // b[22..24] = _pad2
            compressed_size:     u64::from_le_bytes(b[24..32].try_into().unwrap()),
            uncompressed_size:   u64::from_le_bytes(b[32..40].try_into().unwrap()),
            local_header_offset: u64::from_le_bytes(b[40..48].try_into().unwrap()),
        })
    }

    /// Borrow the name of a node zero-copy from the string pool.
    pub fn name<'a>(&'a self, node: &NodeData) -> Result<&'a str, ZidxError> {
        let start = self.strings_offset + node.name_off as usize;
        let end   = start + node.name_len as usize;
        if end > self.mmap.len() {
            return Err(ZidxError::Corrupt(format!(
                "name offset {} out of range", node.name_off
            )));
        }
        std::str::from_utf8(&self.mmap[start..end])
            .map_err(|e| ZidxError::Corrupt(format!("invalid UTF-8 in name: {e}")))
    }

    /// Iterate the direct children of `parent_id` via first_child / next_sibling.
    pub fn children(&self, parent_id: u32) -> ChildIter<'_> {
        let first = self.node(parent_id)
            .map(|n| n.first_child)
            .unwrap_or(u32::MAX);
        ChildIter { index: self, current: first }
    }

    /// Walk from root to find the node for `path`. Returns `Ok(None)` when the
    /// path is not found; `Err` only for corrupt index data.
    ///
    /// Uses the pre-built `lookup_index` for O(depth) resolution rather than
    /// scanning all children at each level.
    pub fn lookup(&self, path: &str) -> Result<Option<u32>, ZidxError> {
        let mut current_id = 0u32; // start at root
        for part in path.split('/').filter(|s| !s.is_empty()) {
            match self.lookup_index.get(&(current_id, part.into())) {
                Some(&child_id) => current_id = child_id,
                None            => return Ok(None),
            }
        }
        Ok(Some(current_id))
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use tempfile::TempDir;

    use crate::zidx::ZidxBuilder;

    fn make_zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        let mut zw  = zip::ZipWriter::new(&mut buf);
        let opts    = zip::write::FileOptions::<()>::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, data) in entries {
            zw.start_file(*name, opts).unwrap();
            zw.write_all(data).unwrap();
        }
        zw.finish().unwrap();
        buf.into_inner()
    }

    fn fixture() -> (TempDir, std::path::PathBuf, ZidxIndex) {
        let dir      = TempDir::new().unwrap();
        let zip_path = dir.path().join("fix.zip");
        let idx_path = dir.path().join("fix.zidx");
        std::fs::write(&zip_path, make_zip_bytes(&[
            ("src/main.rs", b"fn main() {}"),
            ("src/lib.rs",  b"pub fn foo() {}"),
            ("README.md",   b"# readme"),
        ])).unwrap();
        ZidxBuilder::build(&zip_path, &idx_path).unwrap();
        let idx = ZidxIndex::open(&idx_path).unwrap();
        (dir, idx_path, idx)
    }

    #[test]
    fn open_valid_file_succeeds() {
        let (_dir, _path, _idx) = fixture();
    }

    #[test]
    fn open_wrong_magic_returns_corrupt() {
        let dir      = TempDir::new().unwrap();
        let bad_path = dir.path().join("bad.zidx");
        std::fs::write(&bad_path, b"XXXX\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00").unwrap();
        let err = ZidxIndex::open(&bad_path).unwrap_err();
        assert!(matches!(err, ZidxError::Corrupt(_)), "expected Corrupt, got {err}");
    }

    #[test]
    fn open_truncated_file_returns_corrupt() {
        let dir      = TempDir::new().unwrap();
        let bad_path = dir.path().join("trunc.zidx");
        std::fs::write(&bad_path, b"ZIDX").unwrap(); // only 4 bytes
        let err = ZidxIndex::open(&bad_path).unwrap_err();
        assert!(matches!(err, ZidxError::Corrupt(_)), "expected Corrupt, got {err}");
    }

    #[test]
    fn node_zero_is_root() {
        let (_dir, _, idx) = fixture();
        let root = idx.node(0).unwrap();
        assert_eq!(root.kind, 1, "root must be a directory");
        assert_eq!(root.parent, 0, "root parent = self");
    }

    #[test]
    fn out_of_range_node_returns_corrupt() {
        let (_dir, _, idx) = fixture();
        let err = idx.node(idx.node_count + 1).unwrap_err();
        assert!(matches!(err, ZidxError::Corrupt(_)));
    }

    #[test]
    fn name_returns_correct_utf8() {
        let (_dir, _, idx) = fixture();
        // Collect all names from the tree.
        let mut names = Vec::new();
        for id in 0..idx.node_count {
            let n    = idx.node(id).unwrap();
            let name = idx.name(&n).unwrap();
            names.push(name.to_owned());
        }
        assert!(names.contains(&"/".to_owned()),       "root name '/'");
        assert!(names.contains(&"src".to_owned()),     "dir name 'src'");
        assert!(names.contains(&"main.rs".to_owned()), "file name 'main.rs'");
    }

    #[test]
    fn children_iteration_matches_file_list() {
        let (_dir, _, idx) = fixture();
        // Root children: README.md and src/
        let root_children: Vec<String> = idx.children(0)
            .map(|r| {
                let id   = r.unwrap();
                let n    = idx.node(id).unwrap();
                idx.name(&n).unwrap().to_owned()
            })
            .collect();
        assert!(root_children.contains(&"README.md".to_owned()));
        assert!(root_children.contains(&"src".to_owned()));
    }

    #[test]
    fn lookup_finds_existing_path() {
        let (_dir, _, idx) = fixture();
        let id = idx.lookup("src/main.rs").unwrap();
        assert!(id.is_some(), "src/main.rs must be found");
        let node = idx.node(id.unwrap()).unwrap();
        assert_eq!(idx.name(&node).unwrap(), "main.rs");
        assert_eq!(node.kind, 0, "must be a file");
    }

    #[test]
    fn lookup_missing_path_returns_none() {
        let (_dir, _, idx) = fixture();
        let result = idx.lookup("nonexistent/path.rs").unwrap();
        assert!(result.is_none());
    }
}
