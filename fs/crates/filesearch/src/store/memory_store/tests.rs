use super::matching::{glob_match, like_match};
use super::*;
use tempfile::TempDir;

fn sample(path: &str) -> FileRecord {
    FileRecord {
        id: None,
        path: path.to_owned(),
        name: path.split('/').next_back().unwrap_or(path).to_owned(),
        extension: Some("rs".into()),
        size_bytes: 1024,
        mtime: 1_000_000,
        ctime: 1_000_000,
        is_dir: false,
        is_hidden: false,
        is_symlink: false,
        attrib_bitmask: 0,
        content_indexed: false,
        tantivy_id: None,
        indexed_at: 1_000_000,
        archive_path: None,
    }
}

fn make_store() -> MemoryStore {
    MemoryStore::new(std::path::Path::new(":memory-test:")).unwrap()
}

#[test]
fn upsert_and_get() {
    let store = make_store();
    store.upsert_file(&sample("/a/b.rs"));
    assert!(store.get_by_path("/a/b.rs").unwrap().is_some());
    assert!(store.get_by_path("/a/c.rs").unwrap().is_none());
}

#[test]
fn remove_updates_index() {
    let store = make_store();
    store.upsert_file(&sample("/a/b.rs"));
    store.upsert_file(&sample("/a/c.rs"));
    store.remove_file("/a/b.rs");
    assert!(store.get_by_path("/a/b.rs").unwrap().is_none());
    assert!(store.get_by_path("/a/c.rs").unwrap().is_some());
}

#[test]
fn total_files_excludes_archive_entries() {
    let store = make_store();
    store.upsert_file(&sample("/real.rs"));
    let mut inner = sample("/z.zip!/inner.rs");
    inner.archive_path = Some("/z.zip".into());
    store.upsert_file(&inner);
    assert_eq!(store.total_files(), 1);
    assert_eq!(store.total_archive_entries(), 1);
}

#[test]
fn search_conditions_name_like() {
    let store = make_store();
    store.upsert_file(&sample("/a/foo.rs"));
    store.upsert_file(&sample("/a/bar.toml"));
    let results = store
        .search_conditions(&[SqlCondition::NameLike("%foo%".into())], 100)
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].path, "/a/foo.rs");
}

#[test]
fn crawl_generation_remove_stale() {
    let store = make_store();
    store.upsert_file(&sample("/w/keep.rs"));
    store.upsert_file(&sample("/w/stale.rs"));
    let gen = store.begin_crawl_generation();
    store.batch_upsert_crawl(&[(sample("/w/keep.rs"), "/w".into())], gen);
    let removed = store.remove_stale_for_root("/w", gen);
    assert_eq!(removed, 1);
    assert!(store.get_by_path("/w/keep.rs").unwrap().is_some());
    assert!(store.get_by_path("/w/stale.rs").unwrap().is_none());
}

#[test]
fn flush_and_reload() {
    let dir = TempDir::new().unwrap();
    {
        let store = MemoryStore::new(dir.path()).unwrap();
        store.upsert_file(&sample("/a/b.rs"));
        store.flush_to_disk(dir.path()).unwrap();
    }
    let store2 = MemoryStore::new(dir.path()).unwrap();
    assert!(store2.get_by_path("/a/b.rs").unwrap().is_some());
}

#[test]
fn search_name_like() {
    let store = make_store();
    store.upsert_file(&sample("/a/foo.rs"));
    store.upsert_file(&sample("/a/bar.toml"));
    let results = store
        .search_name("%foo%", &MetaFilters::default(), 100)
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "foo.rs");
    let all = store
        .search_name("%", &MetaFilters::default(), 100)
        .unwrap();
    assert_eq!(all.len(), 2);
}

#[test]
fn search_extension_exact() {
    let store = make_store();
    store.upsert_file(&sample("/a/foo.rs"));
    let mut toml = sample("/a/bar.toml");
    toml.extension = Some("toml".into());
    store.upsert_file(&toml);
    let rs = store
        .search_extension("rs", &MetaFilters::default(), 100)
        .unwrap();
    assert_eq!(rs.len(), 1);
    assert_eq!(rs[0].name, "foo.rs");
}

