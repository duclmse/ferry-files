//! First-index crawl from the NTFS master file table, for Windows.
//!
//! Walking directories costs at least one system call per directory and
//! visits the disk in directory order. The MFT holds one record per file with
//! its name, parent, timestamps and size, so one sequential read of it can
//! replace the walk. The USN journal's enumeration call was not enough: it
//! gives names and parents, but no sizes or dates.
//!
//! Everything here reads through [`Volume`], so the parser runs against
//! synthetic images in tests on any platform. Only [`open_volume`] is
//! Windows-specific; it needs administrator rights. Every failure is reported
//! so the crawl can fall back to walking directories.

use std::collections::HashMap;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

const ROOT_RECORD: u32 = 5;
/// Records below this are NTFS's own metafiles (`$MFT`, `$Bitmap`, ...),
/// which directory listings never show.
const FIRST_USER_RECORD: u32 = 24;

const ATTR_STANDARD_INFORMATION: u32 = 0x10;
const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;
const ATTR_REPARSE_POINT: u32 = 0xC0;
const ATTR_END: u32 = 0xFFFF_FFFF;

const RECORD_IN_USE: u16 = 1;
const RECORD_IS_DIRECTORY: u16 = 2;
const NAMESPACE_DOS: u8 = 2;
const REFERENCE_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
const REPARSE_SYMLINK: u32 = 0xA000_000C;
const REPARSE_MOUNT_POINT: u32 = 0xA000_0003;
/// Update sequence protection covers units of 512 bytes whatever the
/// volume's sector size is.
const FIXUP_STRIDE: usize = 512;
const FILETIME_TICKS_PER_SEC: u64 = 10_000_000;
const FILETIME_TO_UNIX_SECS: i64 = 11_644_473_600;
/// Bytes per read; aligned to whole clusters when used.
const READ_CHUNK: u64 = 1 << 24;
/// Raw volume reads must be sector aligned; this suits 512 and 4096 sectors.
const BOOT_READ: usize = 4096;

pub trait Volume {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

fn filetime_to_unix(ticks: u64) -> i64 {
    ((ticks / FILETIME_TICKS_PER_SEC) as i64 - FILETIME_TO_UNIX_SECS).max(0)
}

struct Geometry {
    cluster: u64,
    mft_offset: u64,
    record: usize,
}

fn parse_boot(b: &[u8]) -> Option<Geometry> {
    if b.get(3..11)? != b"NTFS    " {
        return None;
    }
    let sector = u64::from(u16_at(b, 11)?);
    let cluster = sector * u64::from(*b.get(13)?);
    let per_record = *b.get(64)? as i8;
    let record = if per_record < 0 {
        1usize.checked_shl(u32::from(per_record.unsigned_abs()))?
    } else {
        usize::try_from(i64::from(per_record) * cluster as i64).ok()?
    };
    let valid = cluster > 0 && (FIXUP_STRIDE..=1 << 16).contains(&record);
    valid.then_some(Geometry {
        cluster,
        mft_offset: u64_at(b, 48)?.checked_mul(cluster)?,
        record,
    })
}

/// Restores the two bytes NTFS swaps out at the end of each 512-byte unit. A
/// mismatch is a torn record, which a live volume can legitimately have.
fn apply_fixups(record: &mut [u8]) -> Option<()> {
    let offset = usize::from(u16_at(record, 4)?);
    let count = usize::from(u16_at(record, 6)?);
    if count == 0 || (count - 1) * FIXUP_STRIDE > record.len() {
        return None;
    }
    let check = *record.get(offset..offset + 2)?.first_chunk::<2>()?;
    for unit in 1..count {
        let end = unit * FIXUP_STRIDE;
        let fix = *record
            .get(offset + 2 * unit..offset + 2 * unit + 2)?
            .first_chunk::<2>()?;
        let tail = record.get_mut(end - 2..end)?;
        if tail != check {
            return None;
        }
        tail.copy_from_slice(&fix);
    }
    Some(())
}

struct Attribute<'a> {
    kind: u32,
    non_resident: bool,
    named: bool,
    raw: &'a [u8],
}

