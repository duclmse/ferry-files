use super::*;
use crate::config::MacroConfig;
use crate::query::ast::Modifiers;
use crate::query::parser::parse;

fn empty_mods() -> Modifiers {
    Modifiers::default()
}

fn parse_plan(q: &str) -> QueryPlan {
    let macros = MacroConfig::default();
    let expr = parse(q, &macros).unwrap();
    plan(&expr, &empty_mods()).unwrap()
}

#[test]
fn plain_term_produces_name_like() {
    let p = parse_plan("main");
    assert!(p
        .sql_conditions
        .iter()
        .any(|c| matches!(c, SqlCondition::NameLike(_))));
    assert!(matches!(p.strategy, EvalStrategy::SqliteOnly));
}

#[test]
fn wildcard_produces_glob() {
    let p = parse_plan("*.rs");
    assert!(p
        .sql_conditions
        .iter()
        .any(|c| matches!(c, SqlCondition::NameGlobNoCase(_))));
}

#[test]
fn case_modifier_produces_case_sensitive_glob() {
    let p = parse_plan("case: *.rs");
    assert!(p
        .sql_conditions
        .iter()
        .any(|c| matches!(c, SqlCondition::NameGlob(_))));
}

#[test]
fn ext_produces_ext_in() {
    let p = parse_plan("ext:jpg;png");
    assert!(p
        .sql_conditions
        .iter()
        .any(|c| matches!(c, SqlCondition::ExtIn(_))));
}

#[test]
fn size_large_produces_size_range() {
    let p = parse_plan("size:large");
    assert!(p
        .sql_conditions
        .iter()
        .any(|c| matches!(c, SqlCondition::SizeRange(_, _))));
}

#[test]
fn content_sets_tantivy_strategy() {
    let p = parse_plan("content:TODO");
    assert!(p.tantivy_query.is_some());
    assert!(matches!(p.strategy, EvalStrategy::TantivyThenSqlite));
}

#[test]
fn fuzzy_sets_fuzzy_strategy() {
    let p = parse_plan("fuzzy:mian");
    assert!(matches!(p.strategy, EvalStrategy::FuzzyFilter(_)));
}

#[test]
fn dupe_sets_dupe_strategy() {
    let p = parse_plan("dupe:");
    assert!(matches!(p.strategy, EvalStrategy::DupeSearch(_)));
}

#[test]
fn count_sets_limit() {
    let p = parse_plan("count:50");
    assert_eq!(p.limit, Some(50));
}

#[test]
fn archive_entry_only_condition() {
    let p = parse_plan("archive: *.rs");
    assert!(p
        .sql_conditions
        .iter()
        .any(|c| matches!(c, SqlCondition::IsArchiveEntry)));
}

#[test]
fn noarchive_post_filter() {
    let p = parse_plan("noarchive: *.log");
    assert!(p
        .post_filters
        .iter()
        .any(|f| matches!(f, PostFilter::ArchiveExclude)));
}

#[test]
fn archiveof_produces_condition() {
    let p = parse_plan("archiveof:project.zip");
    assert!(p
        .sql_conditions
        .iter()
        .any(|c| matches!(c, SqlCondition::ArchiveOf(_))));
}

#[test]
fn select_strategy_disk_below_threshold() {
    // Disk strategy when threshold = 0 (disabled)
    let p = parse_plan("*.rs");
    assert!(matches!(p.strategy, EvalStrategy::SqliteOnly));
}

#[test]
fn dm_today_produces_mtime_range() {
    let p = parse_plan("dm:today");
    assert!(p
        .sql_conditions
        .iter()
        .any(|c| matches!(c, SqlCondition::MtimeRange(_, _))));
    assert!(matches!(p.strategy, EvalStrategy::SqliteOnly));
}

#[test]
fn size_gt_1mb_produces_size_op() {
    let p = parse_plan("size:>1mb");
    assert!(p
        .sql_conditions
        .iter()
        .any(|c| matches!(c, SqlCondition::SizeOp(CmpOp::Gt, 1_048_576))));
}

// ---------------------------------------------------------------------------
// OR / NOT evaluated end-to-end against both store backends
// ---------------------------------------------------------------------------

mod boolean {
    use super::*;
    use crate::store::{
        memory_store::MemoryStore, sqlite_store::SqliteStore, FileRecord, FileStore,
    };
    use std::sync::Arc;

