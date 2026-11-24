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
    let mut folder = sample_record("/d");
    folder.is_dir = true;
    store.upsert_file(&folder).unwrap();
    assert_eq!(store.entry_counts().unwrap(), (1, 1));
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
fn parent_of_splits_on_either_separator() {
    assert_eq!(parent_of(r"C:\a\b.txt"), r"C:\a");
    assert_eq!(
        parent_of(r"C:/Windows/System32\x.dll"),
        "C:/Windows/System32"
    );
    assert_eq!(parent_of(r"C:\a\arc.zip!/src/c.rs"), r"C:\a\arc.zip!/src");
}

#[test]
fn backslash_parents_written_before_the_fix_are_repaired_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("index.db");
    {
        let store = SqliteStore::new(&db).unwrap();
        for p in [r"C:\w\a.txt", "/u/b.txt"] {
            store.upsert_file(&sample_record(p)).unwrap();
        }
        let conn = store.get_conn().unwrap();
        // What the old parent_of stored for a Windows path.
        conn.execute(
            "UPDATE files SET parent = '' WHERE path = ?",
            [r"C:\w\a.txt"],
        )
        .unwrap();
        conn.execute(
            "DELETE FROM index_meta WHERE key = 'parent_splits_backslash'",
            [],
        )
        .unwrap();
    }
    let store = SqliteStore::new(&db).unwrap();
    let parent = |p: &str| -> String {
        store
            .get_conn()
            .unwrap()
            .query_row("SELECT parent FROM files WHERE path = ?", [p], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(parent(r"C:\w\a.txt"), r"C:\w");
    assert_eq!(parent("/u/b.txt"), "/u");
    assert_eq!(store.children_of(r"C:\w").unwrap(), [r"C:\w\a.txt"]);
    assert_eq!(
        store
            .get_meta("parent_splits_backslash")
            .unwrap()
            .as_deref(),
        Some("1")
    );
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

#[test]
fn sorted_search_orders_the_same_way_as_the_memory_store() {
    use crate::store::{Sort, SortKey, SqlCondition};

    let store = make_store();
    let gen = store.begin_crawl_generation().unwrap();
    let batch: Vec<(FileRecord, String)> = [("/r/b.rs", 5), ("/r/A.rs", 9), ("/r/D.rs", 1)]
        .iter()
        .map(|(p, s)| {
            let mut r = sample_record(p);
            r.size_bytes = *s;
            (r, "/r".to_owned())
        })
        .collect();
    store.batch_upsert_crawl(&batch, gen).unwrap();

    let names = |key, descending, limit| -> Vec<String> {
        store
            .search_sorted(
                &[SqlCondition::NameLike("%.rs".into())],
                Sort { key, descending },
                limit,
            )
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect()
    };
    assert_eq!(names(SortKey::Name, false, 10), ["A.rs", "b.rs", "D.rs"]);
    assert_eq!(names(SortKey::Name, true, 1), ["D.rs"]);
    assert_eq!(names(SortKey::Size, true, 2), ["A.rs", "b.rs"]);
}

#[test]
fn memory_and_sqlite_agree_on_escaped_like_patterns() {
    use crate::query::glob::like_escape;
    use crate::store::{memory_store::MemoryStore, SqlCondition};

    let names = [
        "my_file.txt",
        "myXfile.txt",
        "100%.txt",
        "100 percent.txt",
        r"back\slash.txt",
        "MY_FILE.TXT",
        "_lead",
        "%lead",
        "plain",
    ];
    let store = make_store();
    let memory = MemoryStore::new(std::path::Path::new(":memory-test:")).unwrap();
    let gen = store.begin_crawl_generation().unwrap();
    let batch: Vec<(FileRecord, String)> = names
        .iter()
        .map(|n| (sample_record(&format!("/r/{n}")), "/r".to_owned()))
        .collect();
    for (r, _) in &batch {
        memory.upsert_file(r);
    }
    store.batch_upsert_crawl(&batch, gen).unwrap();

    let sorted = |mut v: Vec<String>| {
        v.sort();
        v
    };
    let check = |pattern: String, want: &[&str]| {
        let cond = [SqlCondition::NameLike(pattern.clone())];
        let by = |rows: Vec<FileRecord>| sorted(rows.into_iter().map(|r| r.name).collect());
        let sql = by(store.search_conditions(&cond, 100).unwrap());
        let mem = by(memory.search_conditions(&cond, 100).unwrap());
        assert_eq!(sql, mem, "stores disagree on {pattern:?}");
        assert_eq!(
            sql,
            sorted(want.iter().map(|s| s.to_string()).collect()),
            "{pattern:?}"
        );
    };
    let contains = |s: &str| format!("%{}%", like_escape(s));
    check(contains("my_file"), &["my_file.txt", "MY_FILE.TXT"]);
    check(contains("100%"), &["100%.txt"]);
    check(contains(r"k\s"), &[r"back\slash.txt"]);
    check(like_escape("_lead"), &["_lead"]);
    check(format!("{}%", like_escape("%l")), &["%lead"]);
    // Unescaped wildcards still work, including beside an escaped one.
    check(
        "my_file%".into(),
        &["my_file.txt", "myXfile.txt", "MY_FILE.TXT"],
    );
    check(r"my\_f%.txt".into(), &["my_file.txt", "MY_FILE.TXT"]);

    let check_path = |pattern: String, want: &[&str]| {
        let cond = [SqlCondition::PathLike(pattern.clone())];
        let by = |rows: Vec<FileRecord>| sorted(rows.into_iter().map(|r| r.name).collect());
        let sql = by(store.search_conditions(&cond, 100).unwrap());
        let mem = by(memory.search_conditions(&cond, 100).unwrap());
        assert_eq!(sql, mem, "stores disagree on path {pattern:?}");
        assert_eq!(
            sql,
            sorted(want.iter().map(|s| s.to_string()).collect()),
            "path {pattern:?}"
        );
    };
    use crate::query::glob::glob_to_like;
    check_path(
        glob_to_like("*.txt"),
        &[
            "my_file.txt",
            "myXfile.txt",
            "100%.txt",
            "100 percent.txt",
            r"back\slash.txt",
            "MY_FILE.TXT",
        ],
    );
    check_path(glob_to_like("/r/?lead"), &["_lead", "%lead"]);
    check_path(glob_to_like("/r/_*"), &["_lead"]);
    check_path(glob_to_like("*lead*"), &["_lead", "%lead"]);
    check_path(glob_to_like("lead"), &[]);
}

#[test]
fn extension_glob_finds_folders_and_dot_names_the_column_lacks() {
    use crate::store::SqlCondition;

    let dir = tempfile::tempdir().unwrap();
    let with_index = SqliteStore::new(&dir.path().join("a.db")).unwrap();
    let without = SqliteStore::with_indexes(&dir.path().join("b.db"), &[]).unwrap();
    for store in [&with_index, &without] {
        let gen = store.begin_crawl_generation().unwrap();
        let mut batch: Vec<(FileRecord, String)> = (0..50)
            .map(|i| {
                let mut r = sample_record(&format!("/r/doc{i}.PDF"));
                r.extension = Some("pdf".into());
                (r, "/r".to_owned())
            })
            .collect();
        let mut folder = sample_record("/r/books.pdf");
        folder.is_dir = true;
        folder.extension = None;
        let mut dot = sample_record("/r/.pdf");
        dot.extension = None;
        let mut other = sample_record("/r/notes.txt");
        other.extension = Some("txt".into());
        batch.extend([folder, dot, other].map(|r| (r, "/r".to_owned())));
        store.batch_upsert_crawl(&batch, gen).unwrap();

        // What the planner emits for `*.pdf`.
        let glob = [
            SqlCondition::NameGlobNoCase("*.pdf".into()),
            SqlCondition::ExtOrNone("pdf".into()),
        ];
        let page = store.search_conditions(&glob, 10).unwrap();
        assert_eq!(page.len(), 10);
        assert!(page.iter().all(|r| r.name.to_lowercase().ends_with(".pdf")));

        let mut all: Vec<String> = store
            .search_conditions(&glob, 100)
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        all.sort();
        assert_eq!(all.len(), 52);
        assert_eq!(all[0], ".pdf");
        assert!(all.contains(&"books.pdf".to_owned()));
        assert!(!all.contains(&"notes.txt".to_owned()));

        let folders = store
            .search_conditions(&[glob[0].clone(), SqlCondition::IsDir(true)], 1)
            .unwrap();
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].name, "books.pdf");

        let sort = crate::store::Sort {
            key: crate::store::SortKey::Name,
            descending: false,
        };
        let first = store.search_sorted(&glob, sort, 3).unwrap();
        let names: Vec<&str> = first.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            [".pdf", "books.pdf", "doc0.PDF"],
            "sorted over all matches"
        );
    }
}

