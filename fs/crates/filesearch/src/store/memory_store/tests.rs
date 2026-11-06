use super::*;
use super::matching::{glob_match, like_match};
use tempfile::TempDir;

fn sample(path: &str) -> FileRecord {
    FileRecord {
        id:              None,
        path:            path.to_owned(),
        name:            path.split('/').next_back().unwrap_or(path).to_owned(),
        extension:       Some("rs".into()),
        size_bytes:      1024,
        mtime:           1_000_000,
        ctime:           1_000_000,
        is_dir:          false,
        is_hidden:       false,
        is_symlink:      false,
        attrib_bitmask:  0,
        content_indexed: false,
        tantivy_id:      None,
        indexed_at:      1_000_000,
        archive_path:    None,
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
    let results = store.search_conditions(
        &[SqlCondition::NameLike("%foo%".into())], 100,
    ).unwrap();
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
    let results = store.search_name("%foo%", &MetaFilters::default(), 100).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "foo.rs");
    let all = store.search_name("%", &MetaFilters::default(), 100).unwrap();
    assert_eq!(all.len(), 2);
}

#[test]
fn search_extension_exact() {
    let store = make_store();
    store.upsert_file(&sample("/a/foo.rs"));
    let mut toml = sample("/a/bar.toml");
    toml.extension = Some("toml".into());
    store.upsert_file(&toml);
    let rs = store.search_extension("rs", &MetaFilters::default(), 100).unwrap();
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
    assert_eq!(gen, 2);
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
    let paths: Vec<String> = (0..2000).map(|i| format!("/d/{i:04}/file_{i}.rs")).collect();
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
            names.iter().enumerate().filter(|(_, n)| n.starts_with('m')).map(|(i, _)| i).collect()
        })
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, "/b/main.rs");
}
