use super::matching::{glob_match, glob_nocase_match, like_match};
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
    for r in [
        dir("/w"),
        dir("/w/src"),
        sample("/w/src/a.rs"),
        sample("/w/src/b.rs"),
    ] {
        store.upsert_file(&r);
    }
    assert!(store.remove_file("/w/src"));
    assert!(store.get_by_path("/w/src").unwrap().is_none());
    assert_same(
        store.get_by_path("/w/src/a.rs").unwrap(),
        &sample("/w/src/a.rs"),
    );
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
    for r in [
        dir("/a"),
        sample("/a/x.rs"),
        dir("/ab"),
        sample("/ab/y.rs"),
        sample("/a/d/z.rs"),
    ] {
        store.upsert_file(&r);
    }
    assert_eq!(store.paths_under("/a").unwrap().len(), 2);
    assert_eq!(store.children_of("/a").unwrap(), ["/a/x.rs"]);
    assert!(store.has_child("/ab", "y%").unwrap());
    assert!(!store.has_child("/a", "z%").unwrap());
    assert_eq!(store.children_of("").unwrap().len(), 2);
    let depth = [SqlCondition::DepthOp(CmpOp::Eq, 3)];
    assert_eq!(
        paths(store.search_conditions(&depth, 10).unwrap()),
        ["/a/d/z.rs"]
    );
    let like = [SqlCondition::PathLike("%b/y%".into())];
    assert_eq!(
        paths(store.search_conditions(&like, 10).unwrap()),
        ["/ab/y.rs"]
    );
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
            entry(
                (child.len() + parent.len()) as u64,
                member,
                11,
                child.len() as u32,
                0,
            ),
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

// The matchers these replaced, kept as the definition of correct behaviour.
fn reference_like(text: &[u8], pattern: &[u8]) -> bool {
    match pattern {
        [] => text.is_empty(),
        [b'%', rest @ ..] => (0..=text.len()).any(|i| reference_like(&text[i..], rest)),
        [b'_', rest @ ..] => !text.is_empty() && reference_like(&text[1..], rest),
        [c, rest @ ..] => {
            !text.is_empty() && text[0].eq_ignore_ascii_case(c) && reference_like(&text[1..], rest)
        }
    }
}

fn reference_glob(text: &[u8], pattern: &[u8]) -> bool {
    match pattern {
        [] => text.is_empty(),
        [b'*', rest @ ..] => (0..=text.len()).any(|i| reference_glob(&text[i..], rest)),
        [b'?', rest @ ..] => !text.is_empty() && reference_glob(&text[1..], rest),
        [c, rest @ ..] => !text.is_empty() && text[0] == *c && reference_glob(&text[1..], rest),
    }
}

/// Every string of up to `max` bytes over `alphabet`.
fn all_strings(alphabet: &[u8], max: usize) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut layer = out.clone();
    for _ in 0..max {
        layer = layer
            .iter()
            .flat_map(|s| {
                alphabet.iter().map(move |&c| {
                    let mut next = s.clone();
                    next.push(c as char);
                    next
                })
            })
            .collect();
        out.extend(layer.iter().cloned());
    }
    out
}

#[test]
fn like_agrees_with_the_reference_matcher() {
    let texts = all_strings(b"aB%_", 4);
    for pattern in all_strings(b"aAb%_", 4) {
        for text in &texts {
            assert_eq!(
                like_match(text, &pattern),
                reference_like(text.as_bytes(), pattern.as_bytes()),
                "text {text:?} pattern {pattern:?}"
            );
        }
    }
}

#[test]
fn glob_agrees_with_the_reference_matcher() {
    let texts = all_strings(b"aB*?", 4);
    for pattern in all_strings(b"aB*?", 5) {
        for text in &texts {
            assert_eq!(
                glob_match(text, &pattern),
                reference_glob(text.as_bytes(), pattern.as_bytes()),
                "text {text:?} pattern {pattern:?}"
            );
        }
    }
}

#[test]
fn glob_nocase_agrees_with_lowercasing_both_sides() {
    let texts = all_strings(b"aB*?", 4);
    for pattern in all_strings(b"aB*?", 5) {
        for text in &texts {
            assert_eq!(
                glob_nocase_match(text, &pattern),
                reference_glob(
                    text.to_ascii_lowercase().as_bytes(),
                    pattern.to_ascii_lowercase().as_bytes(),
                ),
                "text {text:?} pattern {pattern:?}"
            );
        }
    }
}

#[test]
fn many_wildcards_do_not_blow_up() {
    let text = "a".repeat(4096);
    assert!(!glob_match(&text, "*a*a*a*a*a*a*a*a*a*b"));
    assert!(!like_match(&text, "%a%a%a%a%a%a%a%a%a%b"));
}

