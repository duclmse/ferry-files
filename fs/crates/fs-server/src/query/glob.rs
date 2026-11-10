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
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Converts an Everything-style wildcard pattern to a `LIKE ... ESCAPE '\\'`
/// pattern that must match the whole string: `*` becomes `%`, `?` becomes
/// `_`, and literal `%`, `_` and `\` are escaped.
pub fn glob_to_like(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len() + 4);
    for c in pattern.chars() {
        match c {
            '*' => out.push('%'),
            '?' => out.push('_'),
            '%' | '_' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
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