impl<'a> Attribute<'a> {
    fn content(&self) -> Option<&'a [u8]> {
        if self.non_resident {
            return None;
        }
        let len = u32_at(self.raw, 16)? as usize;
        let off = usize::from(u16_at(self.raw, 20)?);
        self.raw.get(off..off + len)
    }

    /// The real size, which only the extent starting at VCN 0 carries.
    fn data_size(&self) -> Option<u64> {
        if !self.non_resident {
            return Some(u64::from(u32_at(self.raw, 16)?));
        }
        (u64_at(self.raw, 16)? == 0)
            .then(|| u64_at(self.raw, 48))
            .flatten()
    }
}

fn attributes(record: &[u8]) -> impl Iterator<Item = Attribute<'_>> {
    let mut at = u16_at(record, 20).map_or(usize::MAX, usize::from);
    std::iter::from_fn(move || {
        let kind = u32_at(record, at)?;
        if kind == ATTR_END {
            return None;
        }
        let len = u32_at(record, at + 4)? as usize;
        if len < 16 {
            return None;
        }
        let raw = record.get(at..at + len)?;
        at += len;
        Some(Attribute {
            kind,
            non_resident: *raw.get(8)? != 0,
            named: *raw.get(9)? != 0,
            raw,
        })
    })
}

/// Byte ranges of a non-resident attribute's data, in order.
fn data_runs(attr: &Attribute<'_>, cluster: u64) -> Option<Vec<(u64, u64)>> {
    let list = attr.raw.get(usize::from(u16_at(attr.raw, 32)?)..)?;
    let mut runs = Vec::new();
    let mut lcn = 0i64;
    let mut at = 0;
    loop {
        let header = *list.get(at)?;
        if header == 0 {
            return Some(runs);
        }
        let (len_size, off_size) = (usize::from(header & 15), usize::from(header >> 4));
        if len_size == 0 || len_size > 8 || off_size == 0 || off_size > 8 {
            return None;
        }
        at += 1;
        let mut length = 0u64;
        for (i, b) in list.get(at..at + len_size)?.iter().enumerate() {
            length |= u64::from(*b) << (8 * i);
        }
        at += len_size;
        let bytes = list.get(at..at + off_size)?;
        let mut delta = i64::from(bytes[off_size - 1] as i8);
        for b in bytes[..off_size - 1].iter().rev() {
            delta = (delta << 8) | i64::from(*b);
        }
        at += off_size;
        lcn = lcn.checked_add(delta)?;
        runs.push((
            u64::try_from(lcn).ok()?.checked_mul(cluster)?,
            length.checked_mul(cluster)?,
        ));
    }
}

fn mft_extents(vol: &impl Volume, geo: &Geometry) -> Result<(Vec<(u64, u64)>, u64), String> {
    let mut first = vec![0u8; (geo.record as u64).max(geo.cluster) as usize];
    vol.read_at(geo.mft_offset, &mut first)
        .map_err(|e| format!("read $MFT record: {e}"))?;
    let record = &mut first[..geo.record];
    if !record.starts_with(b"FILE") || apply_fixups(record).is_none() {
        return Err("$MFT record is damaged".into());
    }
    if attributes(record).any(|a| a.kind == ATTR_ATTRIBUTE_LIST) {
        return Err("$MFT is split across records, which is not supported".into());
    }
    let data = attributes(record)
        .find(|a| a.kind == ATTR_DATA && !a.named && a.non_resident)
        .ok_or("$MFT has no data attribute")?;
    let size = data.data_size().ok_or("$MFT has no size")?;
    let runs = data_runs(&data, geo.cluster).ok_or("$MFT data runs are damaged")?;
    Ok((runs, size))
}

const NO_LINK: u32 = u32::MAX;

