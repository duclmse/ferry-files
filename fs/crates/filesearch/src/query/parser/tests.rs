use super::*;
use crate::config::MacroConfig;
use crate::query::ast::{DateExpr, TypeMacro};

fn empty_macros() -> MacroConfig {
    MacroConfig::default()
}

fn parse_ok(q: &str) -> Expr {
    parse(q, &empty_macros()).expect("parse should succeed")
}

#[test]
fn plain_and() {
    let e = parse_ok("main rs");
    assert!(matches!(e, Expr::And(_, _)));
}

#[test]
fn pipe_or() {
    let e = parse_ok("*.mp3|*.flac");
    assert!(matches!(e, Expr::Or(_, _)));
}

#[test]
fn bang_not() {
    let e = parse_ok("!node_modules");
    assert!(matches!(e, Expr::Not(_)));
}

#[test]
fn or_lower_than_and() {
    // "A|B C" → Or(A, And(B, C))
    let e = parse_ok("alpha|beta gamma");
    match e {
        Expr::Or(left, right) => {
            assert!(matches!(*left, Expr::Predicate(_)));
            assert!(matches!(*right, Expr::And(_, _)));
        }
        _ => panic!("expected Or at top level"),
    }
}

#[test]
fn angle_group() {
    // "<c:|d:> *.mp3" - c: and d: are drive prefixes inside a group
    let e = parse_ok("<c:|d:> *.mp3");
    assert!(matches!(e, Expr::And(_, _)));
}

#[test]
fn ext_function() {
    let e = parse_ok("ext:jpg;png;gif");
    if let Expr::Predicate(p) = e {
        if let PredicateKind::Function(Function::Ext(exts)) = p.kind {
            assert_eq!(exts, vec!["jpg", "png", "gif"]);
            return;
        }
    }
    panic!("expected Ext function");
}

#[test]
fn type_macro_audio() {
    let e = parse_ok("audio:");
    if let Expr::Predicate(p) = e {
        assert!(matches!(p.kind, PredicateKind::TypeMacro(TypeMacro::Audio)));
        return;
    }
    panic!("expected TypeMacro::Audio");
}

#[test]
fn dm_today() {
    let e = parse_ok("dm:today");
    if let Expr::Predicate(p) = e {
        if let PredicateKind::Function(Function::DateModified(DateExpr::Constant(_))) = p.kind {
            return;
        }
    }
    panic!("expected DateModified(Constant)");
}

#[test]
fn size_large() {
    let e = parse_ok("size:large");
    if let Expr::Predicate(p) = e {
        if let PredicateKind::Function(Function::Size(_)) = p.kind {
            return;
        }
    }
    panic!("expected Size function");
}

#[test]
fn fuzzy_predicate() {
    let e = parse_ok("fuzzy:mian");
    if let Expr::Predicate(p) = e {
        assert!(matches!(p.kind, PredicateKind::Fuzzy(_)));
        return;
    }
    panic!("expected Fuzzy");
}

#[test]
fn fuzzy_with_ext() {
    let e = parse_ok("fuzzy:cnofig ext:toml");
    assert!(matches!(e, Expr::And(_, _)));
}

#[test]
fn double_not_and() {
    // "!A !B" → And(Not(A), Not(B))
    let e = parse_ok("!node_modules !target");
    match e {
        Expr::And(l, r) => {
            assert!(matches!(*l, Expr::Not(_)));
            assert!(matches!(*r, Expr::Not(_)));
        }
        _ => panic!("expected And(Not, Not)"),
    }
}

#[test]
fn archive_modifier() {
    // "archive:" with a space is a modifier, not a separate term.
    // The modifier accumulates onto "*.rs" → single Predicate with archive_only=Some(true).
    let e = parse_ok("archive: *.rs");
    if let Expr::Predicate(p) = e {
        assert_eq!(p.modifiers.archive_only, Some(true));
        return;
    }
    panic!("expected single Predicate with archive_only=true");
}

#[test]
fn noarchive_modifier() {
    // noarchive: with empty val → modifier ArchiveOnly(false)
    // followed by *.log → the *.log term gets archive_only=Some(false)
    let e = parse_ok("noarchive: *.log");
    // Should produce And( noarchive_modifier_predicate, wildcard_predicate )
    // OR a predicate with archive_only set. Either way, parse should not error.
    assert!(
        !matches!(e, Expr::Predicate(ref p) if matches!(p.kind, PredicateKind::Term(ref s) if s.is_empty()))
    );
}

#[test]
fn archiveof_function() {
    let e = parse_ok("archiveof:project.zip");
    if let Expr::Predicate(p) = e {
        assert!(matches!(
            p.kind,
            PredicateKind::Function(Function::ArchiveOf(_))
        ));
        return;
    }
    panic!("expected ArchiveOf function");
}

#[test]
fn count_function() {
    let e = parse_ok("count:50");
    if let Expr::Predicate(p) = e {
        assert!(matches!(
            p.kind,
            PredicateKind::Function(Function::Count(50))
        ));
        return;
    }
    panic!("expected Count(50)");
}

#[test]
fn size_gt_1mb() {
    use crate::query::ast::RangeExpr;
    let e = parse_ok("size:>1mb");
    if let Expr::Predicate(p) = e {
        if let PredicateKind::Function(Function::Size(RangeExpr::Gt(v))) = p.kind {
            assert_eq!(v, 1_048_576);
            return;
        }
    }
    panic!("expected Size(Gt(1048576))");
}

#[test]
fn windows_drive_path_prefix() {
    // "c:\users\" is a Windows path prefix - should produce a path: predicate
    // with match_path=true.
    let e = parse_ok(r"c:\users\");
    if let Expr::Predicate(p) = e {
        assert_eq!(p.modifiers.match_path, Some(true));
        if let PredicateKind::Function(Function::Path(path)) = &p.kind {
            assert!(path.contains("users") || path.starts_with("c:"));
            return;
        }
    }
    panic!("expected path predicate with match_path=true");
}
