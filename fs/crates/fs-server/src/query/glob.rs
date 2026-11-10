/// Convert an Everything-style wildcard pattern to a SQLite GLOB pattern.
///
/// Everything uses `*` (zero or more chars) and `?` (one char) - identical
/// to SQLite GLOB, so no character translation is needed.
///
/// `whole_filename = false` wraps with `*...*` so that the pattern matches
/// anywhere within the filename (substring search).
pub fn glob_to_sqlite(pattern: &str, whole_filename: bool) -> String {
    if whole_filename || (pattern.starts_with('*') && pattern.ends_with('*')) {
        pattern.to_owned()
    } else if pattern.starts_with('*') {
        format!("{pattern}*")
    } else if pattern.ends_with('*') {
        format!("*{pattern}")
    } else {
        format!("*{pattern}*")
    }
}

/// Escapes `s` for a `LIKE ... ESCAPE '\\'` pattern, so it matches only itself.
pub fn like_escape(s: &str) -> String {
    to_like(s, false, false).0
}

/// Converts an Everything-style wildcard pattern to a `LIKE ... ESCAPE '\\'`
/// pattern that must match the whole string: `*` becomes `%`, `?` becomes
/// `_`, and literal `%`, `_` and `\` are escaped.
pub fn glob_to_like(pattern: &str) -> String {
    to_like(pattern, true, false).0
}

/// Simple one-character case fold, so folded text keeps its length in
/// characters and stays aligned with SQL's one-character `_`. Going through
/// uppercase first makes `ς` and `σ` fold together.
pub fn fold_char(c: char) -> char {
    fn single(mut it: impl Iterator<Item = char>) -> Option<char> {
        let c = it.next()?;
        it.next().is_none().then_some(c)
    }
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    let upper = single(c.to_uppercase()).unwrap_or(c);
    single(upper.to_lowercase()).unwrap_or(upper)
}

pub fn fold_case(s: &str) -> String {
    s.chars().map(fold_char).collect()
}

/// SQLite's `LIKE` (and the memory store, which mirrors it) folds ASCII
/// only, so a non-ASCII letter with another case would match just itself.
fn has_other_case(c: char) -> bool {
    !c.is_ascii() && (c.to_lowercase().ne([c]) || c.to_uppercase().ne([c]))
}

fn to_like(s: &str, wildcards: bool, widen: bool) -> (String, bool) {
    let mut out = String::with_capacity(s.len() + 4);
    let mut widened = false;
    for c in s.chars() {
        match c {
            '*' if wildcards => out.push('%'),
            '?' if wildcards => out.push('_'),
            '%' | '_' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            _ if widen && has_other_case(c) => {
                out.push('_');
                widened = true;
            }
            _ => out.push(c),
        }
    }
    (out, widened)
}

/// [`like_escape`] for a case-insensitive match: each non-ASCII letter with
/// another case becomes `_`, so the pattern selects a superset. Returns
/// whether that happened, in which case the caller must recheck with
/// [`fold_case`].
pub fn like_escape_nocase(s: &str) -> (String, bool) {
    to_like(s, false, true)
}

/// [`glob_to_like`] widened like [`like_escape_nocase`].
pub fn glob_to_like_nocase(pattern: &str) -> (String, bool) {
    to_like(pattern, true, true)
}

/// Case-sensitive match of `text` against a whole-string `*`/`?` pattern;
/// every other character is literal.
pub fn wildcard_match(text: &str, pattern: &str) -> bool {
    let t: Vec<char> = text.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    let (mut ti, mut pi) = (0, 0);
    // Last `*` seen and the text position it is currently absorbing up to.
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        match p.get(pi) {
            Some('*') => {
                star = Some((pi, ti));
                pi += 1;
            }
            Some(&c) if c == '?' || c == t[ti] => {
                ti += 1;
                pi += 1;
            }
            _ => match star {
                Some((sp, st)) => {
                    pi = sp + 1;
                    ti = st + 1;
                    star = Some((sp, st + 1));
                }
                None => return false,
            },
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// Returns true if the pattern contains `*` or `?` wildcards.
pub fn has_wildcards(s: &str) -> bool {
    s.contains('*') || s.contains('?')
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_to_like_translates_wildcards_and_escapes_the_rest() {
        assert_eq!(glob_to_like("*.rs"), "%.rs");
        assert_eq!(glob_to_like("*/src/?_*"), r"%/src/_\_%");
        assert_eq!(glob_to_like(r"C:\a%*"), r"C:\\a\%%");
    }

    #[test]
    fn wildcard_match_is_case_sensitive_and_whole_string() {
        assert!(wildcard_match("/src/Main.RS", "*.RS"));
        assert!(!wildcard_match("/src/main.rs", "*.RS"));
        assert!(wildcard_match("abc", "a?c"));
        assert!(!wildcard_match("abcd", "a?c"));
        assert!(wildcard_match("a.b.c", "*.*"));
        assert!(wildcard_match("", "*"));
        assert!(!wildcard_match("ab", "a"));
        assert!(wildcard_match("aXbXc", "a*b*c"));
        assert!(wildcard_match("100%_x", "100%_?"));
    }

    #[test]
    fn nocase_patterns_widen_only_non_ascii_letters_with_a_case() {
        assert_eq!(like_escape_nocase("CAFÉ_1"), (r"CAF_\_1".to_owned(), true));
        assert_eq!(like_escape_nocase("plain"), ("plain".to_owned(), false));
        // No case partner, so LIKE already compares it correctly.
        assert_eq!(like_escape_nocase("日本"), ("日本".to_owned(), false));
        assert_eq!(glob_to_like_nocase("*É?.txt"), ("%__.txt".to_owned(), true));
    }

    #[test]
    fn fold_case_keeps_length_and_folds_unicode() {
        assert_eq!(fold_case("CAFÉ"), "café");
        assert_eq!(fold_case("ΣΑΣ"), fold_case("σας"));
        // `ß` uppercases to two letters; it stays as is rather than shifting.
        assert_eq!(fold_case("Straße").chars().count(), 6);
    }

    #[test]
    fn like_escape_protects_only_like_syntax() {
        assert_eq!(like_escape("plain.txt"), "plain.txt");
        assert_eq!(like_escape(r"a_b%c\d"), r"a\_b\%c\\d");
    }

    #[test]
    fn whole_filename_unchanged() {
        assert_eq!(glob_to_sqlite("*.rs", true), "*.rs");
    }

    #[test]
    fn substring_wrapped() {
        assert_eq!(glob_to_sqlite("config", false), "*config*");
    }

    #[test]
    fn already_starred() {
        assert_eq!(glob_to_sqlite("*config*", false), "*config*");
    }

    #[test]
    fn has_wildcards_detects_star() {
        assert!(has_wildcards("*.rs"));
        assert!(!has_wildcards("main"));
    }
}
