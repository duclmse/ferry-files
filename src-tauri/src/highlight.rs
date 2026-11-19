//! Colours for code in the text preview, with syntect's grammars.
//!
//! The preview reads a file a piece at a time, so the parser's state at the end of a piece is kept
//! (a few files' worth) and the next piece goes on from it: a comment or string that spans two
//! pieces stays coloured. Each piece comes back as runs of (kind, bytes); the frontend makes a
//! `<span>` per run, so nothing from the file is ever read as HTML. Kinds are classes, coloured by
//! CSS tokens that follow light and dark mode, not by a fixed theme.
//!
//! To stay quick: lines longer than `LONG_LINE` are left plain (minified files), colours stop after
//! `COLOUR_MAX` bytes or when a piece takes longer than `BUDGET`.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

/// Lines longer than this stay plain.
const LONG_LINE: usize = 5_000;
/// Colours stop this far into a file.
pub(crate) const COLOUR_MAX: u64 = 2 * 1024 * 1024;
/// The most time a piece may take to colour; past it the rest of the file is plain.
const BUDGET: Duration = Duration::from_millis(400);
/// How many files' states are kept for their next piece.
const KEPT: usize = 8;

/// What a run of text is. Its number is what the frontend gets (`tok-…` classes there).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(crate) enum Kind {
    Plain = 0,
    Comment = 1,
    Str = 2,
    Number = 3,
    Keyword = 4,
    Type = 5,
    Function = 6,
    Tag = 7,
    Attr = 8,
    Heading = 9,
}

/// Scope prefixes and their kinds, checked from the innermost scope out. `None` passes the scope
/// over (punctuation inside a comment is still the comment); `Some(Plain)` stops there.
const RULES: &[(&str, Option<Kind>)] = &[
    ("punctuation", None),
    ("comment", Some(Kind::Comment)),
    ("string", Some(Kind::Str)),
    ("markup.raw", Some(Kind::Str)),
    ("constant.character.escape", Some(Kind::Str)),
    ("constant.numeric", Some(Kind::Number)),
    ("constant.language", Some(Kind::Number)),
    ("keyword.operator", Some(Kind::Plain)),
    ("keyword", Some(Kind::Keyword)),
    // `fn`, `let`, `class` are storage.type in many grammars: keywords to the eye. Primitive types
    // (`int`, `u8`) are types.
    ("storage.type.primitive", Some(Kind::Type)),
    ("storage", Some(Kind::Keyword)),
    ("entity.name.function", Some(Kind::Function)),
    ("support.function", Some(Kind::Function)),
    ("variable.function", Some(Kind::Function)),
    ("entity.name.tag", Some(Kind::Tag)),
    ("entity.other.attribute-name", Some(Kind::Attr)),
    ("support.type.property-name", Some(Kind::Attr)),
    ("entity.name", Some(Kind::Type)),
    ("support.type", Some(Kind::Type)),
    ("support.class", Some(Kind::Type)),
    ("markup.heading", Some(Kind::Heading)),
];

fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// The grammar for a file, by its name, then its extension, then its first line (`#!/bin/sh`).
/// None for plain text.
fn syntax_for<'a>(ss: &'a SyntaxSet, name: &str, first_line: &str) -> Option<&'a SyntaxReference> {
    let file = name.rsplit('/').next().unwrap_or(name);
    let ext = file.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    // Close enough to a grammar that's there.
    let ext = ext.map(|e| match e.as_str() {
        "ts" | "tsx" | "mts" | "cts" | "jsx" | "mjs" | "cjs" => "js".to_string(),
        "jsonc" | "json5" | "webmanifest" => "json".to_string(),
        "kt" | "kts" => "java".to_string(),
        "zsh" | "fish" => "sh".to_string(),
        "htm" | "xhtml" => "html".to_string(),
        "opf" | "ncx" | "svg" | "plist" => "xml".to_string(),
        _ => e,
    });
    ss.find_syntax_by_extension(file)
        .or_else(|| ext.and_then(|e| ss.find_syntax_by_extension(&e)))
        .or_else(|| ss.find_syntax_by_first_line(first_line))
        .filter(|s| s.name != "Plain Text")
}

/// A file's parser where its last piece stopped.
struct Carry {
    key: String,
    at: u64,
    state: ParseState,
    stack: ScopeStack,
    /// Colours stopped (too far in, or too slow): the rest is plain.
    stopped: bool,
}

fn kept() -> &'static Mutex<Vec<Carry>> {
    static KEPT_STATES: Mutex<Vec<Carry>> = Mutex::new(Vec::new());
    &KEPT_STATES
}

/// A piece's colours.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Colours {
    /// (kind, bytes) runs covering the whole piece; None when it's plain.
    pub runs: Option<Vec<(u8, u32)>>,
    /// Colours stopped in this piece; the rest of the file is plain.
    pub stopped: bool,
}

