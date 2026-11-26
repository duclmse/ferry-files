//! What the palette's `:` sends fsd: the text as typed, in fsd's search syntax, with the Match
//! case, Whole word and Regular expression buttons turned into fsd's modifiers, or the text to
//! find inside files. This reads the
//! text the way fsd's parser does (fs-server's `query/parser`), only as far as it needs to.

use super::find::FindOpts;

/// The query for fsd. With Regular expression on, the whole text is one regular expression
/// (as in the Filter box), and Whole word puts `\b` around it, as fsd's `ww:` doesn't apply to
/// regular expressions. Otherwise each plain word or "quoted phrase" gets `case:` and `ww:`
/// unless it says otherwise itself (`nocase:`, `noww:` …); functions (`ext:`, `size:` …) are
/// left as typed.
pub(super) fn fsd_query(text: &str, o: FindOpts) -> String {
    let text = text.trim();
    if text.is_empty() {
        return String::new();
    }
    if o.regex {
        // fsd's quotes can't hold a quote; \x22 is the same character to the regex.
        let p = text.replace('"', r"\x22");
        let p = if o.word { format!(r"\b(?:{p})\b") } else { p };
        let case = if o.case { "case: " } else { "" };
        return format!("{case}regex: \"{p}\"");
    }
    if !o.case && !o.word {
        return text.to_string();
    }
    let toks = tokens(text);
    // Where to put each term's modifiers, and what they are.
    let mut adds: Vec<(usize, &'static str)> = vec![];
    let mut i = 0;
    while i < toks.len() {
        let start = toks[i].start;
        // The modifiers that come before the term, and whether it's a regular expression.
        let (mut has_case, mut has_word, mut regex) = (false, false, false);
        // A term (plain word or phrase) the modifiers go on, or none (a function, or
        // modifiers with nothing after them).
        let mut term = false;
        while i < toks.len() {
            let t = &toks[i];
            i += 1;
            match t.kind {
                Kind::Op(_) => {
                    i -= 1;
                    break;
                }
                Kind::Phrase => {
                    term = true;
                    break;
                }
                Kind::Word => {}
            }
            let mut w = &text[t.start..t.end];
            let mut chained = false;
            loop {
                let Some(colon) = w.find(':') else {
                    term = !w.is_empty() || !chained;
                    break;
                };
                let kw = w[..colon].to_ascii_lowercase();
                let val = &w[colon + 1..];
                match modifier(&kw, val) {
                    Some(m) => {
                        has_case |= m == Mod::Case;
                        has_word |= m == Mod::Word;
                        regex |= m == Mod::Regex;
                        if val.is_empty() {
                            chained = true;
                            break;
                        }
                        w = val;
                    }
                    None => {
                        // A function. `regex:…` follows match case; the others are left alone.
                        if kw == "regex" {
                            regex = true;
                            term = true;
                        } else if val.is_empty() {
                            // fsd reads `size:>1mb` as `size:`, `>` and `1mb`.
                            if let Some(Kind::Op('<' | '>')) = toks.get(i).map(|t| t.kind) {
                                i += 1;
                                if let Some(Kind::Word) = toks.get(i).map(|t| t.kind) {
                                    i += 1;
                                }
                            }
                        }
                        chained = false;
                        break;
                    }
                }
            }
            if term || !chained {
                break;
            }
        }
        if term {
            if o.case && !has_case {
                adds.push((start, "case: "));
            }
            if o.word && !has_word && !regex {
                adds.push((start, "ww: "));
            }
        }
        // An operator on its own: move past it.
        if i < toks.len() && toks[i].start == start {
            i += 1;
        }
    }
    let mut out = String::with_capacity(text.len() + adds.len() * 6);
    let mut at = 0;
    for (pos, add) in adds {
        out.push_str(&text[at..pos]);
        out.push_str(add);
        at = pos;
    }
    out.push_str(&text[at..]);
    out
}

/// The query for files whose text (or name) holds every word of `text`, in any case, from fsd's
/// content index. fsd hands `content:`'s value to its full-text index, which reads `+word` as a
/// word that must be there and keeps only letters and digits in words, so only those are sent:
/// `fix: the "bug"` asks for the words fix, the and bug.
pub(super) fn content_query(text: &str) -> String {
    let words: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| format!("+{w}"))
        .collect();
    if words.is_empty() {
        return String::new();
    }
    format!("content:\"{}\"", words.join(" "))
}

