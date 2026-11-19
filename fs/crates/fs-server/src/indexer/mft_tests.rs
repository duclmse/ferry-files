use std::path::PathBuf;

use super::*;

const RECORD: usize = 1024;
const CLUSTER: usize = 4096;
const MFT_CLUSTER: usize = 2;
const WINDOWS_2023: u64 = (1_700_000_000 + 11_644_473_600) * FILETIME_TICKS_PER_SEC;

struct Image(Vec<u8>);

impl Volume for Image {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let start = offset as usize;
        let slice = self
            .0
            .get(start..start + buf.len())
            .ok_or(io::ErrorKind::UnexpectedEof)?;
        buf.copy_from_slice(slice);
        Ok(())
    }
}

fn resident(kind: u32, name_len: u8, content: &[u8]) -> Vec<u8> {
    let len = (24 + content.len()).next_multiple_of(8);
    let mut a = vec![0u8; len];
    a[0..4].copy_from_slice(&kind.to_le_bytes());
    a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
    a[9] = name_len;
    a[16..20].copy_from_slice(&(content.len() as u32).to_le_bytes());
    a[20..22].copy_from_slice(&24u16.to_le_bytes());
    a[24..24 + content.len()].copy_from_slice(content);
    a
}

fn non_resident(kind: u32, vcn: u64, size: u64, runs: &[u8]) -> Vec<u8> {
    let len = (64 + runs.len() + 1).next_multiple_of(8);
    let mut a = vec![0u8; len];
    a[0..4].copy_from_slice(&kind.to_le_bytes());
    a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
    a[8] = 1;
    a[16..24].copy_from_slice(&vcn.to_le_bytes());
    a[32..34].copy_from_slice(&64u16.to_le_bytes());
    a[48..56].copy_from_slice(&size.to_le_bytes());
    a[64..64 + runs.len()].copy_from_slice(runs);
    a
}

fn standard_info(created: u64, modified: u64, attrs: u32) -> Vec<u8> {
    let mut c = vec![0u8; 48];
    c[0..8].copy_from_slice(&created.to_le_bytes());
    c[8..16].copy_from_slice(&modified.to_le_bytes());
    c[32..36].copy_from_slice(&attrs.to_le_bytes());
    resident(ATTR_STANDARD_INFORMATION, 0, &c)
}

fn file_name(parent: u64, name: &str, namespace: u8) -> Vec<u8> {
    let units: Vec<u16> = name.encode_utf16().collect();
    let mut c = vec![0u8; 66 + units.len() * 2];
    // The high bits are the sequence number, which must be masked off.
    c[0..8].copy_from_slice(&(parent | (7u64 << 48)).to_le_bytes());
    c[64] = units.len() as u8;
    c[65] = namespace;
    for (i, u) in units.iter().enumerate() {
        c[66 + 2 * i..68 + 2 * i].copy_from_slice(&u.to_le_bytes());
    }
    resident(ATTR_FILE_NAME, 0, &c)
}

fn record(flags: u16, base: u64, attrs: &[Vec<u8>]) -> Vec<u8> {
    let mut r = vec![0u8; RECORD];
    r[..4].copy_from_slice(b"FILE");
    r[4..6].copy_from_slice(&48u16.to_le_bytes());
    r[6..8].copy_from_slice(&3u16.to_le_bytes());
    r[20..22].copy_from_slice(&56u16.to_le_bytes());
    r[22..24].copy_from_slice(&flags.to_le_bytes());
    r[32..40].copy_from_slice(&base.to_le_bytes());
    let mut at = 56;
    for a in attrs {
        r[at..at + a.len()].copy_from_slice(a);
        at += a.len();
    }
    r[at..at + 4].copy_from_slice(&ATTR_END.to_le_bytes());
    // Protect each 512-byte unit the way NTFS does.
    let check = [0xAB, 0xCD];
    r[48..50].copy_from_slice(&check);
    for unit in 1..3 {
        let end = unit * 512;
        let saved = [r[end - 2], r[end - 1]];
        r[48 + 2 * unit..50 + 2 * unit].copy_from_slice(&saved);
        r[end - 2..end].copy_from_slice(&check);
    }
    r
}

