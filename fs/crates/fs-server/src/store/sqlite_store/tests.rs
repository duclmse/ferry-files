use super::schema::parent_of;
use super::*;
use crate::store::{FileRecord, MetaFilters};
use std::path::Path;
use std::sync::Arc;

fn make_store() -> SqliteStore {
    SqliteStore::new(Path::new(":memory:")).unwrap()
}

fn sample_record(path: &str) -> FileRecord {
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

#[test]
fn upsert_and_get_by_path() {
    let store = make_store();
    let rec = sample_record("/tmp/foo.rs");
    store.upsert_file(&rec).unwrap();
    let got = store.get_by_path("/tmp/foo.rs").unwrap().unwrap();
    assert_eq!(got.path, "/tmp/foo.rs");
    assert_eq!(got.size_bytes, 1024);
}

#[test]
fn remove_file_returns_none() {
    let store = make_store();
    store.upsert_file(&sample_record("/tmp/foo.rs")).unwrap();
    store.remove_file("/tmp/foo.rs").unwrap();
    assert!(store.get_by_path("/tmp/foo.rs").unwrap().is_none());
}

#[test]
fn remove_directory_removes_children() {
    let store = make_store();
    store.upsert_file(&sample_record("/proj/src/a.rs")).unwrap();
    store.upsert_file(&sample_record("/proj/src/b.rs")).unwrap();
    store
        .upsert_file(&sample_record("/proj/README.md"))
        .unwrap();
    let removed = store.remove_directory("/proj/src").unwrap();
    assert_eq!(removed, 2);
    assert!(store.get_by_path("/proj/src/a.rs").unwrap().is_none());
    assert!(store.get_by_path("/proj/README.md").unwrap().is_some());
}

#[test]
fn search_name_with_extension_filter() {
    let store = make_store();
    store.upsert_file(&sample_record("/a/foo.rs")).unwrap();
    let mut rec2 = sample_record("/a/bar.toml");
    rec2.extension = Some("toml".into());
    store.upsert_file(&rec2).unwrap();

    let filters = MetaFilters {
        extensions: Some(vec!["rs".into()]),
        ..Default::default()
    };
    let results = store.search_name("%", &filters, 100).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].path, "/a/foo.rs");
}

#[test]
fn search_meta_size_range() {
    let store = make_store();
    let mut small = sample_record("/a/small.rs");
    small.size_bytes = 100;
    let mut large = sample_record("/a/large.rs");
    large.size_bytes = 5000;
    store.upsert_file(&small).unwrap();
    store.upsert_file(&large).unwrap();

    let filters = MetaFilters {
        min_size: Some(1000),
        ..Default::default()
    };
    let results = store.search_meta(&filters, 100).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].path, "/a/large.rs");
}

#[test]
fn paths_under_returns_correct_subset() {
    let store = make_store();
    store.upsert_file(&sample_record("/root/a/b.rs")).unwrap();
    store.upsert_file(&sample_record("/root/a/c.rs")).unwrap();
    store.upsert_file(&sample_record("/other/x.rs")).unwrap();

    let paths = store.paths_under("/root/a").unwrap();
    assert_eq!(paths.len(), 2);
    assert!(paths.contains(&"/root/a/b.rs".to_owned()));
    assert!(paths.contains(&"/root/a/c.rs".to_owned()));
}

