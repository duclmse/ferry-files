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
