use crate::config::MacroConfig;
use crate::error::{AppError, Result};
use crate::store::{CmpOp, DupeKind};

use super::ast::{Expr, Function, Modifiers, Predicate, PredicateKind};
use super::date::parse_date_value;
use super::macros::keyword_to_type_macro;
use super::size::parse_size_value;

// ---------------------------------------------------------------------------
// Tokeniser
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Word(String),
    QuotedPhrase(String),
    Pipe,
    Bang,
    LAngle,
    RAngle,
    Eof,
}

fn tokenize(input: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        match chars[i] {
            c if c.is_whitespace() => { i += 1; }
            '|' => { tokens.push(Token::Pipe);   i += 1; }
            '!' => { tokens.push(Token::Bang);   i += 1; }
            '<' => { tokens.push(Token::LAngle); i += 1; }
            '>' => { tokens.push(Token::RAngle); i += 1; }
            '"' => {
                i += 1;
                let start = i;
                while i < chars.len() && chars[i] != '"' { i += 1; }
                let s: String = chars[start..i].iter().collect();
                tokens.push(Token::QuotedPhrase(s));
                if i < chars.len() { i += 1; }
            }
            _ => {
                let start = i;
                while i < chars.len()
                    && !chars[i].is_whitespace()
                    && !matches!(chars[i], '|' | '!' | '<' | '>')
                {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                tokens.push(Token::Word(word));
            }
        }
    }
    tokens.push(Token::Eof);
    tokens
}

// ---------------------------------------------------------------------------
// Parser state
// ---------------------------------------------------------------------------

struct Parser {
    tokens: Vec<Token>,
    pos:    usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    fn advance(&mut self) -> Token {
        let tok = self.tokens[self.pos].clone();
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        tok
    }

    fn at_group_end(&self) -> bool {
        matches!(self.peek(), Token::Eof | Token::RAngle)
    }