#[test]
fn paths_under_returns_subtree() {
    let store = make_store();
    store.upsert_file(&sample("/root/a/x.rs"));
    store.upsert_file(&sample("/root/a/b/y.rs"));
    store.upsert_file(&sample("/root/c/z.rs"));
    store.upsert_file(&sample("/other/w.rs"));
    let mut paths = store.paths_under("/root/a").unwrap();
    paths.sort();
    assert_eq!(paths, vec!["/root/a/b/y.rs", "/root/a/x.rs"]);
}

#[test]
fn like_match_wildcards() {
    assert!(like_match("hello world", "%world"));
    assert!(like_match("hello world", "hello%"));
    assert!(like_match("hello world", "%ello%"));
    assert!(!like_match("hello world", "%xyz%"));
    assert!(like_match("Hello", "hello")); // case-insensitive
}

#[test]
fn glob_match_wildcards() {
    assert!(glob_match("file.rs", "*.rs"));
    assert!(glob_match("file.rs", "file.*"));
    assert!(!glob_match("FILE.rs", "file.*")); // case-sensitive
    assert!(glob_match("file.rs", "????.rs"));
}

// ---------------------------------------------------------------------------
// Compact layout (docs/plans/everything-borrowing.md, phases 1 and 3)
// ---------------------------------------------------------------------------

/// `FileRecord` has no `PartialEq`; its `Debug` output covers every field.
fn assert_same(got: Option<FileRecord>, want: &FileRecord) {
    assert_eq!(format!("{got:?}"), format!("{:?}", Some(want)));
}

#[test]
fn snapshot_v3_keeps_meta_generation_and_odd_records() {
    let dir = TempDir::new().unwrap();
    let mut odd = sample("/a/odd.rs");
    odd.name = "renamed".into();
    odd.archive_path = Some("/elsewhere.zip".into());
    odd.id = Some(7);
    odd.tantivy_id = Some("t7".into());
    {
        let store = MemoryStore::new(dir.path()).unwrap();
        store.upsert_file(&sample("/a/b.rs"));
        store.upsert_file(&odd);
        store.set_meta("last_crawl_time", "123");
        store.set_usn_checkpoint(42);
        let gen = store.begin_crawl_generation();
        store.batch_upsert_crawl(&[(sample("/w/x.rs"), "/w".into())], gen);
        store.flush_to_disk(dir.path()).unwrap();
    }
    let store = MemoryStore::new(dir.path()).unwrap();
    assert_eq!(store.get_meta("last_crawl_time").as_deref(), Some("123"));
    assert_eq!(store.get_usn_checkpoint(), 42);
    assert_same(store.get_by_path("/a/odd.rs").unwrap(), &odd);
    // The crawl generation counter survived, so the tagged entry is current.
    let gen = store.begin_crawl_generation();
    assert_eq!(gen, 2i64);
    store.batch_upsert_crawl(&[(sample("/w/x.rs"), "/w".into())], gen);
    assert_eq!(store.remove_stale_for_root("/w", gen), 0);
}

#[test]
fn legacy_v2_snapshot_still_loads() {
    let dir = TempDir::new().unwrap();
    let body = postcard::to_allocvec(&vec![sample("/a/b.rs")]).unwrap();
    let mut buf = Vec::new();
    buf.extend_from_slice(b"FSID");
    buf.extend_from_slice(&2u16.to_le_bytes());
    buf.extend_from_slice(&9u64.to_le_bytes());
    buf.extend_from_slice(&(body.len() as u64).to_le_bytes());
    buf.extend_from_slice(&body);
    std::fs::write(dir.path().join("index.bin"), buf).unwrap();
    let store = MemoryStore::new(dir.path()).unwrap();
    assert_same(store.get_by_path("/a/b.rs").unwrap(), &sample("/a/b.rs"));
    assert_eq!(store.get_usn_checkpoint(), 9);
    assert!(store.get_meta("last_crawl_time").is_none());
}