    fn rec(path: &str, ext: Option<&str>, is_dir: bool) -> FileRecord {
        FileRecord {
            id: None,
            path: path.into(),
            name: path.rsplit('/').next().unwrap_or(path).into(),
            extension: ext.map(Into::into),
            size_bytes: 10,
            mtime: 0,
            ctime: 0,
            is_dir,
            is_hidden: false,
            is_symlink: false,
            attrib_bitmask: 0,
            content_indexed: false,
            tantivy_id: None,
            indexed_at: 0,
            archive_path: None,
        }
    }

    fn fill(store: &FileStore) {
        for r in [
            rec("/p/main.rs", Some("rs"), false),
            rec("/p/lib.rs", Some("rs"), false),
            rec("/p/notes.txt", Some("txt"), false),
            rec("/p/README", None, false),
            rec("/p/src", None, true),
        ] {
            store.upsert_file(&r).unwrap();
        }
    }

    fn run(store: &FileStore, q: &str) -> Vec<String> {
        let p = parse_plan(q);
        let rows = store.search_conditions(&p.sql_conditions, 1000).unwrap();
        let mut names: Vec<String> = apply_post_filters(rows, &p.post_filters, store)
            .into_iter()
            .map(|r| r.name)
            .collect();
        names.sort();
        names
    }

    fn check(store: &FileStore) {
        fill(store);
        let cases: &[(&str, &[&str])] = &[
            ("main.rs | lib.rs", &["lib.rs", "main.rs"]),
            (
                "main.rs | lib.rs | README",
                &["README", "lib.rs", "main.rs"],
            ),
            ("!main.rs", &["README", "lib.rs", "notes.txt", "src"]),
            ("!ext:rs", &["README", "notes.txt", "src"]),
            ("*.rs !lib", &["main.rs"]),
            ("!<main | lib>", &["README", "notes.txt", "src"]),
            (r"main | regex:notes\.txt$", &["main.rs", "notes.txt"]),
            (r"!regex:\.rs$", &["README", "notes.txt", "src"]),
        ];
        for (q, want) in cases {
            assert_eq!(run(store, q), *want, "query {q:?}");
        }
    }