#[test]
fn parallel_scan_returns_the_first_matches_in_entry_order() {
    let store = MemoryStore::new(tempfile::tempdir().unwrap().path()).unwrap();
    let total = 200_000;
    let expected: Vec<String> = (0..total)
        .step_by(7)
        .map(|i| format!("/s/needle{i}.txt"))
        .collect();
    for i in 0..total {
        let word = if i % 7 == 0 { "needle" } else { "other" };
        store.upsert_file(&sample(&format!("/s/{word}{i}.txt")));
    }
    let conds = [SqlCondition::NameLike("%needle%".into())];
    for limit in [0, 1, 100, 20_000, usize::MAX] {
        let paths: Vec<String> = store
            .search_conditions(&conds, limit)
            .unwrap()
            .into_iter()
            .map(|r| r.path)
            .collect();
        assert_eq!(
            paths,
            expected[..limit.min(expected.len())],
            "limit {limit}"
        );
    }
}

// ---------------------------------------------------------------------------
// Per-directory path matching, sort indexes, dupe keys
// ---------------------------------------------------------------------------

fn names(records: Vec<FileRecord>) -> Vec<String> {
    records.into_iter().map(|r| r.name).collect()
}

#[test]
fn path_search_per_directory_agrees_with_matching_the_whole_path() {
    let store = make_store();
    let mut paths = vec!["/Ab".to_owned(), "/Ab/cD".to_owned(), "/x".to_owned()];
    for dir in ["/Ab", "/Ab/cD", "/x"] {
        for file in ["e.txt", "Cd.rs", "ab"] {
            paths.push(format!("{dir}/{file}"));
        }
    }
    // Parents first, so these entries extend their directories.
    for p in &paths {
        store.upsert_file(&sample(p));
    }
    // No parent entry, so its tail is the whole path.
    store.upsert_file(&sample("/orphan/dir/Ab.txt"));
    paths.push("/orphan/dir/Ab.txt".to_owned());

    for pattern in [
        "%b/c%",
        "%d/e%",
        "%ab%",
        "%CD%",
        "%/%",
        "%.txt%",
        "%x/a%",
        "/a%",
        "/ab/cd/%",
        "/Ab/cD/e%",
        "/x%",
        "/%",
        "/ab/cd/e.txt",
        "%.rs",
        "/_b%",
        "%orphan%",
    ] {
        let mut want: Vec<String> = paths
            .iter()
            .filter(|p| like_match(p, pattern))
            .cloned()
            .collect();
        let mut got: Vec<String> = store
            .search_conditions(&[SqlCondition::PathLike(pattern.into())], 1000)
            .unwrap()
            .into_iter()
            .map(|r| r.path)
            .collect();
        want.sort();
        got.sort();
        assert_eq!(got, want, "pattern {pattern:?}");
    }
}

#[test]
fn two_path_conditions_keep_separate_verdicts() {
    let store = make_store();
    store.upsert_file(&sample("/keep"));
    store.upsert_file(&sample("/keep/a.txt"));
    let conds = [
        SqlCondition::PathLike("%keep%".into()),
        SqlCondition::Not(vec![SqlCondition::PathLike("%zzz%".into())]),
    ];
    let got = store.search_conditions(&conds, 10).unwrap();
    assert_eq!(got.len(), 2);
}

fn sized(path: &str, size: u64) -> FileRecord {
    FileRecord {
        size_bytes: size,
        mtime: 1000 - size as i64,
        ..sample(path)
    }
}

const BY_NAME: Sort = Sort {
    key: SortKey::Name,
    descending: false,
};
const BY_SIZE_DESC: Sort = Sort {
    key: SortKey::Size,
    descending: true,
};

#[test]
fn sorted_search_orders_and_filters() {
    let store = make_store();
    for (p, s) in [("/b.rs", 5), ("/A.rs", 9), ("/c.txt", 100), ("/D.rs", 1)] {
        store.upsert_file(&sized(p, s));
    }
    let rs = [SqlCondition::NameLike("%.rs".into())];
    assert_eq!(
        names(store.search_sorted(&rs, BY_NAME, 10).unwrap()),
        ["A.rs", "b.rs", "D.rs"],
        "names sort ignoring case"
    );
    assert_eq!(
        names(store.search_sorted(&rs, BY_SIZE_DESC, 2).unwrap()),
        ["A.rs", "b.rs"]
    );
    let oldest_first = Sort {
        key: SortKey::DateModified,
        descending: false,
    };
    assert_eq!(
        names(store.search_sorted(&[], oldest_first, 1).unwrap()),
        ["c.txt"]
    );
}