#[test]
fn records_round_trip_exactly() {
    let store = make_store();
    let mut dir = sample("/a/dir");
    dir.is_dir = true;
    dir.extension = None;
    let mut inner = sample("/a/x.zip!/in/f.txt");
    inner.extension = Some("txt".into());
    inner.archive_path = Some("/a/x.zip".into());
    let mut flags = sample("/a/.h");
    flags.is_hidden = true;
    flags.is_symlink = true;
    flags.content_indexed = true;
    flags.attrib_bitmask = 0x22;
    flags.size_bytes = u64::MAX;
    flags.mtime = -5;
    let mut empty_name = sample("/a/e");
    empty_name.name = String::new();
    for r in [&dir, &inner, &flags, &empty_name] {
        store.upsert_file(r);
    }
    for r in [&dir, &inner, &flags, &empty_name] {
        assert_same(store.get_by_path(&r.path).unwrap(), r);
    }
    assert_eq!(store.total_archive_entries(), 1);
    assert_eq!(store.remove_archive_entries("/a/x.zip"), 1);
}

#[test]
fn replace_updates_fields_and_side_table() {
    let store = make_store();
    let mut r = sample("/a/b.rs");
    r.tantivy_id = Some("t".into());
    store.upsert_file(&r);
    r.tantivy_id = None;
    r.size_bytes = 5;
    store.upsert_file(&r);
    assert_same(store.get_by_path("/a/b.rs").unwrap(), &r);
    assert_eq!(store.total_files(), 1);
}

#[test]
fn many_removals_keep_index_consistent_and_compact() {
    let store = make_store();
    let paths: Vec<String> = (0..2000)
        .map(|i| format!("/d/{i:04}/file_{i}.rs"))
        .collect();
    for (i, p) in paths.iter().enumerate() {
        let mut r = sample(p);
        if i % 3 == 0 {
            r.id = Some(i as i64);
        }
        store.upsert_file(&r);
    }
    for p in paths.iter().step_by(2) {
        assert!(store.remove_file(p));
    }
    assert!(!store.remove_file(&paths[0]));
    for (i, p) in paths.iter().enumerate() {
        let got = store.get_by_path(p).unwrap();
        if i % 2 == 0 {
            assert!(got.is_none(), "{p} should be gone");
        } else {
            let got = got.unwrap();
            assert_eq!(&got.path, p);
            assert_eq!(got.id, (i % 3 == 0).then_some(i as i64));
        }
    }
    assert_eq!(store.total_files(), 1000);
    assert_eq!(store.remove_directory("/d"), 1000);
    assert_eq!(store.total_files(), 0);
}

#[test]
fn corrupt_v3_snapshot_is_rejected() {
    let dir = TempDir::new().unwrap();
    {
        let store = MemoryStore::new(dir.path()).unwrap();
        store.upsert_file(&sample("/a/b.rs"));
        store.flush_to_disk(dir.path()).unwrap();
    }
    let p = dir.path().join("index.bin");
    let mut bytes = std::fs::read(&p).unwrap();
    bytes.truncate(bytes.len() - 3);
    std::fs::write(&p, bytes).unwrap();
    let store = MemoryStore::new(dir.path()).unwrap();
    assert_eq!(store.total_files(), 0);
}

#[test]
fn fuzzy_search_materializes_ranked_winners_only() {
    let store = make_store();
    for p in ["/a/main.rs", "/a/lib.rs", "/b/main.rs"] {
        store.upsert_file(&sample(p));
    }
    let conds = [SqlCondition::ParentPath("/b".into())];
    let hits = store
        .fuzzy_search(&conds, 100, &|names: &[&str]| {
            names
                .iter()
                .enumerate()
                .filter(|(_, n)| n.starts_with('m'))
                .map(|(i, _)| i)
                .collect()
        })
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, "/b/main.rs");
}

// ---------------------------------------------------------------------------
// Parent-pointer tree (phase 3 follow-up)
// ---------------------------------------------------------------------------

fn dir(path: &str) -> FileRecord {
    let mut r = sample(path);
    r.is_dir = true;
    r.extension = None;
    r
}