// One per record. Names live in `Table::names` and links in `Table::links`:
// a `Vec` and a `Box<str>` per record cost two allocations each, about 130
// MB over the 1.4M records of a whole volume.
struct Node {
    in_use: bool,
    is_dir: bool,
    /// Head of this record's list in `Table::links`.
    first_link: u32,
    size: u64,
    mtime: i64,
    ctime: i64,
    attributes: u32,
    reparse_tag: u32,
}

impl Default for Node {
    fn default() -> Self {
        Self {
            in_use: false,
            is_dir: false,
            first_link: NO_LINK,
            size: 0,
            mtime: 0,
            ctime: 0,
            attributes: 0,
            reparse_tag: 0,
        }
    }
}

/// One name of a record in its parent directory; hard links have several.
struct Link {
    parent: u32,
    next: u32,
    name_off: u32,
    name_len: u32,
}

#[derive(Default)]
struct Table {
    nodes: Vec<Node>,
    links: Vec<Link>,
    names: String,
}

impl Table {
    fn name(&self, link: u32) -> &str {
        let l = &self.links[link as usize];
        &self.names[l.name_off as usize..(l.name_off + l.name_len) as usize]
    }

    fn links_of(&self, node: u32) -> impl Iterator<Item = u32> + '_ {
        let mut at = self.nodes[node as usize].first_link;
        std::iter::from_fn(move || {
            let cur = at;
            (cur != NO_LINK).then(|| {
                at = self.links[cur as usize].next;
                cur
            })
        })
    }

    /// Adds a name unless the record already has it under that parent.
    fn add_link(&mut self, node: u32, parent: u32, units: impl Iterator<Item = u16>) -> Option<()> {
        let start = self.names.len();
        self.names
            .extend(char::decode_utf16(units).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)));
        let name_off = u32::try_from(start).ok()?;
        let name_len = u32::try_from(self.names.len() - start).ok()?;
        let duplicate = self.links_of(node).any(|l| {
            self.links[l as usize].parent == parent && self.name(l) == &self.names[start..]
        });
        if duplicate {
            self.names.truncate(start);
            return Some(());
        }
        let link = u32::try_from(self.links.len()).ok()?;
        let head = &mut self.nodes[node as usize].first_link;
        self.links.push(Link {
            parent,
            next: *head,
            name_off,
            name_len,
        });
        *head = link;
        Some(())
    }
}

/// One file as the crawl sees it.
pub struct MftFile {
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub mtime: i64,
    pub ctime: i64,
    pub attributes: u32,
}

fn merge_record(table: &mut Table, number: u32, record: &[u8]) -> Option<()> {
    let flags = u16_at(record, 22)?;
    let base = (u64_at(record, 32)? & REFERENCE_MASK) as u32;
    let index = if base == 0 { number } else { base };
    if index as usize >= table.nodes.len() {
        table.nodes.resize_with(index as usize + 1, Node::default);
    }
    let node = &mut table.nodes[index as usize];
    if base == 0 {
        node.in_use = flags & RECORD_IN_USE != 0;
        node.is_dir = flags & RECORD_IS_DIRECTORY != 0;
    }
    for attr in attributes(record) {
        let node = &mut table.nodes[index as usize];
        match attr.kind {
            ATTR_STANDARD_INFORMATION => {
                let c = attr.content()?;
                node.ctime = filetime_to_unix(u64_at(c, 0)?);
                node.mtime = filetime_to_unix(u64_at(c, 8)?);
                node.attributes = u32_at(c, 32)?;
            }
            ATTR_FILE_NAME => {
                let c = attr.content()?;
                if *c.get(65)? == NAMESPACE_DOS {
                    continue;
                }
                let parent = (u64_at(c, 0)? & REFERENCE_MASK) as u32;
                let units = c
                    .get(66..66 + 2 * usize::from(*c.get(64)?))?
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|u| u16::from_le_bytes(*u));
                table.add_link(index, parent, units)?;
            }
            ATTR_DATA if !attr.named => {
                if let Some(size) = attr.data_size() {
                    node.size = size;
                }
            }
            ATTR_REPARSE_POINT => {
                if let Some(c) = attr.content() {
                    node.reparse_tag = u32_at(c, 0)?;
                }
            }
            _ => {}
        }
    }
    Some(())
}