#[test]
fn windows_paths_have_subtrees_in_both_stores() {
    use crate::store::{memory_store::MemoryStore, SqlCondition};

    let paths = [
        r"C:\w",
        r"C:\w\a.txt",
        r"C:\w\sub",
        r"C:\w\sub\b.txt",
        r"C:\wx\c.txt",
        r"C:\w\arc.zip!/in/d.txt",
    ];
    let sqlite = make_store();
    let memory = MemoryStore::new(std::path::Path::new(":memory-test:")).unwrap();
    for p in paths {
        let mut r = sample_record(p);
        r.is_dir = !p.ends_with(".txt");
        sqlite.upsert_file(&r).unwrap();
        memory.upsert_file(&r);
    }
    let sorted = |mut v: Vec<String>| {
        v.sort();
        v
    };
    let expected_under = sorted(
        [
            r"C:\w\a.txt",
            r"C:\w\arc.zip!/in/d.txt",
            r"C:\w\sub",
            r"C:\w\sub\b.txt",
        ]
        .map(String::from)
        .to_vec(),
    );
    assert_eq!(sorted(sqlite.paths_under(r"C:\w").unwrap()), expected_under);
    assert_eq!(sorted(memory.paths_under(r"C:\w").unwrap()), expected_under);
    assert_eq!(
        sqlite.children_of(r"C:\w").unwrap().len(),
        2,
        "a.txt and sub"
    );

    let names = |rows: Vec<FileRecord>| sorted(rows.into_iter().map(|r| r.path).collect());
    let want = vec![r"C:\w\a.txt".to_owned(), r"C:\w\sub".to_owned()];
    // What parent:C:\w plans to, in any case, as Windows paths compare.
    for dir in [r"C:\w", r"c:\W"] {
        let direct = [SqlCondition::ParentIs(dir.into())];
        assert_eq!(names(sqlite.search_conditions(&direct, 100).unwrap()), want);
        assert_eq!(names(memory.search_conditions(&direct, 100).unwrap()), want);
    }
    assert!(sqlite.children_of(r"c:\W").unwrap().is_empty(), "exact");

    assert_eq!(sqlite.remove_directory(r"C:\w\sub").unwrap(), 1);
    assert_eq!(memory.remove_directory(r"C:\w\sub"), 1);
    for store_has in [
        sqlite.get_by_path(r"C:\w\sub\b.txt").unwrap().is_some(),
        memory.get_by_path(r"C:\w\sub\b.txt").unwrap().is_some(),
    ] {
        assert!(!store_has);
    }
    assert!(
        sqlite.get_by_path(r"C:\wx\c.txt").unwrap().is_some(),
        "a sibling prefix is not a parent"
    );
}

