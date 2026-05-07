/// Convert an Everything-style wildcard pattern to a SQLite GLOB pattern.
///
/// Everything uses `*` (zero or more chars) and `?` (one char) — identical
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

/// Returns true if the pattern contains `*` or `?` wildcards.
pub fn has_wildcards(s: &str) -> bool {
    s.contains('*') || s.contains('?')
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