#[test]
fn sort_index_follows_changes_after_it_is_built() {
    let store = make_store();
    for (p, s) in [("/a", 1), ("/b", 2), ("/c", 3)] {
        store.upsert_file(&sized(p, s));
    }
    // Enough matches that the query uses the order rather than sorting them.
    for i in 0..5000 {
        store.upsert_file(&sized(&format!("/z{i:04}"), 0));
    }
    let all = |sort| {
        let mut got = names(store.search_sorted(&[], sort, 4).unwrap());
        got.retain(|n| !n.starts_with('z'));
        got
    };
    assert_eq!(all(BY_SIZE_DESC), ["c", "b", "a"]);

    store.upsert_file(&sized("/a", 50)); // changed
    store.upsert_file(&sized("/d", 2)); // added, ties with /b
    store.remove_file("/c");
    assert_eq!(all(BY_SIZE_DESC), ["a", "d", "b"]);
    store.upsert_file(&sized("/c", 25)); // revived from a ghost
    assert_eq!(all(BY_SIZE_DESC), ["a", "c", "d", "b"]);
    assert_eq!(all(BY_NAME), ["a", "b", "c", "d"]);
}

#[test]
fn sort_index_survives_compaction_and_large_change_sets() {
    let store = make_store();
    let count = 20_000u64;
    for i in 0..count {
        store.upsert_file(&sized(&format!("/f{i:05}"), i));
    }
    let top = |n| names(store.search_sorted(&[], BY_SIZE_DESC, n).unwrap());
    assert_eq!(top(1), ["f19999"]);
    // Enough removals to compact, which renumbers every entry.
    for i in (0..count).rev().take(10_000) {
        store.remove_file(&format!("/f{i:05}"));
    }
    assert_eq!(top(2), ["f09999", "f09998"]);
}

#[test]
fn dupes_group_numerically_and_keep_entry_order() {
    let store = make_store();
    for (p, s) in [("/a", 10), ("/b", 9), ("/c", 10), ("/d", 9), ("/e", 3)] {
        store.upsert_file(&sized(p, s));
    }
    assert_eq!(
        names(store.find_dupes(DupeKind::Size, &[], 10).unwrap()),
        ["b", "d", "a", "c"]
    );
    assert_eq!(
        names(store.find_dupes(DupeKind::Size, &[], 3).unwrap()).len(),
        3
    );
}

#[test]
fn dupes_by_name_ignore_unique_names() {
    let store = make_store();
    for p in ["/x/same.rs", "/y/same.rs", "/y/other.rs"] {
        store.upsert_file(&sample(p));
    }
    assert_eq!(
        names(store.find_dupes(DupeKind::Name, &[], 10).unwrap()),
        ["same.rs", "same.rs"]
    );
}

#[test]
fn flush_if_changed_saves_only_after_an_entry_changes() {
    let dir = TempDir::new().unwrap();
    let store = MemoryStore::new(dir.path()).unwrap();
    assert!(!store.flush_if_changed(dir.path()).unwrap(), "nothing yet");

    store.upsert_file(&sample("/a/b.rs"));
    assert!(store.flush_if_changed(dir.path()).unwrap());
    assert!(!store.flush_if_changed(dir.path()).unwrap(), "unchanged");

    store.set_meta("journal_position", "{}");
    assert!(
        !store.flush_if_changed(dir.path()).unwrap(),
        "metadata alone does not justify a save"
    );

    store.remove_file("/a/b.rs");
    assert!(store.flush_if_changed(dir.path()).unwrap());
    let reloaded = MemoryStore::new(dir.path()).unwrap();
    assert!(reloaded.get_by_path("/a/b.rs").unwrap().is_none());
    assert_eq!(reloaded.get_meta("journal_position").as_deref(), Some("{}"));
}

#[test]
fn few_and_many_matches_sort_identically() {
    let store = make_store();
    for i in 0..6000u64 {
        store.upsert_file(&sized(&format!("/f{i:04}"), i % 7));
    }
    let top = |conds: &[SqlCondition], sort| names(store.search_sorted(conds, sort, 6000).unwrap());
    // Everything matches, so this comes from the order.
    let via_order = top(&[], BY_SIZE_DESC);
    // Under the bound, so these are sorted directly; ties must agree.
    let few = [SqlCondition::NameLike("f00%".into())];
    let direct = top(&few, BY_SIZE_DESC);
    assert_eq!(direct.len(), 100);
    let filtered: Vec<String> = via_order
        .into_iter()
        .filter(|n| n.starts_with("f00"))
        .collect();
    assert_eq!(direct, filtered);
    assert_eq!(
        names(store.search_sorted(&few, BY_NAME, 2).unwrap()),
        ["f0000", "f0001"]
    );
}
