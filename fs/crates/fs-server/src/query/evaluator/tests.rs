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
        let hits = store.fuzzy_search(&[], 1000, &rank("mian")).unwrap();
        assert_eq!(hits.first().map(|r| r.path.as_str()), Some("/p/main.rs"));
        let cond = parse_plan("ext:txt").sql_conditions;
        let hits = store.fuzzy_search(&cond, 1000, &rank("notes")).unwrap();
        assert_eq!(
            hits.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["notes.txt"]
        );
        assert!(store
            .fuzzy_search(&cond, 1000, &rank("main"))
            .unwrap()
            .is_empty());
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
    assert!(has("path:/a_b", r"/a\_b%"));
}