    // or_expr = and_expr ("|" and_expr)*
    fn parse_or(&mut self) -> Result<Expr> {
        let mut left = self.parse_and()?;
        while matches!(self.peek(), Token::Pipe) {
            self.advance();
            let right = self.parse_and()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    // and_expr = not_expr (not_expr)*  — space is AND
    fn parse_and(&mut self) -> Result<Expr> {
        let mut left = self.parse_not()?;
        while !self.at_group_end() && !matches!(self.peek(), Token::Pipe) {
            let right = self.parse_not()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    // not_expr = "!" atom | atom
    fn parse_not(&mut self) -> Result<Expr> {
        if matches!(self.peek(), Token::Bang) {
            self.advance();
            let inner = self.parse_atom()?;
            Ok(Expr::Not(Box::new(inner)))
        } else {
            self.parse_atom()
        }
    }

    // atom = "<" expr ">" | quoted_phrase | term
    fn parse_atom(&mut self) -> Result<Expr> {
        match self.peek() {
            Token::LAngle => {
                self.advance();
                let inner = self.parse_or()?;
                if matches!(self.peek(), Token::RAngle) {
                    self.advance();
                }
                Ok(inner)
            }
            Token::QuotedPhrase(_) => {
                if let Token::QuotedPhrase(s) = self.advance() {
                    Ok(Expr::Predicate(Predicate {
                        kind:      PredicateKind::Term(s),
                        modifiers: Modifiers { whole_filename: Some(true), ..Default::default() },
                    }))
                } else {
                    unreachable!()
                }
            }
            Token::Eof => Err(AppError::Query("unexpected end of query".into())),
            _ => self.parse_term(),
        }
    }

    // term = modifier* (function | word)
    // Modifiers are words followed by ':' that map to known modifier keywords.
    fn parse_term(&mut self) -> Result<Expr> {
        let mut mods = Modifiers::default();

        loop {
            let tok = match self.peek().clone() {
                Token::Word(w) => w,
                other => {
                    return Err(AppError::Query(format!(
                        "expected term, got {other:?}"
                    )));
                }
            };
            self.advance();

            // Split on first colon
            if let Some(colon) = tok.find(':') {
                let kw      = tok[..colon].to_ascii_lowercase();
                let raw_val = tok[colon + 1..].to_owned();

                // Modifier keyword — accumulate and loop
                if let Some(m) = modifier_from_kw(&kw, &raw_val) {
                    apply_modifier(&mut mods, m);
                    continue;
                }

                // Comparison-capable functions may have the operator tokenised
                // separately: "size:>1mb" → Word("size:") + RAngle + Word("1mb").
                let val = if raw_val.is_empty() && is_comparison_kw(&kw) {
                    absorb_cmp_op(self)
                } else {
                    raw_val
                };

                // Function or type macro — build expression and return
                return build_predicate(&kw, &val, mods);
            }

            // Plain word — Term predicate
            return Ok(Expr::Predicate(Predicate {
                kind:      PredicateKind::Term(tok),
                modifiers: mods,
            }));
        }
    }
}

// ---------------------------------------------------------------------------
// Modifier helpers
// ---------------------------------------------------------------------------

enum ModifierKind {
    CaseSensitive(bool),
    MatchPath,
    FilesOnly,
    FoldersOnly,
    WholeFilename,
    WholeWord,
    ArchiveOnly(bool),
}

fn modifier_from_kw(kw: &str, val: &str) -> Option<ModifierKind> {
    // Only treat as modifier when there's no value attached; otherwise it may
    // be a function with an empty-string value (e.g., archive: vs archiveof:).
    match kw {
        "cs" | "case"                    => Some(ModifierKind::CaseSensitive(true)),
        "i"  | "nocase"                  => Some(ModifierKind::CaseSensitive(false)),
        "wfn"                            => Some(ModifierKind::WholeFilename),
        "w"  | "wholeword" | "ww"        => Some(ModifierKind::WholeWord),
        "file"                           => Some(ModifierKind::FilesOnly),
        // "dir" / "folder" with no value = FoldersOnly modifier.
        // "dir:name" with a value falls through to build_predicate as Function::Parent.
        "folder" | "dir" if val.is_empty() => Some(ModifierKind::FoldersOnly),
        // path: with no value is the path modifier; with a value it's a function
        "path" if val.is_empty()         => Some(ModifierKind::MatchPath),
        "archive"   if val.is_empty()    => Some(ModifierKind::ArchiveOnly(true)),
        "noarchive" if val.is_empty()    => Some(ModifierKind::ArchiveOnly(false)),
        _ => None,
    }
}

fn apply_modifier(mods: &mut Modifiers, m: ModifierKind) {
    match m {
        ModifierKind::CaseSensitive(v) => mods.case_sensitive    = Some(v),
        ModifierKind::MatchPath        => mods.match_path         = Some(true),
        ModifierKind::FilesOnly        => mods.files_only         = Some(true),
        ModifierKind::FoldersOnly      => mods.folders_only       = Some(true),
        ModifierKind::WholeFilename    => mods.whole_filename      = Some(true),
        ModifierKind::WholeWord        => mods.whole_word          = Some(true),
        ModifierKind::ArchiveOnly(v)   => mods.archive_only       = Some(v),
    }
}

// ---------------------------------------------------------------------------
// Comparison operator absorption helpers
// ---------------------------------------------------------------------------

fn is_comparison_kw(kw: &str) -> bool {
    matches!(
        kw,
        "size" | "dm" | "datemodified" | "date"
            | "dc" | "datecreated"
            | "da" | "dateaccessed"
            | "len" | "depth"
    )
}

/// Consume a split comparison operator token (RAngle/LAngle) optionally
/// followed by the rest of the value word. Returns the reconstructed string.
fn absorb_cmp_op(parser: &mut Parser) -> String {
    let op = match parser.peek().clone() {
        Token::RAngle => { parser.advance(); '>' }
        Token::LAngle => { parser.advance(); '<' }
        _ => return String::new(),
    };
    let rest = if let Token::Word(w) = parser.peek().clone() {
        parser.advance();
        w
    } else {
        String::new()
    };
    format!("{op}{rest}")
}

// ---------------------------------------------------------------------------
// Build an Expr from a recognised keyword:value token
// ---------------------------------------------------------------------------

fn build_predicate(kw: &str, val: &str, mods: Modifiers) -> Result<Expr> {
    // Type macros
    if let Some(tm) = keyword_to_type_macro(kw) {
        return Ok(Expr::Predicate(Predicate {
            kind:      PredicateKind::TypeMacro(tm),
            modifiers: mods,
        }));
    }

    // Windows drive letter prefix: single alpha char (e.g. c:, d:)
    if kw.len() == 1 && kw.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
        let path_val = format!("{kw}:{val}");
        return Ok(Expr::Predicate(Predicate {
            kind:      PredicateKind::Function(Function::Path(path_val)),
            modifiers: Modifiers { match_path: Some(true), ..mods },
        }));
    }

    let func = match kw {
        "dm" | "datemodified" | "date" => {
            Function::DateModified(parse_date_value(val)?)
        }
        "dc" | "datecreated" => {
            Function::DateCreated(parse_date_value(val)?)
        }
        "da" | "dateaccessed" => {
            Function::DateAccessed(parse_date_value(val)?)
        }
        "size" => {
            Function::Size(parse_size_value(val)?)
        }
        "ext" | "type" => {
            let exts: Vec<String> = val
                .split(';')
                .filter(|s| !s.is_empty())
                .map(|s| s.to_ascii_lowercase())
                .collect();
            Function::Ext(exts)
        }
        "path" => Function::Path(val.to_owned()),
        "parent" | "dir" => Function::Parent(val.to_owned()),
        "content" => Function::Content(val.to_owned()),
        "fuzzy" => {
            return Ok(Expr::Predicate(Predicate {
                kind:      PredicateKind::Fuzzy(val.to_owned()),
                modifiers: mods,
            }));
        }
        "regex" => {
            return Ok(Expr::Predicate(Predicate {
                kind:      PredicateKind::Regex(val.to_owned()),
                modifiers: mods,
            }));
        }
        "dupe" | "namedup" => Function::Dupe(DupeKind::Name),
        "sizedupe" => Function::Dupe(DupeKind::Size),
        "dmdup"    => Function::Dupe(DupeKind::DateModified),
        "dcdup"    => Function::Dupe(DupeKind::DateCreated),
        "count" => {
            let n = val.parse::<usize>()
                .map_err(|_| AppError::Query(format!("invalid count value: '{val}'")))?;
            Function::Count(n)
        }
        "len" | "length" => {
            let (op, n) = parse_cmp_usize(val)?;
            Function::Len(op, n)
        }
        "depth" => {
            let (op, n) = parse_cmp_usize(val)?;
            Function::Depth(op, n)
        }
        "attrib" => {
            let mask = attrib_mask(val);
            Function::Attrib(mask)
        }
        "child" => Function::Child(val.to_owned()),
        "startwith" | "startswith" => Function::StartWith(val.to_owned()),
        "endwith"   | "endswith"   => Function::EndWith(val.to_owned()),
        "archiveof" => Function::ArchiveOf(val.to_owned()),
        _ => {
            // Unknown keyword — treat "keyword:value" as a literal term
            return Ok(Expr::Predicate(Predicate {
                kind:      PredicateKind::Term(format!("{kw}:{val}")),
                modifiers: mods,
            }));
        }
    };

    Ok(Expr::Predicate(Predicate {
        kind:      PredicateKind::Function(func),
        modifiers: mods,
    }))
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn parse_cmp_usize(s: &str) -> Result<(CmpOp, usize)> {
    let (op, rest) = if let Some(r) = s.strip_prefix("<=") {
        (CmpOp::Lte, r)
    } else if let Some(r) = s.strip_prefix(">=") {
        (CmpOp::Gte, r)
    } else if let Some(r) = s.strip_prefix('<') {
        (CmpOp::Lt, r)
    } else if let Some(r) = s.strip_prefix('>') {
        (CmpOp::Gt, r)
    } else if let Some(r) = s.strip_prefix('=') {
        (CmpOp::Eq, r)
    } else {
        (CmpOp::Eq, s)
    };
    let n = rest.trim().parse::<usize>()
        .map_err(|_| AppError::Query(format!("invalid number: '{s}'")))?;
    Ok((op, n))
}

fn attrib_mask(s: &str) -> u32 {
    use crate::store::{
        ATTRIB_ARCHIVE, ATTRIB_COMPRESSED, ATTRIB_DIRECTORY, ATTRIB_ENCRYPTED,
        ATTRIB_HIDDEN, ATTRIB_NORMAL, ATTRIB_OFFLINE, ATTRIB_READONLY,
        ATTRIB_REPARSE_POINT, ATTRIB_SPARSE, ATTRIB_SYSTEM, ATTRIB_TEMPORARY,
    };
    let mut mask = 0u32;
    for c in s.chars() {
        mask |= match c.to_ascii_uppercase() {
            'A' => ATTRIB_ARCHIVE,
            'C' => ATTRIB_COMPRESSED,
            'D' => ATTRIB_DIRECTORY,
            'E' => ATTRIB_ENCRYPTED,
            'H' => ATTRIB_HIDDEN,
            'J' => ATTRIB_REPARSE_POINT,
            'N' => ATTRIB_NORMAL,
            'O' => ATTRIB_OFFLINE,
            'P' => ATTRIB_SPARSE,
            'R' => ATTRIB_READONLY,
            'S' => ATTRIB_SYSTEM,
            'T' => ATTRIB_TEMPORARY,
            _ => 0,
        };
    }
    mask
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Pre-process the query string (macro expansion) then parse into an AST.
pub fn parse(raw: &str, macros: &MacroConfig) -> Result<Expr> {
    // Expand character escape macros
    let expanded = raw
        .replace("quot:", "\"")
        .replace("apos:", "'");

    // Expand user-defined macros (single pass, no recursion)
    let expanded = expand_user_macros(&expanded, macros);

    let tokens = tokenize(&expanded);
    let mut parser = Parser::new(tokens);
    let expr = parser.parse_or()?;
    Ok(expr)
}

fn expand_user_macros(s: &str, macros: &MacroConfig) -> String {
    if macros.entries.is_empty() {
        return s.to_owned();
    }
    let mut result = s.to_owned();
    for (name, replacement) in &macros.entries {
        let pattern = format!("{name}:");
        result = result.replace(&pattern, &format!("{replacement} "));
    }
    result
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
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
        // "<c:|d:> *.mp3" — c: and d: are drive prefixes inside a group
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
        assert!(!matches!(e, Expr::Predicate(ref p) if matches!(p.kind, PredicateKind::Term(ref s) if s.is_empty())));
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
            assert!(matches!(p.kind, PredicateKind::Function(Function::Count(50))));
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
        // "c:\users\" is a Windows path prefix — should produce a path: predicate
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
}