/// fsd's functions and type macros (`ext:pdf`, `size:>1mb`, `pic:`), and the modifiers that
/// are functions with a value (`path:src`, `regex:a.b`), as fs's query-language reference lists them.
const FUNCTIONS: &[&str] = &[
    "dm",
    "datemodified",
    "dc",
    "datecreated",
    "da",
    "dateaccessed",
    "rc",
    "recentchange",
    "dr",
    "daterun",
    "size",
    "ext",
    "type",
    "parent",
    "infolder",
    "nosubfolders",
    "dir",
    "child",
    "startwith",
    "startswith",
    "endwith",
    "endswith",
    "len",
    "length",
    "depth",
    "parents",
    "root",
    "attrib",
    "content",
    "utf8content",
    "ansicontent",
    "childcount",
    "childfilecount",
    "childfoldercount",
    "empty",
    "dupe",
    "namedupe",
    "namepartdupe",
    "sizedupe",
    "dmdupe",
    "dcdupe",
    "dadupe",
    "attribdupe",
    "count",
    "sort",
    "archiveof",
    "fuzzy",
    "audio",
    "zip",
    "doc",
    "exe",
    "pic",
    "video",
    "path",
    "regex",
    "archive",
    "noarchive",
];

/// Whether `text`, typed where a path goes, is a search in fsd's syntax instead: it has a
/// wildcard (* ?), an operator (| ! < >), a "quoted phrase", or a modifier or function fsd
/// knows (`ext:pdf`, `size:>1mb`, `file:`). Plain words aren't: they name a folder.
pub(in crate::app) fn is_search(text: &str) -> bool {
    tokens(text).iter().any(|t| match t.kind {
        Kind::Op(_) | Kind::Phrase => true,
        Kind::Word => {
            let w = &text[t.start..t.end];
            w.contains(['*', '?'])
                || w.split_once(':').is_some_and(|(kw, val)| {
                    let kw = kw.to_ascii_lowercase();
                    modifier(&kw, val).is_some() || FUNCTIONS.contains(&kw.as_str())
                })
        }
    })
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Word,
    Phrase,
    Op(char),
}

struct Tok {
    kind: Kind,
    /// Byte offsets in the text; a phrase starts at its opening quote.
    start: usize,
    end: usize,
}

/// fsd's tokens: words (where quotes keep spaces in, as in `path:"a b"`), "phrases", and the
/// operators | ! < >.
fn tokens(text: &str) -> Vec<Tok> {
    let mut out = vec![];
    let mut it = text.char_indices().peekable();
    while let Some((start, c)) = it.next() {
        match c {
            c if c.is_whitespace() => {}
            '|' | '!' | '<' | '>' => out.push(Tok {
                kind: Kind::Op(c),
                start,
                end: start + 1,
            }),
            '"' => {
                let mut end = text.len();
                for (j, c) in it.by_ref() {
                    if c == '"' {
                        end = j + 1;
                        break;
                    }
                }
                out.push(Tok {
                    kind: Kind::Phrase,
                    start,
                    end,
                });
            }
            _ => {
                // A quote inside a word keeps spaces and operators in it, up to the next one.
                let mut quoted = c == '"';
                let mut end = start + c.len_utf8();
                while let Some(&(j, c)) = it.peek() {
                    if !quoted && (c.is_whitespace() || matches!(c, '|' | '<' | '>')) {
                        break;
                    }
                    quoted ^= c == '"';
                    end = j + c.len_utf8();
                    it.next();
                }
                out.push(Tok {
                    kind: Kind::Word,
                    start,
                    end,
                });
            }
        }
    }
    out
}

#[derive(PartialEq)]
enum Mod {
    Case,
    Word,
    Regex,
    Other,
}