fn paths(records: Vec<FileRecord>) -> Vec<String> {
    let mut v: Vec<String> = records.into_iter().map(|r| r.path).collect();
    v.sort();
    v
}

#[test]
fn removed_parent_keeps_children_resolvable_and_revives() {
    let store = make_store();
    for r in [dir("/w"), dir("/w/src"), sample("/w/src/a.rs"), sample("/w/src/b.rs")] {
        store.upsert_file(&r);
    }
    assert!(store.remove_file("/w/src"));
    assert!(store.get_by_path("/w/src").unwrap().is_none());
    assert_same(store.get_by_path("/w/src/a.rs").unwrap(), &sample("/w/src/a.rs"));
    let under = [SqlCondition::ParentPath("/w/src".into())];
    assert_eq!(
        paths(store.search_conditions(&under, 10).unwrap()),
        ["/w/src/a.rs", "/w/src/b.rs"]
    );
    assert_eq!(store.children_of("/w/src").unwrap().len(), 2);
    assert!(store.children_of("/w").unwrap().is_empty());

    store.upsert_file(&dir("/w/src"));
    assert_same(store.get_by_path("/w/src").unwrap(), &dir("/w/src"));
    assert_eq!(store.children_of("/w").unwrap(), ["/w/src"]);
    assert_eq!(store.total_files(), 4);
    assert_eq!(store.remove_directory("/w"), 3);
    assert_eq!(store.total_files(), 1);
}

#[test]
fn prefix_predicates_match_whole_components_only() {
    let store = make_store();
    for r in [dir("/a"), sample("/a/x.rs"), dir("/ab"), sample("/ab/y.rs"), sample("/a/d/z.rs")] {
        store.upsert_file(&r);
    }
    assert_eq!(store.paths_under("/a").unwrap().len(), 2);
    assert_eq!(store.children_of("/a").unwrap(), ["/a/x.rs"]);
    assert!(store.has_child("/ab", "y%").unwrap());
    assert!(!store.has_child("/a", "z%").unwrap());
    assert_eq!(store.children_of("").unwrap().len(), 2);
    let depth = [SqlCondition::DepthOp(CmpOp::Eq, 3)];
    assert_eq!(paths(store.search_conditions(&depth, 10).unwrap()), ["/a/d/z.rs"]);
    let like = [SqlCondition::PathLike("%b/y%".into())];
    assert_eq!(paths(store.search_conditions(&like, 10).unwrap()), ["/ab/y.rs"]);
}

#[test]
fn archive_members_share_the_archive_entry() {
    let store = make_store();
    let mut zip = sample("/a/x.zip");
    zip.extension = Some("zip".into());
    let member = |p: &str| {
        let mut r = sample(p);
        r.archive_path = Some("/a/x.zip".into());
        r
    };
    store.upsert_file(&dir("/a"));
    store.upsert_file(&zip);
    for p in ["/a/x.zip!/top.rs", "/a/x.zip!/d", "/a/x.zip!/d/f.rs"] {
        store.upsert_file(&member(p));
    }
    for p in ["/a/x.zip!/top.rs", "/a/x.zip!/d/f.rs"] {
        assert_same(store.get_by_path(p).unwrap(), &member(p));
    }
    let of = [SqlCondition::ArchiveOf("/a/x.zip".into())];
    assert_eq!(store.search_conditions(&of, 10).unwrap().len(), 3);
    assert_eq!(store.list_archive_paths().unwrap(), ["/a/x.zip"]);
    assert_eq!(store.children_of("/a").unwrap(), ["/a/x.zip"]);
    assert_eq!(store.remove_archive_entries("/a/x.zip"), 3);
    assert_eq!(store.total_files(), 2);
}