#[test]
fn concurrent_upserts_all_succeed() {
    let db_file = tempfile::NamedTempFile::new().unwrap();
    let store = Arc::new(SqliteStore::new(db_file.path()).unwrap());
    let mut handles = Vec::new();
    for i in 0..10_usize {
        let s = store.clone();
        handles.push(std::thread::spawn(move || {
            let rec = sample_record(&format!("/tmp/file_{i}.rs"));
            s.upsert_file(&rec).unwrap();
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(store.total_files().unwrap(), 10);
}

#[test]
fn has_child_uses_parent_index() {
    let store = make_store();
    store
        .upsert_file(&sample_record("/proj/src/main.rs"))
        .unwrap();
    assert!(store.has_child("/proj/src", "main.rs").unwrap());
    assert!(!store.has_child("/proj/src", "other.rs").unwrap());
    // Grandchild must not appear as direct child of /proj
    assert!(!store.has_child("/proj", "main.rs").unwrap());
}

#[test]
fn upsert_inner_file_and_get() {
    let store = make_store();
    let mut rec = sample_record("/archive.zip!/inner/file.rs");
    rec.archive_path = Some("/archive.zip".into());
    store.upsert_file(&rec).unwrap();
    let got = store
        .get_by_path("/archive.zip!/inner/file.rs")
        .unwrap()
        .unwrap();
    assert_eq!(got.archive_path, Some("/archive.zip".into()));
}

#[test]
fn remove_archive_entries_leaves_real_files() {
    let store = make_store();
    let mut inner1 = sample_record("/a.zip!/x.rs");
    inner1.archive_path = Some("/a.zip".into());
    let mut inner2 = sample_record("/a.zip!/y.rs");
    inner2.archive_path = Some("/a.zip".into());
    let real = sample_record("/a.zip");
    store.upsert_file(&inner1).unwrap();
    store.upsert_file(&inner2).unwrap();
    store.upsert_file(&real).unwrap();

    let removed = store.remove_archive_entries("/a.zip").unwrap();
    assert_eq!(removed, 2);
    assert!(store.get_by_path("/a.zip").unwrap().is_some());
}

#[test]
fn paths_in_archive_returns_correct_paths() {
    let store = make_store();
    let mut inner = sample_record("/b.zip!/file.rs");
    inner.archive_path = Some("/b.zip".into());
    store.upsert_file(&inner).unwrap();
    let paths = store.paths_in_archive("/b.zip").unwrap();
    assert_eq!(paths, vec!["/b.zip!/file.rs".to_owned()]);
}

#[test]
fn total_files_excludes_archive_entries() {
    let store = make_store();
    store.upsert_file(&sample_record("/real.rs")).unwrap();
    let mut inner = sample_record("/z.zip!/inner.rs");
    inner.archive_path = Some("/z.zip".into());
    store.upsert_file(&inner).unwrap();
    assert_eq!(store.total_files().unwrap(), 1);
}

#[test]
fn total_archive_entries_counts_only_inner() {
    let store = make_store();
    store.upsert_file(&sample_record("/real.rs")).unwrap();
    let mut inner = sample_record("/z.zip!/inner.rs");
    inner.archive_path = Some("/z.zip".into());
    store.upsert_file(&inner).unwrap();
    assert_eq!(store.total_archive_entries().unwrap(), 1);
}

#[test]
fn crawl_generation_begin_increments() {
    let store = make_store();
    let g1 = store.begin_crawl_generation().unwrap();
    let g2 = store.begin_crawl_generation().unwrap();
    assert!(
        g2 > g1,
        "each call must return a strictly larger generation"
    );
}

#[test]
fn schema_includes_parent_column() {
    let store = make_store();
    // If parent column is missing this upsert+query would fail to store parent
    store.upsert_file(&sample_record("/a/b/c.rs")).unwrap();
    // Verify parent is stored by using has_child
    assert!(store.has_child("/a/b", "c.rs").unwrap());
}

#[test]
fn parent_of_archive_entry() {
    assert_eq!(parent_of("/a/arc.zip!/src/main.rs"), "/a/arc.zip!/src");
    assert_eq!(parent_of("/a/b/c.rs"), "/a/b");
    assert_eq!(parent_of("/root.rs"), "");
}

#[test]
fn name_search_partitioned_matches_unpartitioned_semantics() {
    use crate::store::SqlCondition;

    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::new(&dir.path().join("index.db")).unwrap();
    let gen = store.begin_crawl_generation().unwrap();

    // Enough rows for several sampled partitions; every 997th name carries the
    // needle, and leading letters spread the hits across all name ranges.
    let batch: Vec<(FileRecord, String)> = (0..40_000)
        .map(|i| {
            let lead = (b'a' + (i % 26) as u8) as char;
            let name = if i % 997 == 0 {
                format!("{lead}_needle_{i}.rs")
            } else {
                format!("{lead}_file_{i}.txt")
            };
            let mut r = sample_record(&format!("/root/d{}/{name}", i % 50));
            r.extension = Some(if name.ends_with(".rs") { "rs" } else { "txt" }.into());
            (r, "/root".to_owned())
        })
        .collect();
    store.batch_upsert_crawl(&batch, gen).unwrap();

    let expected: Vec<String> = {
        let mut v: Vec<String> = batch
            .iter()
            .map(|(r, _)| r.name.clone())
            .filter(|n| n.contains("needle"))
            .collect();
        v.sort();
        v
    };

    let like = SqlCondition::NameLike("%NEEDLE%".into());
    let mut got: Vec<String> = store
        .search_conditions(std::slice::from_ref(&like), 1_000)
        .unwrap()
        .into_iter()
        .map(|r| r.name)
        .collect();
    got.sort();
    assert_eq!(
        got, expected,
        "LIKE is case-insensitive and every partition contributes"
    );

    let limited = store
        .search_conditions(std::slice::from_ref(&like), 7)
        .unwrap();
    assert_eq!(limited.len(), 7);

    let with_ext = store
        .search_conditions(
            &[like.clone(), SqlCondition::ExtIn(vec!["txt".into()])],
            1_000,
        )
        .unwrap();
    assert!(with_ext.is_empty(), "non-name conditions still apply");

    // Run twice so the second call goes through the cached bounds.
    let again = store.search_conditions(&[like], 1_000).unwrap();
    assert_eq!(again.len(), expected.len());
}

#[test]
fn clustered_name_matches_stop_the_other_ranges_cleanly() {
    use crate::store::SqlCondition;

    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::new(&dir.path().join("index.db")).unwrap();
    let gen = store.begin_crawl_generation().unwrap();
    // Hits all sort into one name range, as `%readme%` does in a real tree.
    let batch: Vec<(FileRecord, String)> = (0..60_000)
        .map(|i| {
            let lead = (b'a' + (i % 26) as u8) as char;
            let name = if i % 20 == 0 {
                format!("readme_{i}.md")
            } else {
                format!("{lead}_file_{i}.txt")
            };
            (
                sample_record(&format!("/root/d{}/{name}", i % 50)),
                "/root".to_owned(),
            )
        })
        .collect();
    store.batch_upsert_crawl(&batch, gen).unwrap();

    let like = [SqlCondition::NameLike("%readme%".into())];
    let none = [SqlCondition::NameLike("%zzqx%".into())];
    // Repeated so an interrupt that outlived its range would surface as an
    // error or a short result on a later query over the same pooled connections.
    for _ in 0..20 {
        let few = store.search_conditions(&like, 10).unwrap();
        assert_eq!(few.len(), 10);
        assert!(few.iter().all(|r| r.name.starts_with("readme_")));
        assert_eq!(store.search_conditions(&like, 10_000).unwrap().len(), 3_000);
        assert!(store.search_conditions(&none, 10).unwrap().is_empty());
    }
}

#[test]
fn glob_nocase_matches_any_case_and_glob_does_not() {
    use crate::store::SqlCondition;

    let store = make_store();
    let gen = store.begin_crawl_generation().unwrap();
    let batch: Vec<(FileRecord, String)> = ["/r/FOO.PDF", "/r/bar.pdf", "/r/baz.txt"]
        .iter()
        .map(|p| (sample_record(p), "/r".to_owned()))
        .collect();
    store.batch_upsert_crawl(&batch, gen).unwrap();

    let names = |cond: SqlCondition| {
        let mut v: Vec<String> = store
            .search_conditions(&[cond], 100)
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        v.sort();
        v
    };
    assert_eq!(
        names(SqlCondition::NameGlobNoCase("*.pdf".into())),
        ["FOO.PDF", "bar.pdf"]
    );
    assert_eq!(
        names(SqlCondition::NameGlobNoCase("foo*".into())),
        ["FOO.PDF"]
    );
    assert_eq!(names(SqlCondition::NameGlob("*.pdf".into())), ["bar.pdf"]);
}