    #[test]
    fn sqlite_backend() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = SqliteStore::new(&dir.path().join("i.db")).unwrap();
        check(&FileStore::Sqlite(Arc::new(s)));
    }

    #[test]
    fn memory_backend() {
        let dir = tempfile::TempDir::new().unwrap();
        check(&FileStore::Memory(Arc::new(
            MemoryStore::new(dir.path()).unwrap(),
        )));
    }

    fn fuzzy(store: &FileStore) {
        fill(store);
        let rank = |term: &'static str| {
            move |names: &[&str]| super::super::postfilter::fuzzy_rank(names, term, 0.7, 10)
        };
        let hits = store
            .fuzzy_search(&[], 1000, &|_| true, &rank("mian"))
            .unwrap();
        assert_eq!(hits.first().map(|r| r.path.as_str()), Some("/p/main.rs"));
        let cond = parse_plan("ext:txt").sql_conditions;
        let hits = store
            .fuzzy_search(&cond, 1000, &|_| true, &rank("notes"))
            .unwrap();
        assert_eq!(
            hits.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["notes.txt"]
        );
        assert!(store
            .fuzzy_search(&cond, 1000, &|_| true, &rank("main"))
            .unwrap()
            .is_empty());
    }

    /// More non-matching names than the cap, before the match both in
    /// insertion order and in name order, which is how SQLite reads them.
    fn fuzzy_past_the_cap(store: &FileStore) {
        for i in 0..300 {
            store
                .upsert_file(&rec(&format!("/p/aaaa_{i}.bin"), Some("bin"), false))
                .unwrap();
        }
        store
            .upsert_file(&rec("/p/main.rs", Some("rs"), false))
            .unwrap();
        let prefilter = super::super::postfilter::FuzzyPrefilter::new("mian", 0.7);
        let hits = store
            .fuzzy_search(
                &[],
                50,
                &|name: &str| prefilter.could_match(name),
                &|names: &[&str]| super::super::postfilter::fuzzy_rank(names, "mian", 0.7, 10),
            )
            .unwrap();
        assert_eq!(
            hits.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["main.rs"]
        );
    }

    #[test]
    fn fuzzy_finds_a_match_past_the_candidate_cap() {
        let dir = tempfile::TempDir::new().unwrap();
        fuzzy_past_the_cap(&FileStore::Sqlite(Arc::new(
            SqliteStore::new(&dir.path().join("i.db")).unwrap(),
        )));
        fuzzy_past_the_cap(&FileStore::Memory(Arc::new(
            MemoryStore::new(dir.path()).unwrap(),
        )));
    }

    #[test]
    fn fuzzy_sqlite_backend() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = SqliteStore::new(&dir.path().join("i.db")).unwrap();
        fuzzy(&FileStore::Sqlite(Arc::new(s)));
    }

    #[test]
    fn fuzzy_memory_backend() {
        let dir = tempfile::TempDir::new().unwrap();
        fuzzy(&FileStore::Memory(Arc::new(
            MemoryStore::new(dir.path()).unwrap(),
        )));
    }

    fn buried_matches(store: &FileStore) {
        // 2000 rejected rows come first, so a fetch of exactly `want` rows
        // would see none of the 10 that survive the filter.
        for i in 0..2000 {
            store
                .upsert_file(&rec(&format!("/b/skip_{i}.rs"), Some("rs"), false))
                .unwrap();
        }
        for i in 0..10 {
            store
                .upsert_file(&rec(&format!("/b/hit_{i}.rs"), Some("rs"), false))
                .unwrap();
        }
        let p = parse_plan("!regex:skip_");
        let got = |want, cap| {
            super::super::execute::fetch_filtered(
                store,
                &p.sql_conditions,
                None,
                &p.post_filters,
                want,
                cap,
            )
            .unwrap()
        };

        assert!(got(5, 0).len() >= 5, "grows until enough rows survive");
        assert_eq!(got(100, 0).len(), 10, "stops when the store runs out");
        assert!(got(5, 500).is_empty(), "cap bounds the rows read");
    }

    #[test]
    fn filtered_fetch_grows_sqlite() {
        let dir = tempfile::TempDir::new().unwrap();
        buried_matches(&FileStore::Sqlite(Arc::new(
            SqliteStore::new(&dir.path().join("i.db")).unwrap(),
        )));
    }

    #[test]
    fn filtered_fetch_grows_memory() {
        let dir = tempfile::TempDir::new().unwrap();
        buried_matches(&FileStore::Memory(Arc::new(
            MemoryStore::new(dir.path()).unwrap(),
        )));
    }

    #[test]
    fn chained_or_is_flattened() {
        let p = parse_plan("a | b | c");
        assert!(matches!(p.sql_conditions.as_slice(), [SqlCondition::Or(b)] if b.len() == 3));
    }

    #[test]
    fn strategy_predicates_rejected_inside_or_and_not() {
        let macros = MacroConfig::default();
        for q in ["content:x | main", "!fuzzy:mian", "dupe: | main"] {
            let expr = parse(q, &macros).unwrap();
            assert!(
                plan(&expr, &empty_mods()).is_err(),
                "{q:?} should be rejected"
            );
        }
    }
}

#[test]
fn sort_is_parsed_with_per_key_default_directions() {
    use crate::store::{Sort, SortKey};
    let cases = [
        ("sort:name foo", SortKey::Name, false),
        ("sort:size foo", SortKey::Size, true),
        ("sort:dm foo", SortKey::DateModified, true),
        ("sort:date-created-asc foo", SortKey::DateCreated, false),
        ("sort:name-descending foo", SortKey::Name, true),
    ];
    for (q, key, descending) in cases {
        assert_eq!(parse_plan(q).sort, Some(Sort { key, descending }), "{q}");
    }
    let macros = MacroConfig::default();
    assert!(parse("sort:bogus foo", &macros).is_err());
}

#[test]
fn like_syntax_in_terms_is_escaped() {
    let has = |q: &str, want: &str| {
        parse_plan(q).sql_conditions.iter().any(|c| match c {
            SqlCondition::NameLike(p) | SqlCondition::PathLike(p) => p == want,
            _ => false,
        })
    };
    assert!(has("my_file", r"%my\_file%"));
    assert!(has("100%", r"%100\%%"));
    assert!(has("path:/a_b", r"%/a\_b%"));
    assert!(has("path: *.rs", "%.rs"));
    assert!(has("path: */a_b/?.rs", r"%/a\_b/_.rs"));
}