/// The modifier `kw:` is, as fsd's `modifier_from_kw` reads it; None for a function.
fn modifier(kw: &str, val: &str) -> Option<Mod> {
    if !val.is_empty() && (kw.len() == 1 || matches!(kw, "dir" | "regex" | "archive" | "noarchive"))
    {
        return None;
    }
    Some(match kw {
        "cs" | "case" | "i" | "nocase" => Mod::Case,
        "w" | "wholeword" | "wholewords" | "ww" | "nowholeword" | "nowholewords" | "noww" => {
            Mod::Word
        }
        "regex" | "noregex" => Mod::Regex,
        "path" | "nopath" | "wfn" | "wholefilename" | "nowfn" | "nowholefilename" | "file"
        | "files" | "folder" | "folders" | "dir" | "archive" | "noarchive" | "wildcards"
        | "nowildcards" | "ascii" | "utf8" | "noascii" | "diacritics" | "nodiacritics" => {
            Mod::Other
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CASE: FindOpts = FindOpts {
        case: true,
        word: false,
        regex: false,
    };
    const BOTH: FindOpts = FindOpts {
        case: true,
        word: true,
        regex: false,
    };

    #[test]
    fn off_sends_the_text_as_typed() {
        assert_eq!(fsd_query("  a b|c  ", FindOpts::default()), "a b|c");
        assert_eq!(fsd_query("   ", BOTH), "");
    }

    #[test]
    fn terms_get_the_modifiers() {
        assert_eq!(fsd_query("Report 2024", CASE), "case: Report case: 2024");
        assert_eq!(fsd_query("a|!b", BOTH), "case: ww: a|!case: ww: b");
        assert_eq!(fsd_query("<a b>", CASE), "<case: a case: b>");
        assert_eq!(fsd_query("\"my file\"", BOTH), "case: ww: \"my file\"");
        assert_eq!(fsd_query("path:src", CASE), "case: path:src");
        assert_eq!(fsd_query("path: \"a b\"", CASE), "case: path: \"a b\"");
        assert_eq!(
            fsd_query("path:\"a b|c\" d", CASE),
            "case: path:\"a b|c\" case: d"
        );
    }

    #[test]
    fn what_the_text_says_wins() {
        assert_eq!(fsd_query("nocase:Foo bar", CASE), "nocase:Foo case: bar");
        assert_eq!(fsd_query("noww: foo", BOTH), "case: noww: foo");
        assert_eq!(fsd_query("regex: a.b", BOTH), "case: regex: a.b");
    }

    #[test]
    fn functions_are_left_alone() {
        assert_eq!(
            fsd_query("ext:pdf size:>10mb cv", BOTH),
            "ext:pdf size:>10mb case: ww: cv"
        );
        assert_eq!(fsd_query("dm:today", CASE), "dm:today");
        assert_eq!(fsd_query("regex:^a", BOTH), "case: regex:^a");
        // A modifier with nothing after it filters on its own.
        assert_eq!(fsd_query("folder: | a", CASE), "folder: | case: a");
    }

    #[test]
    fn content_asks_for_every_word() {
        assert_eq!(
            content_query("fix: the \"bug\""),
            "content:\"+fix +the +bug\""
        );
        assert_eq!(content_query("Größe 2024"), "content:\"+Größe +2024\"");
        assert_eq!(content_query(" :;\" "), "");
    }

    #[test]
    fn a_regex_is_the_whole_text() {
        let re = FindOpts {
            regex: true,
            ..Default::default()
        };
        assert_eq!(fsd_query("^a|b$", re), "regex: \"^a|b$\"");
        let all = FindOpts {
            regex: true,
            ..BOTH
        };
        assert_eq!(fsd_query("a\"b", all), "case: regex: \"\\b(?:a\\x22b)\\b\"");
    }

    #[test]
    fn searches_are_told_from_paths() {
        for s in [
            "*.pdf",
            "report?.txt",
            "ext:pdf",
            "EXT:jpg;png",
            "size:>1mb",
            "pic: holiday",
            "file: notes",
            "path:src main",
            "dm:today",
            "a|b",
            "!node_modules",
            "<a b>",
            "\"my notes\"",
            "/Users/me *.rs",
        ] {
            assert!(is_search(s), "{s}");
        }
        for s in [
            "/Users/me/Documents",
            "/Users/me/My Notes",
            "Documents",
            "../Downloads",
            "/C:/Users",
            "/Volumes/Backup (old)",
            "/tmp/a:b",
            "",
        ] {
            assert!(!is_search(s), "{s}");
        }
    }
}