/// A volume whose MFT is split into two runs, to exercise the run list.
fn image(user: Vec<(usize, Vec<u8>)>) -> Image {
    let first_run = 8usize; // clusters, so 32 records
    let second_lcn = 40usize;
    let total_records = 64usize;
    let mut disk = vec![0u8; (second_lcn + 16) * CLUSTER];
    disk[3..11].copy_from_slice(b"NTFS    ");
    disk[11..13].copy_from_slice(&512u16.to_le_bytes());
    disk[13] = 8;
    disk[48..56].copy_from_slice(&(MFT_CLUSTER as u64).to_le_bytes());
    disk[64] = (-10i8) as u8; // 2^10 bytes per record

    let delta = (second_lcn - MFT_CLUSTER) as u8;
    let runs = [0x11, first_run as u8, MFT_CLUSTER as u8, 0x11, 8, delta];
    let mft = record(
        RECORD_IN_USE,
        0,
        &[non_resident(
            ATTR_DATA,
            0,
            (total_records * RECORD) as u64,
            &runs,
        )],
    );
    let root = record(
        RECORD_IN_USE | RECORD_IS_DIRECTORY,
        0,
        &[file_name(5, ".", 1)],
    );
    let offset = |n: usize| {
        if n * RECORD < first_run * CLUSTER {
            MFT_CLUSTER * CLUSTER + n * RECORD
        } else {
            second_lcn * CLUSTER + n * RECORD - first_run * CLUSTER
        }
    };
    let mut put = |n: usize, r: &[u8]| {
        let at = offset(n);
        disk[at..at + RECORD].copy_from_slice(r);
    };
    put(0, &mft);
    put(5, &root);
    for (n, r) in user {
        put(n, &r);
    }
    Image(disk)
}

fn dir(parent: u64, name: &str) -> Vec<u8> {
    record(
        RECORD_IN_USE | RECORD_IS_DIRECTORY,
        0,
        &[
            standard_info(WINDOWS_2023, WINDOWS_2023, 0x10),
            file_name(parent, name, 1),
        ],
    )
}

fn file(parent: u64, name: &str, size: u32) -> Vec<u8> {
    record(
        RECORD_IN_USE,
        0,
        &[
            standard_info(WINDOWS_2023, WINDOWS_2023 + 50_000_000, 0x20),
            file_name(parent, name, 1),
            resident(ATTR_DATA, 0, &vec![1u8; size as usize]),
        ],
    )
}

fn listing(index: &MftIndex, root: &str) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    index
        .walk(Path::new(root), |p, f| {
            // The walk joins with the platform separator, as the directory
            // walker does; the expectations here are written with `/`.
            out.push((p.to_string_lossy().replace('\\', "/"), f.size));
            true
        })
        .unwrap();
    out.sort();
    out
}

#[test]
fn reads_a_tree_across_mft_runs() {
    // Record 40 lives in the second run.
    let img = image(vec![
        (24, dir(5, "Users")),
        (25, file(24, "a.txt", 12)),
        (40, file(24, "far.txt", 3)),
    ]);
    let index = MftIndex::read(&img).unwrap();
    assert_eq!(
        listing(&index, "/"),
        [
            ("/".to_owned(), 0),
            ("/Users".to_owned(), 0),
            ("/Users/a.txt".to_owned(), 12),
            ("/Users/far.txt".to_owned(), 3),
        ]
    );
}

#[test]
fn reports_times_and_attributes() {
    let index = MftIndex::read(&image(vec![(24, file(5, "a", 1))])).unwrap();
    let mut seen = None;
    index
        .walk(Path::new("/a"), |_, f| {
            seen = Some((f.ctime, f.mtime, f.attributes, f.is_dir));
            true
        })
        .unwrap();
    assert_eq!(seen, Some((1_700_000_000, 1_700_000_005, 0x20, false)));
}

#[test]
fn skips_dos_names_deleted_files_and_metafiles() {
    let dos_and_long = record(
        RECORD_IN_USE,
        0,
        &[
            standard_info(WINDOWS_2023, WINDOWS_2023, 0),
            file_name(5, "LONGNA~1.TXT", NAMESPACE_DOS),
            file_name(5, "longname.txt", 1),
        ],
    );
    let deleted = record(0, 0, &[file_name(5, "gone.txt", 1)]);
    let img = image(vec![
        (24, dos_and_long),
        (25, deleted),
        (11, record(RECORD_IN_USE, 0, &[file_name(5, "$Extend", 1)])),
    ]);
    let names: Vec<String> = listing(&MftIndex::read(&img).unwrap(), "/")
        .into_iter()
        .map(|e| e.0)
        .collect();
    assert_eq!(names, ["/", "/longname.txt"]);
}

#[test]
fn hard_links_appear_at_each_path() {
    let linked = record(
        RECORD_IN_USE,
        0,
        &[file_name(24, "one", 1), file_name(25, "two", 1)],
    );
    let img = image(vec![(24, dir(5, "a")), (25, dir(5, "b")), (26, linked)]);
    let names: Vec<String> = listing(&MftIndex::read(&img).unwrap(), "/")
        .into_iter()
        .map(|e| e.0)
        .collect();
    assert_eq!(names, ["/", "/a", "/a/one", "/b", "/b/two"]);
}