/// The whole file table of one volume, ready to walk.
pub struct MftIndex {
    table: Table,
    /// `(parent, child, link)`, sorted so a directory's children are
    /// one binary search away.
    children: Vec<(u32, u32, u32)>,
}

impl MftIndex {
    pub fn read(vol: &impl Volume) -> Result<Self, String> {
        let mut boot = vec![0u8; BOOT_READ];
        vol.read_at(0, &mut boot)
            .map_err(|e| format!("read boot sector: {e}"))?;
        let geo = parse_boot(&boot).ok_or("not an NTFS volume")?;
        let (extents, size) = mft_extents(vol, &geo)?;

        let chunk = READ_CHUNK - READ_CHUNK % geo.cluster.max(geo.record as u64);
        let mut buf = vec![0u8; chunk as usize];
        let mut table = Table::default();
        table.nodes.reserve((size / geo.record as u64) as usize);
        let mut number = 0u32;
        let mut left = size;
        for (start, length) in extents {
            let mut done = 0;
            while done < length && left > 0 {
                let want = chunk.min(length - done);
                let part = &mut buf[..want as usize];
                vol.read_at(start + done, part)
                    .map_err(|e| format!("read $MFT: {e}"))?;
                done += want;
                for record in part.chunks_exact_mut(geo.record) {
                    if left == 0 {
                        break;
                    }
                    left = left.saturating_sub(geo.record as u64);
                    if record.starts_with(b"FILE") && apply_fixups(record).is_some() {
                        // A record that does not parse is skipped like a torn
                        // one; later changes come from the change log.
                        let _ = merge_record(&mut table, number, record);
                    }
                    number += 1;
                }
            }
        }
        if table.nodes.len() <= ROOT_RECORD as usize || !table.nodes[ROOT_RECORD as usize].in_use {
            return Err("$MFT has no root directory".into());
        }
        table.names.shrink_to_fit();

        let mut children = Vec::with_capacity(table.links.len());
        for (child, node) in table.nodes.iter().enumerate() {
            if !node.in_use || (child as u32) < FIRST_USER_RECORD {
                continue;
            }
            for link in table.links_of(child as u32) {
                children.push((table.links[link as usize].parent, child as u32, link));
            }
        }
        children.sort_unstable();
        Ok(Self { table, children })
    }

    fn children_of(&self, dir: u32) -> &[(u32, u32, u32)] {
        let start = self.children.partition_point(|c| c.0 < dir);
        let len = self.children[start..].partition_point(|c| c.0 == dir);
        &self.children[start..start + len]
    }

    fn file(&self, number: u32) -> MftFile {
        let n = &self.table.nodes[number as usize];
        MftFile {
            is_dir: n.is_dir,
            is_symlink: matches!(n.reparse_tag, REPARSE_SYMLINK | REPARSE_MOUNT_POINT),
            size: if n.is_dir { 0 } else { n.size },
            mtime: n.mtime,
            ctime: n.ctime,
            attributes: n.attributes,
        }
    }

    fn find(&self, root: &Path) -> Result<u32, String> {
        let mut at = ROOT_RECORD;
        for part in root.components() {
            let Component::Normal(part) = part else {
                continue;
            };
            let want = part.to_string_lossy().to_lowercase();
            at = self
                .children_of(at)
                .iter()
                .find(|(_, _, link)| self.table.name(*link).to_lowercase() == want)
                .map(|c| c.1)
                .ok_or_else(|| format!("{} is not in the file table", root.display()))?;
        }
        Ok(at)
    }

