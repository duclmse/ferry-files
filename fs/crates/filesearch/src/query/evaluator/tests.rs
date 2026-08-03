use super::*;
use crate::query::ast::Modifiers;
use crate::query::parser::parse;
use crate::config::MacroConfig;

fn empty_mods() -> Modifiers { Modifiers::default() }

fn parse_plan(q: &str) -> QueryPlan {
    let macros = MacroConfig::default();
    let expr   = parse(q, &macros).unwrap();
    plan(&expr, &empty_mods()).unwrap()
}

#[test]
fn plain_term_produces_name_like() {
    let p = parse_plan("main");
    assert!(p.sql_conditions.iter().any(|c| matches!(c, SqlCondition::NameLike(_))));
    assert!(matches!(p.strategy, EvalStrategy::SqliteOnly));
}

#[test]
fn wildcard_produces_glob() {
    let p = parse_plan("*.rs");
    assert!(p.sql_conditions.iter().any(|c| matches!(c, SqlCondition::NameGlob(_))));
}

#[test]
fn ext_produces_ext_in() {
    let p = parse_plan("ext:jpg;png");
    assert!(p.sql_conditions.iter().any(|c| matches!(c, SqlCondition::ExtIn(_))));
}

#[test]
fn size_large_produces_size_range() {
    let p = parse_plan("size:large");
    assert!(p.sql_conditions.iter().any(|c| matches!(c, SqlCondition::SizeRange(_, _))));
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
    assert!(p.sql_conditions.iter().any(|c| matches!(c, SqlCondition::IsArchiveEntry)));
}

#[test]
fn noarchive_post_filter() {
    let p = parse_plan("noarchive: *.log");
    assert!(p.post_filters.iter().any(|f| matches!(f, PostFilter::ArchiveExclude)));
}

#[test]
fn archiveof_produces_condition() {
    let p = parse_plan("archiveof:project.zip");
    assert!(p.sql_conditions.iter().any(|c| matches!(c, SqlCondition::ArchiveOf(_))));
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
    assert!(p.sql_conditions.iter().any(|c| matches!(c, SqlCondition::MtimeRange(_, _))));
    assert!(matches!(p.strategy, EvalStrategy::SqliteOnly));
}

#[test]
fn size_gt_1mb_produces_size_op() {
    let p = parse_plan("size:>1mb");
    assert!(p.sql_conditions.iter().any(|c| matches!(c, SqlCondition::SizeOp(CmpOp::Gt, 1_048_576))));
}