#[test]
fn bare_and_attached_modifiers_parse() {
    let conds = |q: &str| format!("{:?}", parse_plan(q).sql_conditions);
    // Documented modifiers used to fall through and become literal search text.
    for q in [
        "nopath: foo",
        "files: foo",
        "folders: foo",
        "noww: foo",
        "nowfn: foo",
        "noregex: foo",
        "ascii: foo",
        "nodiacritics: foo",
        "wildcards: foo",
    ] {
        assert!(!conds(q).contains(':'), "{q} => {}", conds(q));
    }
    // An attached value is the term, not dropped.
    assert_eq!(conds("case:Foo"), conds("case: Foo"));
    assert!(conds("case:Foo").contains("Foo"));
    assert_eq!(conds("wfn:foo"), r#"[NameLike("foo")]"#);
    assert_eq!(conds("nopath: path: foo"), conds("path: foo"));
    // Single letters with a value stay drive letters.
    assert!(conds(r"i:\data").contains("PathLike"));
    assert!(conds(r"w:\data").contains("PathLike"));
    // Function keywords keep their meaning when valued.
    assert!(conds("path:/src").contains("PathLike"));
    assert!(conds("dir:src").contains("Parent"));
}

#[test]
fn nowildcards_matches_star_literally() {
    assert_eq!(
        format!("{:?}", parse_plan("nowildcards: a*b").sql_conditions),
        r#"[NameLike("%a*b%")]"#
    );
}

#[test]
fn spaced_regex_modifier_is_a_regex_search() {
    let p = parse_plan("regex: a.b");
    assert!(p.sql_conditions.is_empty(), "{:?}", p.sql_conditions);
    assert!(
        matches!(&p.post_filters[..], [PostFilter::Regex { re, on_path: false }] if re.as_str().contains("a.b"))
    );
}

#[test]
fn modifier_before_quoted_phrase() {
    let p = parse_plan(r#"case: "Foo Bar""#);
    assert!(
        matches!(&p.post_filters[..], [PostFilter::Literal { text, whole: false, case_sensitive: true, .. }] if text == "Foo Bar")
    );
}

mod modifiers_end_to_end {
    use super::*;
    use crate::store::{
        memory_store::MemoryStore, sqlite_store::SqliteStore, FileRecord, FileStore,
    };
    use std::sync::Arc;

    fn rec(path: &str) -> FileRecord {
        FileRecord {
            id: None,
            path: path.into(),
            name: path.rsplit('/').next().unwrap_or(path).into(),
            // Lowercased, as every writer stores it.
            extension: path.rsplit_once('.').map(|(_, e)| e.to_lowercase()),
            size_bytes: 10,
            mtime: 0,
            ctime: 0,
            is_dir: false,
            is_hidden: false,
            is_symlink: false,
            attrib_bitmask: 0,
            content_indexed: false,
            tantivy_id: None,
            indexed_at: 0,
            archive_path: None,
        }
    }

    fn run(store: &FileStore, q: &str) -> Vec<String> {
        let p = parse_plan(q);
        let rows = store.search_conditions(&p.sql_conditions, 1000).unwrap();
        let mut paths: Vec<String> = apply_post_filters(rows, &p.post_filters, store)
            .into_iter()
            .map(|r| r.path)
            .collect();
        paths.sort();
        paths
    }

    fn check(store: &FileStore) {
        for path in [
            "/Src/Foo.RS",
            "/src/foo.rs",
            "/src/a*b.txt",
            "/src/aXb.txt",
            "/src/a.b",
            "/u/CAFÉ.txt",
            "/u/Café.md",
            "/u/cafe.txt",
            "/u/ΣΟΦΙΑ.txt",
            "/u/x[1].log",
            "/u/x1.log",
            "/Ünï/ö.txt",
        ] {
            store.upsert_file(&rec(path)).unwrap();
        }
        let cases: &[(&str, &[&str])] = &[
            ("case: Foo", &["/Src/Foo.RS"]),
            ("case:foo", &["/src/foo.rs"]),
            ("foo", &["/Src/Foo.RS", "/src/foo.rs"]),
            ("case: wfn: foo.rs", &["/src/foo.rs"]),
            ("case: path: Src", &["/Src/Foo.RS"]),
            ("case: path: *.RS", &["/Src/Foo.RS"]),
            ("path: *.rs", &["/Src/Foo.RS", "/src/foo.rs"]),
            ("case: *.RS", &["/Src/Foo.RS"]),
            ("a*b*.txt", &["/src/a*b.txt", "/src/aXb.txt"]),
            ("nowildcards: a*b", &["/src/a*b.txt"]),
            (r"regex: ^a\.b$", &["/src/a.b"]),
            (r"regex:^a\.b$", &["/src/a.b"]),
            (r"path: regex: ^/src/a\.b$", &["/src/a.b"]),
            (r"regex: ^/src/a\.b$", &[]),
            (r"regex:Src", &[]),
            (
                r"path: regex:Src",
                &[
                    "/Src/Foo.RS",
                    "/src/a*b.txt",
                    "/src/a.b",
                    "/src/aXb.txt",
                    "/src/foo.rs",
                ],
            ),
            // Case folding beyond ASCII; accents still match exactly.
            ("café", &["/u/CAFÉ.txt", "/u/Café.md"]),
            ("CAFÉ", &["/u/CAFÉ.txt", "/u/Café.md"]),
            ("case: CAFÉ", &["/u/CAFÉ.txt"]),
            ("wfn: café.MD", &["/u/Café.md"]),
            ("cafÉ.*", &["/u/CAFÉ.txt", "/u/Café.md"]),
            ("*?É.TXT", &["/u/CAFÉ.txt"]),
            ("σοφια", &["/u/ΣΟΦΙΑ.txt"]),
            ("startwith:cafÉ", &["/u/CAFÉ.txt", "/u/Café.md"]),
            ("endwith:É.TXT", &["/u/CAFÉ.txt"]),
            ("path: /üNÏ/", &["/Ünï/ö.txt"]),
            ("path: *ünï/Ö*", &["/Ünï/ö.txt"]),
            ("w: café", &["/u/CAFÉ.txt", "/u/Café.md"]),
            // `_` and `?` are one character, not one byte.
            ("caf?.txt", &["/u/CAFÉ.txt", "/u/cafe.txt"]),
            // GLOB would read `[1]` as a character class.
            ("x[1]*", &["/u/x[1].log"]),
            ("case: x[1]*", &["/u/x[1].log"]),
            (r"case: path: regex:Src", &["/Src/Foo.RS"]),
            ("\"foo\"", &["/Src/Foo.RS", "/src/foo.rs"]),
            ("\"oo.r\"", &["/Src/Foo.RS", "/src/foo.rs"]),
            ("wfn: \"foo\"", &[]),
            ("case: \"Foo.RS\"", &["/Src/Foo.RS"]),
            ("nopath: src", &[]),
            ("<case: Foo> | aXb", &["/Src/Foo.RS", "/src/aXb.txt"]),
            ("!case: Foo foo", &["/src/foo.rs"]),
        ];
        for (q, want) in cases {
            assert_eq!(run(store, q), *want, "query {q:?}");
        }
    }

    #[test]
    fn sqlite_backend() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = SqliteStore::new(&dir.path().join("i.db")).unwrap();
        check(&FileStore::Sqlite(Arc::new(s)));
    }

    #[test]
    fn memory_backend() {
        let dir = tempfile::TempDir::new().unwrap();
        check(&FileStore::Memory(Arc::new(
            MemoryStore::new(dir.path()).unwrap(),
        )));
    }
}

#[test]
fn plain_extension_globs_are_narrowed_to_the_extension_index() {
    use crate::store::SqlCondition;
    let macros = MacroConfig::default();
    let ext_of = |q: &str| {
        let p = plan(&parse(q, &macros).unwrap(), &Modifiers::default()).unwrap();
        p.sql_conditions.iter().find_map(|c| match c {
            SqlCondition::ExtOrNone(e) => Some(e.clone()),
            _ => None,
        })
    };
    assert_eq!(ext_of("*.pdf").as_deref(), Some("pdf"));
    assert_eq!(ext_of("*.PDF").as_deref(), Some("pdf"));
    assert_eq!(ext_of("case: *.PDF").as_deref(), Some("pdf"));
    for q in [
        "*.p?f",
        "*.tar.gz",
        "a*.pdf",
        "path:*.pdf",
        "*.",
        "*.[pP]df",
        "nowildcards:*.pdf",
    ] {
        assert_eq!(ext_of(q), None, "{q}");
    }
}

mod functions_end_to_end {
    use super::*;
    use crate::store::{
        memory_store::MemoryStore, sqlite_store::SqliteStore, FileRecord, FileStore,
    };
    use std::sync::Arc;

    fn rec(path: &str, is_dir: bool) -> FileRecord {
        let name: String = path.rsplit('/').next().unwrap_or(path).into();
        FileRecord {
            id: None,
            path: path.into(),
            extension: (!is_dir)
                .then(|| name.rsplit_once('.').map(|(_, e)| e.to_lowercase()))
                .flatten(),
            name,
            size_bytes: 10,
            mtime: 0,
            ctime: 0,
            is_dir,
            is_hidden: false,
            is_symlink: false,
            attrib_bitmask: 0,
            content_indexed: false,
            tantivy_id: None,
            indexed_at: 0,
            archive_path: None,
        }
    }

    fn run(store: &FileStore, q: &str) -> Vec<String> {
        let p = parse_plan(q);
        let rows = store.search_conditions(&p.sql_conditions, 1000).unwrap();
        let mut paths: Vec<String> = apply_post_filters(rows, &p.post_filters, store)
            .into_iter()
            .map(|r| r.path)
            .collect();
        paths.sort();
        paths
    }

    fn check(store: &FileStore) {
        for (p, d) in [
            ("/r", true),
            ("/r/src", true),
            ("/r/src/Main.RS", false),
            ("/r/src/lib.rs", false),
            ("/r/src/sub", true),
            ("/r/src/sub/x.rs", false),
            ("/r/docs", true),
            ("/r/docs/café.md", false),
            ("/r/a_b", true),
            ("/r/a_b/in.txt", false),
            ("/r/aXb", true),
            ("/r/aXb/out.txt", false),
        ] {
            store.upsert_file(&rec(p, d)).unwrap();
        }
        let src_children: &[&str] = &["/r/src/Main.RS", "/r/src/lib.rs", "/r/src/sub"];
        let cases: &[(&str, &[&str])] = &[
            (
                "path:src",
                &[
                    "/r/src",
                    "/r/src/Main.RS",
                    "/r/src/lib.rs",
                    "/r/src/sub",
                    "/r/src/sub/x.rs",
                ],
            ),
            ("path:sub/*.rs", &[]),
            ("path:*/sub/*.rs", &["/r/src/sub/x.rs"]),
            ("startwith:main", &["/r/src/Main.RS"]),
            ("case: startwith:main", &[]),
            ("case: startwith:Main", &["/r/src/Main.RS"]),
            (
                "endwith:.rs",
                &["/r/src/Main.RS", "/r/src/lib.rs", "/r/src/sub/x.rs"],
            ),
            ("case: endwith:.RS", &["/r/src/Main.RS"]),
            (
                "len:7",
                &["/r/aXb/out.txt", "/r/docs/café.md", "/r/src/Main.RS"],
            ),
            ("parent:/r/src", src_children),
            ("parent:/r/src/", src_children),
            ("infolder:/r/src", src_children),
            ("nosubfolders:/r/src", src_children),
            ("parent:/r/a_b", &["/r/a_b/in.txt"]),
            ("parent:/R/Src", src_children),
            ("parent:/", &["/r"]),
            ("parents:2", &["/r/aXb", "/r/a_b", "/r/docs", "/r/src"]),
            ("root:", &["/r"]),
            (
                "folder:",
                &["/r", "/r/aXb", "/r/a_b", "/r/docs", "/r/src", "/r/src/sub"],
            ),
            ("file: <lib | café>", &["/r/docs/café.md", "/r/src/lib.rs"]),
        ];
        for (q, want) in cases {
            assert_eq!(run(store, q), *want, "query {q:?}");
        }
    }

    #[test]
    fn sqlite_backend() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = SqliteStore::new(&dir.path().join("i.db")).unwrap();
        check(&FileStore::Sqlite(Arc::new(s)));
    }

    #[test]
    fn memory_backend() {
        let dir = tempfile::TempDir::new().unwrap();
        check(&FileStore::Memory(Arc::new(
            MemoryStore::new(dir.path()).unwrap(),
        )));
    }

    #[test]
    fn dupe_spellings_from_the_docs_parse() {
        for q in ["dmdupe:", "dcdupe:", "sizedupe:", "namedupe:", "dmdup:"] {
            assert!(
                matches!(parse_plan(q).strategy, EvalStrategy::DupeSearch(_)),
                "{q}"
            );
        }
    }

    #[test]
    fn archiveof_accepts_wildcards() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = SqliteStore::new(&dir.path().join("i.db")).unwrap();
        let mem = MemoryStore::new(dir.path()).unwrap();
        for store in [
            FileStore::Sqlite(Arc::new(s)),
            FileStore::Memory(Arc::new(mem)),
        ] {
            for (outer, inner) in [("/z/Backup.zip", "a.txt"), ("/z/other.zip", "b.txt")] {
                let mut r = rec(&format!("{outer}!/{inner}"), false);
                r.archive_path = Some(outer.into());
                store.upsert_file(&r).unwrap();
            }
            store.upsert_file(&rec("/z/c.txt", false)).unwrap();
            assert_eq!(
                run(&store, "archiveof:*/backup.zip"),
                ["/z/Backup.zip!/a.txt"]
            );
            assert_eq!(
                run(&store, "case: archiveof:*/backup.zip"),
                Vec::<String>::new()
            );
            assert_eq!(
                run(&store, "archiveof:/z/other.zip"),
                ["/z/other.zip!/b.txt"]
            );
        }
    }

    #[test]
    fn unsupported_functions_are_errors_not_text() {
        let macros = MacroConfig::default();
        for q in [
            "rc:today",
            "dr:today",
            "childcount:2",
            "childfilecount:1",
            "empty:",
            "dadupe:",
            "dupe:copy",
            "sizedupe:5",
            "dmdupe:today",
        ] {
            assert!(parse(q, &macros).is_err(), "{q}");
        }
        for q in [
            "dupe:",
            "sizedupe: *.mp4",
            "dmdupe: ext:rs",
            "dupe:size",
            "dupe:date",
        ] {
            assert!(parse(q, &macros).is_ok(), "{q}");
        }
        use crate::query::ast::{Expr, Function, PredicateKind};
        use crate::store::DupeKind;
        let kind = |q: &str| match parse(q, &macros).unwrap() {
            Expr::Predicate(p) => match p.kind {
                PredicateKind::Function(Function::Dupe(k)) => k,
                other => panic!("{q}: {other:?}"),
            },
            other => panic!("{q}: {other:?}"),
        };
        assert_eq!(kind("dupe:"), DupeKind::Name);
        assert_eq!(kind("dupe:name"), DupeKind::Name);
        assert_eq!(kind("dupe:SIZE"), DupeKind::Size);
        assert_eq!(kind("dupe:date"), DupeKind::DateModified);
        assert_eq!(kind("dupe:dc"), DupeKind::DateCreated);
    }
}