#[test]
fn compaction_keeps_ghost_ancestors_and_snapshot_round_trips() {
    let dir_path = TempDir::new().unwrap();
    let store = MemoryStore::new(dir_path.path()).unwrap();
    store.upsert_file(&dir("/r"));
    for d in 0..100 {
        store.upsert_file(&dir(&format!("/r/{d}")));
        for f in 0..100 {
            store.upsert_file(&sample(&format!("/r/{d}/f{f}.rs")));
        }
    }
    // Directories go, one file in each stays: the directories become ghosts
    // and survive compaction; most files are dropped outright.
    for d in 0..100 {
        assert!(store.remove_file(&format!("/r/{d}")));
        for f in 1..100 {
            assert!(store.remove_file(&format!("/r/{d}/f{f}.rs")));
        }
    }
    assert_eq!(store.total_files(), 101);
    let check = |s: &MemoryStore| {
        for d in 0..100 {
            assert!(s.get_by_path(&format!("/r/{d}")).unwrap().is_none());
            assert!(s.get_by_path(&format!("/r/{d}/f1.rs")).unwrap().is_none());
            let f0 = format!("/r/{d}/f0.rs");
            assert_same(s.get_by_path(&f0).unwrap(), &sample(&f0));
        }
        assert_eq!(s.paths_under("/r").unwrap().len(), 100);
    };
    check(&store);
    store.flush_to_disk(dir_path.path()).unwrap();
    let back = MemoryStore::new(dir_path.path()).unwrap();
    assert_eq!(back.total_files(), 101);
    check(&back);
    back.upsert_file(&dir("/r/7"));
    assert_eq!(back.children_of("/r/7").unwrap(), ["/r/7/f0.rs"]);
}

#[test]
fn v3_snapshot_upgrades() {
    use super::state::{EntryV3, Extra};
    #[derive(serde::Serialize)]
    struct V3<'a> {
        arena: &'a str,
        entries: Vec<EntryV3>,
        exts: Vec<String>,
        extra: Vec<(u32, Extra)>,
        meta: Vec<(String, String)>,
        crawl_gen: i64,
    }
    let entry = |off: u64, path: &str, name_off: u32, archive_len: u32, flags: u8| EntryV3 {
        path_off: off,
        path_len: path.len() as u32,
        name_off,
        archive_len,
        ext: 1,
        size: 1024,
        mtime: 1_000_000,
        ctime: 1_000_000,
        indexed_at: 1_000_000,
        attrib: 0,
        flags,
        generation: 3,
    };
    // Child before parent, as swap-remove could leave them.
    let (child, parent, member) = ("/a/b/c.rs", "/a/b", "/a/b/c.rs!/m.rs");
    let arena = format!("{child}{parent}{member}");
    let snap = V3 {
        arena: &arena,
        entries: vec![
            entry(0, child, 5, 0, 0),
            entry(child.len() as u64, parent, 3, 0, 1),
            entry((child.len() + parent.len()) as u64, member, 11, child.len() as u32, 0),
        ],
        exts: vec!["rs".into()],
        extra: vec![],
        meta: vec![("last_crawl_time".into(), "5".into())],
        crawl_gen: 3,
    };
    let body = postcard::to_allocvec(&snap).unwrap();
    let mut buf = Vec::new();
    buf.extend_from_slice(b"FSID");
    buf.extend_from_slice(&3u16.to_le_bytes());
    buf.extend_from_slice(&7u64.to_le_bytes());
    buf.extend_from_slice(&(body.len() as u64).to_le_bytes());
    buf.extend_from_slice(&body);
    let dir_path = TempDir::new().unwrap();
    std::fs::write(dir_path.path().join("index.bin"), buf).unwrap();

    let store = MemoryStore::new(dir_path.path()).unwrap();
    assert_eq!(store.get_usn_checkpoint(), 7);
    assert_eq!(store.get_meta("last_crawl_time").as_deref(), Some("5"));
    assert_same(store.get_by_path(child).unwrap(), &sample(child));
    let mut m = sample(member);
    m.archive_path = Some(child.into());
    assert_same(store.get_by_path(member).unwrap(), &m);
    let p = store.get_by_path(parent).unwrap().unwrap();
    assert!(p.is_dir);
    // Generations carried over: a crawl at generation 3 finds nothing stale.
    assert_eq!(store.remove_stale_for_root("/a", 3), 0);
}