    /// Visits `root` and everything below it, each directory before its
    /// contents; `visit` returns false to leave a path out, and its subtree.
    /// Fails only before the first visit, so a caller that streams what it
    /// visits has sent nothing when this returns an error.
    pub fn walk(
        &self,
        root: &Path,
        mut visit: impl FnMut(&Path, &MftFile) -> bool,
    ) -> Result<(), String> {
        let top = self.find(root)?;
        let mut stack = vec![(top, root.to_path_buf())];
        while let Some((number, path)) = stack.pop() {
            let file = self.file(number);
            if !visit(&path, &file) || !file.is_dir || file.is_symlink {
                continue;
            }
            for (_, child, link) in self.children_of(number) {
                stack.push((*child, path.join(self.table.name(*link))));
            }
        }
        Ok(())
    }
}

/// `C:` for a path on a lettered drive. A volume mounted on a folder has no
/// letter here, and the walk handles it.
fn drive_of(path: &Path) -> Result<String, String> {
    match path.components().next() {
        Some(Component::Prefix(p)) => match p.kind() {
            std::path::Prefix::Disk(d) | std::path::Prefix::VerbatimDisk(d) => {
                Ok(format!("{}:", char::from(d).to_ascii_uppercase()))
            }
            _ => Err(format!("{} is not on a drive letter", path.display())),
        },
        _ => Err(format!("{} is not on a drive letter", path.display())),
    }
}

#[cfg(windows)]
fn open_volume(drive: &str) -> Result<impl Volume, String> {
    use std::os::windows::fs::{FileExt, OpenOptionsExt};

    struct Disk(std::fs::File);

    impl Volume for Disk {
        fn read_at(&self, mut offset: u64, mut buf: &mut [u8]) -> io::Result<()> {
            while !buf.is_empty() {
                match self.0.seek_read(buf, offset)? {
                    0 => return Err(io::ErrorKind::UnexpectedEof.into()),
                    n => {
                        offset += n as u64;
                        buf = &mut buf[n..];
                    }
                }
            }
            Ok(())
        }
    }

    const FILE_SHARE_READ_WRITE_DELETE: u32 = 7;
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ_WRITE_DELETE)
        .open(format!(r"\\.\{drive}"))
        .map(Disk)
        .map_err(|e| format!(r"open \\.\{drive}: {e}"))
}

#[cfg(not(windows))]
fn open_volume(drive: &str) -> Result<NoVolume, String> {
    Err(format!("reading {drive} directly needs Windows"))
}

#[cfg(not(windows))]
struct NoVolume;

#[cfg(not(windows))]
impl Volume for NoVolume {
    fn read_at(&self, _: u64, _: &mut [u8]) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

/// Volumes already read by this crawl, since several roots often share one.
#[derive(Default)]
pub struct MftCache(HashMap<String, Arc<MftIndex>>);

/// Whether `path` is a whole volume, such as `C:\`.
pub fn is_volume_root(path: &Path) -> bool {
    let mut parts = path.components();
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(Component::Prefix(p)), Some(Component::RootDir), None)
            if matches!(p.kind(), std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_))
    )
}

impl MftCache {
    /// Whether this drive's table has been read already.
    pub fn has_drive_of(&self, root: &Path) -> bool {
        drive_of(root).is_ok_and(|d| self.0.contains_key(&d))
    }

    /// Frees this drive's table unless a root in `later` is on that drive.
    pub fn release_unless_needed(&mut self, root: &Path, later: &[PathBuf]) {
        let Ok(drive) = drive_of(root) else { return };
        if !later.iter().any(|p| drive_of(p).is_ok_and(|d| d == drive)) {
            self.0.remove(&drive);
        }
    }

    pub fn for_root(&mut self, root: &Path) -> Result<Arc<MftIndex>, String> {
        let drive = drive_of(root)?;
        if let Some(index) = self.0.get(&drive) {
            return Ok(Arc::clone(index));
        }
        let index = Arc::new(MftIndex::read(&open_volume(&drive)?)?);
        self.0.insert(drive, Arc::clone(&index));
        Ok(index)
    }
}

#[cfg(test)]
#[path = "mft_tests.rs"]
mod tests;