#[test]
fn recrawl_rewrites_changed_rows_and_keeps_unchanged_ones() {
    let store = make_store();
    let rows = |mtime: i64| -> Vec<(FileRecord, String)> {
        ["/r/a.txt", "/r/b.txt", "/r/c.txt"]
            .iter()
            .map(|p| {
                let mut r = sample_record(p);
                r.mtime = if *p == "/r/b.txt" { mtime } else { 1 };
                (r, "/r".to_owned())
            })
            .collect()
    };
    let gen1 = store.begin_crawl_generation().unwrap();
    store.batch_upsert_crawl(&rows(1), gen1).unwrap();

    // b.txt changed and c.txt is gone.
    let gen2 = store.begin_crawl_generation().unwrap();
    let mut second = rows(99);
    second.pop();
    store.batch_upsert_crawl(&second, gen2).unwrap();
    assert_eq!(store.remove_stale_for_root("/r", gen2).unwrap(), 1);

    assert_eq!(store.get_by_path("/r/a.txt").unwrap().unwrap().mtime, 1);
    assert_eq!(store.get_by_path("/r/b.txt").unwrap().unwrap().mtime, 99);
    assert!(store.get_by_path("/r/c.txt").unwrap().is_none());
}

#[test]
fn a_sweep_covers_rows_crawled_under_a_watch_path_it_took_in() {
    let store = make_store();
    let row = |p: &str, root: &str| (sample_record(p), root.to_owned());
    let gen1 = store.begin_crawl_generation().unwrap();
    store
        .batch_upsert_crawl(
            &[
                row("/r/c", "/r/c"),
                row("/r/c/kept.txt", "/r/c"),
                row("/r/c/gone.txt", "/r/c"),
                row("/rx/beside.txt", "/rx"),
            ],
            gen1,
        )
        .unwrap();
    // /r now covers /r/c; gone.txt is no longer there.
    let gen2 = store.begin_crawl_generation().unwrap();
    store
        .batch_upsert_crawl(
            &[
                row("/r", "/r"),
                row("/r/c", "/r"),
                row("/r/c/kept.txt", "/r"),
            ],
            gen2,
        )
        .unwrap();
    assert_eq!(store.remove_stale_for_root("/r", gen2).unwrap(), 1);
    assert!(store.get_by_path("/r/c/gone.txt").unwrap().is_none());
    assert!(store.get_by_path("/r/c/kept.txt").unwrap().is_some());
    assert!(store.get_by_path("/rx/beside.txt").unwrap().is_some());
}