/// Colours `text`, the piece of the file `name` from byte `start`; `next` is where the next piece
/// starts, if there is one. `key` names the file (device, path), to find the state its last piece
/// left.
pub(crate) fn colour(key: &str, name: &str, start: u64, text: &str, next: Option<u64>) -> Colours {
    let ss = syntaxes();
    let carried = {
        let mut kept = kept().lock().unwrap_or_else(|e| e.into_inner());
        kept.iter().position(|c| c.key == key && c.at == start).map(|i| kept.remove(i))
    };
    let (mut state, mut stack) = match carried {
        Some(c) if c.stopped => {
            keep(Carry { at: next.unwrap_or(c.at), ..c }, next.is_some());
            return Colours::default();
        }
        Some(c) => (c.state, c.stack),
        None => {
            // From the start; or, when the state was let go of, from here, which may colour this
            // piece wrong until the next line that starts afresh.
            let first = if start == 0 { text.lines().next().unwrap_or("") } else { "" };
            let Some(syntax) = syntax_for(ss, name, first) else { return Colours::default() };
            (ParseState::new(syntax), ScopeStack::new())
        }
    };
    let deadline = Instant::now() + BUDGET;
    let mut kinds = std::collections::HashMap::<Scope, Option<Kind>>::new();
    let mut runs: Vec<(u8, u32)> = Vec::new();
    let mut push = |kind: Kind, len: usize| {
        if len == 0 {
            return;
        }
        match runs.last_mut() {
            Some((k, n)) if *k == kind as u8 => *n += len as u32,
            _ => runs.push((kind as u8, len as u32)),
        }
    };
    let mut at = 0usize;
    let mut stopped = false;
    for line in text.split_inclusive('\n') {
        if start + at as u64 >= COLOUR_MAX || Instant::now() > deadline {
            stopped = true;
            break;
        }
        if line.len() > LONG_LINE {
            push(Kind::Plain, line.len());
            at += line.len();
            continue;
        }
        let Ok(ops) = state.parse_line(line, ss) else {
            stopped = true;
            break;
        };
        let mut from = 0;
        for (pos, op) in ops {
            if pos > from {
                push(kind_of(&stack, &mut kinds), pos - from);
                from = pos;
            }
            if stack.apply(&op).is_err() {
                stack = ScopeStack::new();
            }
        }
        push(kind_of(&stack, &mut kinds), line.len() - from);
        at += line.len();
    }
    if stopped {
        push(Kind::Plain, text.len() - at);
    }
    if let Some(n) = next {
        keep(Carry { key: key.to_string(), at: n, state, stack, stopped }, true);
    }
    let plain = runs.iter().all(|(k, _)| *k == Kind::Plain as u8);
    Colours { runs: (!plain).then_some(runs), stopped }
}

fn keep(c: Carry, more: bool) {
    if !more {
        return;
    }
    let mut kept = kept().lock().unwrap_or_else(|e| e.into_inner());
    kept.retain(|k| k.key != c.key);
    if kept.len() >= KEPT {
        kept.remove(0);
    }
    kept.push(c);
}

/// The kind of text under `stack`: the innermost scope a rule names.
fn kind_of(stack: &ScopeStack, cache: &mut std::collections::HashMap<Scope, Option<Kind>>) -> Kind {
    for scope in stack.as_slice().iter().rev() {
        let rule = *cache.entry(*scope).or_insert_with(|| {
            let name = scope.build_string();
            RULES.iter().find(|(p, _)| name == *p || name.starts_with(&format!("{p}."))).and_then(|(_, k)| *k)
        });
        if let Some(k) = rule {
            return k;
        }
    }
    Kind::Plain
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text of each run of a kind.
    fn of_kind(text: &str, c: &Colours, kind: Kind) -> Vec<String> {
        let mut at = 0;
        let mut out = vec![];
        for (k, n) in c.runs.as_ref().expect("coloured") {
            let s = &text[at..at + *n as usize];
            if *k == kind as u8 {
                out.push(s.to_string());
            }
            at += *n as usize;
        }
        assert_eq!(at, text.len(), "runs cover the piece");
        out
    }

    #[test]
    fn colours_by_name_extension_and_first_line() {
        let ss = syntaxes();
        assert_eq!(syntax_for(ss, "a/main.rs", "").map(|s| s.name.as_str()), Some("Rust"));
        assert_eq!(syntax_for(ss, "App.TSX", "").map(|s| s.name.as_str()), Some("JavaScript"));
        assert_eq!(syntax_for(ss, "Makefile", "").map(|s| s.name.as_str()), Some("Makefile"));
        assert_eq!(syntax_for(ss, "run", "#!/usr/bin/env python3").map(|s| s.name.as_str()), Some("Python"));
        assert!(syntax_for(ss, "notes.txt", "hello").is_none());
        assert_eq!(colour("t1", "notes.txt", 0, "hello\n", None), Colours::default());
    }

    #[test]
    fn a_comment_goes_on_into_the_next_piece() {
        let a = "fn main() {\n/* one\n";
        let b = "two */ let x = 1;\n}\n";
        let ca = colour("t2", "x.rs", 0, a, Some(a.len() as u64));
        assert_eq!(of_kind(a, &ca, Kind::Keyword), ["fn"]);
        assert_eq!(of_kind(a, &ca, Kind::Comment), ["/* one\n"]);
        let cb = colour("t2", "x.rs", a.len() as u64, b, None);
        assert_eq!(of_kind(b, &cb, Kind::Comment), ["two */"]);
        assert_eq!(of_kind(b, &cb, Kind::Number), ["1"]);
    }

    #[test]
    fn long_lines_stay_plain_and_colours_stop_far_in() {
        let long = format!("let s = \"{}\";\nlet n = 2;\n", "x".repeat(LONG_LINE));
        let c = colour("t3", "x.js", 0, &long, None);
        let first = long.find('\n').unwrap() + 1;
        assert_eq!(c.runs.as_ref().unwrap()[0], (Kind::Plain as u8, first as u32));
        assert_eq!(of_kind(&long, &c, Kind::Number), ["2"]);
        // Past the limit: plain, said once, and later pieces plain too.
        let c = colour("t4", "x.js", COLOUR_MAX, "let n = 2;\n", Some(COLOUR_MAX + 11));
        assert_eq!(c, Colours { runs: None, stopped: true });
        assert_eq!(colour("t4", "x.js", COLOUR_MAX + 11, "let n = 2;\n", None), Colours::default());
    }
}