#[test]
fn extension_records_merge_into_their_base() {
    let base = record(
        RECORD_IN_USE,
        0,
        &[
            standard_info(WINDOWS_2023, WINDOWS_2023, 0),
            file_name(5, "big.bin", 1),
        ],
    );
    // Extents after the first carry no size and must not clear it.
    let extent_a = record(
        RECORD_IN_USE,
        24,
        &[non_resident(ATTR_DATA, 0, 9_000_000, &[0x11, 1, 1])],
    );
    let extent_b = record(
        RECORD_IN_USE,
        24,
        &[non_resident(ATTR_DATA, 100, 0, &[0x11, 1, 1])],
    );
    let img = image(vec![(24, base), (25, extent_a), (26, extent_b)]);
    assert_eq!(
        listing(&MftIndex::read(&img).unwrap(), "/big.bin"),
        [("/big.bin".to_owned(), 9_000_000)]
    );
}

#[test]
fn named_streams_do_not_change_the_size() {
    let r = record(
        RECORD_IN_USE,
        0,
        &[
            file_name(5, "a", 1),
            resident(ATTR_DATA, 0, &[1, 2]),
            resident(ATTR_DATA, 4, &[0u8; 40]),
        ],
    );
    assert_eq!(
        listing(&MftIndex::read(&image(vec![(24, r)])).unwrap(), "/a"),
        [("/a".to_owned(), 2)]
    );
}

#[test]
fn a_torn_record_is_skipped_and_the_rest_still_read() {
    let mut torn = file(5, "torn", 1);
    torn[510] ^= 0xFF;
    let img = image(vec![(24, torn), (25, file(5, "ok", 1))]);
    let names: Vec<String> = listing(&MftIndex::read(&img).unwrap(), "/")
        .into_iter()
        .map(|e| e.0)
        .collect();
    assert_eq!(names, ["/", "/ok"]);
}

#[test]
fn symlinks_are_listed_but_not_followed() {
    let tag = REPARSE_SYMLINK.to_le_bytes();
    let link = record(
        RECORD_IN_USE | RECORD_IS_DIRECTORY,
        0,
        &[
            file_name(5, "link", 1),
            resident(ATTR_REPARSE_POINT, 0, &tag),
        ],
    );
    let img = image(vec![(24, link), (25, file(24, "inside", 1))]);
    let names: Vec<String> = listing(&MftIndex::read(&img).unwrap(), "/")
        .into_iter()
        .map(|e| e.0)
        .collect();
    assert_eq!(names, ["/", "/link"]);
}

#[test]
fn visit_can_prune_a_subtree() {
    let img = image(vec![
        (24, dir(5, "skip")),
        (25, file(24, "x", 1)),
        (26, file(5, "keep", 1)),
    ]);
    let mut seen = Vec::new();
    MftIndex::read(&img)
        .unwrap()
        .walk(Path::new("/"), |p, _| {
            seen.push(p.to_path_buf());
            !p.ends_with("skip")
        })
        .unwrap();
    assert!(seen.contains(&PathBuf::from("/keep")));
    assert!(!seen.contains(&PathBuf::from("/skip/x")));
}

#[test]
fn a_root_is_found_ignoring_case_and_a_missing_one_is_an_error() {
    let img = image(vec![(24, dir(5, "Users")), (25, file(24, "a", 1))]);
    let index = MftIndex::read(&img).unwrap();
    assert_eq!(listing(&index, "/users").len(), 2);
    assert!(index.walk(Path::new("/nope"), |_, _| true).is_err());
}

#[test]
fn rejects_what_it_cannot_read() {
    assert!(MftIndex::read(&Image(vec![0; 8192])).is_err(), "not NTFS");
    assert!(
        MftIndex::read(&Image(vec![0; 16])).is_err(),
        "short volume reads fail"
    );
    let mut img = image(vec![]);
    img.0[MFT_CLUSTER * CLUSTER] = b'X';
    assert!(MftIndex::read(&img).is_err(), "bad $MFT record");
}

#[test]
fn a_negative_run_offset_moves_backwards() {
    // length 1 at +100, then length 1 at -40: LCNs 100 and 60.
    let attr_bytes = non_resident(ATTR_DATA, 0, 0, &[0x11, 1, 100, 0x11, 1, 0xD8]);
    let attr = Attribute {
        kind: ATTR_DATA,
        non_resident: true,
        named: false,
        raw: &attr_bytes,
    };
    assert_eq!(data_runs(&attr, 10).unwrap(), [(1000, 10), (600, 10)]);
}

#[test]
fn only_lettered_drives_are_accepted_off_windows() {
    let root = Path::new("/");
    assert!(drive_of(root).is_err());
    assert!(MftCache::default().for_root(root).is_err());
}