fn schema_version(db: &Path) -> i64 {
    rusqlite::Connection::open(db)
        .unwrap()
        .query_row("PRAGMA schema_version", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn reopening_a_store_leaves_its_schema_alone() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("index.db");
    drop(SqliteStore::new(&db).unwrap());
    let before = schema_version(&db);
    drop(SqliteStore::new(&db).unwrap());
    // Any index dropped and rebuilt on open would bump it.
    assert_eq!(schema_version(&db), before);
}

#[test]
fn an_old_parent_index_is_rebuilt_on_the_parent_column_ignoring_case() {
    for old in ["files(path)", "files(parent)"] {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        drop(SqliteStore::new(&db).unwrap());
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute_batch(&format!(
                "DROP INDEX idx_files_parent; CREATE INDEX idx_files_parent ON {old};"
            ))
            .unwrap();
        drop(SqliteStore::new(&db).unwrap());
        let cols: Vec<(String, String)> = rusqlite::Connection::open(&db)
            .unwrap()
            .prepare("SELECT name, coll FROM pragma_index_xinfo('idx_files_parent') WHERE key = 1")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(cols, [("parent".to_owned(), "NOCASE".to_owned())], "{old}");
    }
}

#[test]
fn parent_lookups_use_the_parent_index() {
    let dir = tempfile::tempdir().unwrap();
    let s = SqliteStore::new(&dir.path().join("index.db")).unwrap();
    let conn = s.get_conn().unwrap();
    let (cond, _) = crate::store::SqlCondition::ParentIs("/a".into()).to_sql();
    for sql in [
        format!("SELECT id FROM files WHERE {cond}"),
        "SELECT path FROM files WHERE parent = ?1 COLLATE NOCASE AND parent = ?1".to_owned(),
    ] {
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map(["/a"], |r| r.get(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            plan.iter().any(|p| p.contains("idx_files_parent")),
            "{sql}: {plan:?}"
        );
    }
}

#[test]
fn an_extension_glob_reads_indexes_not_the_table() {
    use crate::store::SqlCondition;

    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::new(&dir.path().join("a.db")).unwrap();
    let conds = [
        SqlCondition::NameGlobNoCase("*.rs".into()),
        SqlCondition::ExtOrNone("rs".into()),
        SqlCondition::IsDir(false),
    ];
    let (sql, params) = super::search::extension_query(&conds, 10).unwrap();
    let conn = store.get_conn().unwrap();
    let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    let plan: Vec<String> = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |r| r.get(3))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(plan.iter().all(|step| step != "SCAN files"), "{plan:#?}");
    assert!(
        plan.iter().any(|s| s.contains("idx_files_extension")),
        "{plan:#?}"
    );
    assert!(
        plan.iter().any(|s| s.contains("idx_files_noext_name")),
        "{plan:#?}"
    );
}