mod values {
    use super::*;
    use chrono::{Local, NaiveDate, TimeZone};

    fn local(y: i32, m: u32, d: u32, h: u32, mi: u32, sec: u32) -> i64 {
        let dt = NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, mi, sec)
            .unwrap();
        Local
            .from_local_datetime(&dt)
            .earliest()
            .unwrap()
            .timestamp()
    }

    fn mtime(q: &str) -> (i64, i64) {
        match parse_plan(q).sql_conditions.as_slice() {
            [SqlCondition::MtimeRange(lo, hi)] => (*lo, *hi),
            other => panic!("{q}: {other:?}"),
        }
    }

    #[test]
    fn dates_cover_whole_local_periods() {
        let day = (local(2024, 1, 1, 0, 0, 0), local(2024, 1, 1, 23, 59, 59));
        assert_eq!(mtime("dm:2024-01-01"), day);
        assert_eq!(mtime("dm:20240101"), day);
        assert_eq!(mtime("dm:=2024-01-01"), day);
        assert_eq!(
            mtime("dm:2024"),
            (local(2024, 1, 1, 0, 0, 0), local(2024, 12, 31, 23, 59, 59))
        );
        assert_eq!(
            mtime("dm:2024-02"),
            (local(2024, 2, 1, 0, 0, 0), local(2024, 2, 29, 23, 59, 59))
        );
        let t = local(2024, 1, 1, 10, 0, 0);
        assert_eq!(mtime("dm:2024-01-01T10:00:00"), (t, t));
    }

    #[test]
    fn date_ranges_include_the_whole_end_period() {
        let want = (local(2024, 1, 1, 0, 0, 0), local(2024, 12, 31, 23, 59, 59));
        assert_eq!(mtime("dm:2024-01-01..2024-12-31"), want);
        assert_eq!(mtime("dm:2024-01-01-2024-12-31"), want);
        assert_eq!(mtime("dm:2023..2024"), (local(2023, 1, 1, 0, 0, 0), want.1));
        assert_eq!(mtime("dm:2023-2024"), mtime("dm:2023..2024"));
    }

    #[test]
    fn date_comparisons_treat_the_value_as_a_period() {
        assert_eq!(mtime("dm:>2024-01-01").0, local(2024, 1, 2, 0, 0, 0));
        assert_eq!(mtime("dm:>=2024-01-01").0, local(2024, 1, 1, 0, 0, 0));
        assert_eq!(mtime("dm:<2024-01-01").1, local(2023, 12, 31, 23, 59, 59));
        assert_eq!(mtime("dm:<=2024-01-01").1, local(2024, 1, 1, 23, 59, 59));
        assert_eq!(mtime("dm:<2024").1, local(2023, 12, 31, 23, 59, 59));
    }

    #[test]
    fn date_comparisons_accept_constants() {
        let (today_lo, today_hi) = mtime("dm:today");
        assert_eq!(mtime("dm:>=today"), (today_lo, i64::MAX));
        assert_eq!(mtime("dm:<today"), (i64::MIN, today_lo - 1));
        assert_eq!(mtime("dm:yesterday..today").1, today_hi);
    }

    #[test]
    fn sizes_accept_dash_ranges_and_terabytes() {
        let conds = |q: &str| format!("{:?}", parse_plan(q).sql_conditions);
        assert_eq!(conds("size:1kb-2kb"), conds("size:1kb..2kb"));
        assert_eq!(conds("size:1tb"), format!("[SizeOp(Eq, {})]", 1u64 << 40));
        let macros = MacroConfig::default();
        assert!(parse("size:-1", &macros).is_err());
        assert!(parse("size:nan", &macros).is_err());
    }

    #[test]
    fn len_and_depth_accept_ranges() {
        let conds = |q: &str| format!("{:?}", parse_plan(q).sql_conditions);
        assert_eq!(
            conds("len:5..10"),
            "[NameLenOp(Gte, 5), NameLenOp(Lte, 10)]"
        );
        assert_eq!(conds("depth:1-2"), "[DepthOp(Gte, 1), DepthOp(Lte, 2)]");
    }

    #[test]
    fn unknown_attribute_letters_are_errors() {
        let macros = MacroConfig::default();
        assert!(parse("attrib:Z", &macros).is_err());
        assert_eq!(
            format!("{:?}", parse_plan("attrib:L").sql_conditions),
            format!("{:?}", parse_plan("attrib:J").sql_conditions)
        );
    }

    #[test]
    fn escapes_are_literal_characters() {
        let conds = |q: &str| format!("{:?}", parse_plan(q).sql_conditions);
        assert_eq!(conds("quot:a"), r#"[NameLike("%\"a%")]"#);
        assert_eq!(conds("#x41:b"), r#"[NameLike("%Ab%")]"#);
        assert_eq!(conds("lt:"), r#"[NameLike("%<%")]"#);
    }

    #[test]
    fn terms_with_separators_search_paths() {
        let conds = |q: &str| format!("{:?}", parse_plan(q).sql_conditions);
        assert_eq!(conds("src/main"), r#"[PathLike("%src/main%")]"#);
        assert_eq!(conds(r"docs\a"), r#"[PathLike("%docs\\\\a%")]"#);
        assert_eq!(conds("nopath: src/main"), r#"[NameLike("%src/main%")]"#);
    }

    #[test]
    fn archive_paths_with_bang_stay_one_term() {
        let conds = |q: &str| format!("{:?}", parse_plan(q).sql_conditions);
        assert_eq!(conds("b.zip!/c"), r#"[PathLike("%b.zip!/c%")]"#);
    }
}

#[test]
fn user_macros_expand_only_at_token_start() {
    let mut macros = MacroConfig::default();
    macros.entries.insert("work".into(), "ext:rs".into());
    macros.entries.insert("w".into(), "ext:md".into());
    let conds = |q: &str| {
        let e = parse(q, &macros).unwrap();
        format!("{:?}", plan(&e, &empty_mods()).unwrap().sql_conditions)
    };
    assert_eq!(conds("work:"), r#"[ExtIn(["rs"])]"#);
    assert_eq!(conds("homework:x"), r#"[NameLike("%homework:x%")]"#);
    assert_eq!(conds("!work:"), r#"[Not([ExtIn(["rs"])])]"#);
    assert_eq!(conds(r#""work:""#), r#"[NameLike("%work:%")]"#);
}
