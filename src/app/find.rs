//! Finding in the editor's text, and what Replace puts in place of a match: plain text or a
//! regular expression (the `regex` crate's), any case or not, whole words or not.

use super::util::regex_problem;
use regex::{Regex, RegexBuilder};
use std::cell::{OnceCell, RefCell};

/// How the find box's text is matched.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub(super) struct FindOpts {
    /// Upper and lower case are different.
    pub case: bool,
    /// Only where it's a whole word: no letter, digit or _ just before or after.
    pub word: bool,
    /// The text is a regular expression.
    pub regex: bool,
}

/// A group of a match, by number ("0" is the whole match) or name: None when there's no such
/// group, Some(None) when it didn't match anything.
type Group<'a> = &'a dyn Fn(&str) -> Option<Option<String>>;

thread_local! {
    /// The last pattern made, as it's looked for again after each edit.
    static LAST: RefCell<Option<(String, FindOpts, Regex)>> = const { RefCell::new(None) };
}

/// The regex for `q`, or what's wrong with it.
fn compiled(q: &str, o: FindOpts) -> Result<Regex, String> {
    let o = FindOpts { word: false, ..o };
    if let Some(re) = LAST.with_borrow(|l| {
        l.as_ref()
            .filter(|(lq, lo, _)| lq == q && *lo == o)
            .map(|(_, _, re)| re.clone())
    }) {
        return Ok(re);
    }
    let src = if o.regex {
        q.to_string()
    } else {
        regex::escape(q)
    };
    // ^ and $ are at each line's start and end, as in other editors.
    let re = RegexBuilder::new(&src)
        .case_insensitive(!o.case)
        .multi_line(true)
        .build()
        .map_err(|e| regex_problem(&e))?;
    LAST.set(Some((q.to_string(), o, re.clone())));
    Ok(re)
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// No letter, digit or _ just before `a` or just after `b`.
pub(super) fn whole_word(t: &str, a: usize, b: usize) -> bool {
    !t[..a].chars().next_back().is_some_and(is_word) && !t[b..].chars().next().is_some_and(is_word)
}

/// Calls `f` with each match of `q` in `t` (byte ranges, in order, none empty) and its groups,
/// until `f` returns false. Err is what's wrong with a regular expression.
fn each(
    t: &str,
    q: &str,
    o: FindOpts,
    f: &mut dyn FnMut(usize, usize, Group) -> bool,
) -> Result<(), String> {
    if q.is_empty() {
        return Ok(());
    }
    let re = compiled(q, o)?;
    let mut at = 0;
    while at <= t.len() {
        let Some(m) = re.find_at(t, at) else { break };
        let (a, b) = (m.start(), m.end());
        if a == b || (o.word && !whole_word(t, a, b)) {
            // Not one: looked for again from the next character.
            at = a + t[a..].chars().next().map_or(1, char::len_utf8);
            continue;
        }
        // The groups are only worked out when Replace asks for one.
        let caps = OnceCell::new();
        let group = |n: &str| -> Option<Option<String>> {
            if n == "0" {
                return Some(Some(t[a..b].to_string()));
            }
            let c = caps.get_or_init(|| re.captures_at(t, a));
            let c = c.as_ref()?;
            match n.parse::<usize>() {
                Ok(k) => (k < re.captures_len()).then(|| c.get(k).map(|g| g.as_str().to_string())),
                Err(_) => re
                    .capture_names()
                    .flatten()
                    .any(|x| x == n)
                    .then(|| c.name(n).map(|g| g.as_str().to_string())),
            }
        };
        if !f(a, b, &group) {
            break;
        }
        at = b;
    }
    Ok(())
}

/// Where `q` is in `t`, as byte ranges, at most `max` of them.
pub(super) fn find(
    t: &str,
    q: &str,
    o: FindOpts,
    max: usize,
) -> Result<Vec<(usize, usize)>, String> {
    let mut out = vec![];
    each(t, q, o, &mut |a, b, _| {
        out.push((a, b));
        out.len() < max
    })?;
    Ok(out)
}

/// What Replace with puts in place of a match: with a regular expression, $1, $<name>, $& and $$
/// are what they are in JavaScript; with Preserve case, it's in the match's case.
fn replacement(with: &str, o: FindOpts, keep: bool, group: Group) -> String {
    let w = if o.regex {
        expand(with, group)
    } else {
        with.to_string()
    };
    match (keep, group("0").flatten()) {
        (true, Some(m)) => keep_case(&m, &w),
        _ => w,
    }
}

/// What Replace puts in place of the match that starts at byte `a`.
pub(super) fn replacement_at(
    t: &str,
    q: &str,
    o: FindOpts,
    with: &str,
    keep: bool,
    a: usize,
) -> Option<String> {
    let mut out = None;
    let _ = each(t, q, o, &mut |s, _, g| {
        if s == a {
            out = Some(replacement(with, o, keep, g));
        }
        s < a
    });
    out
}

/// `t` with every match of `q` replaced, and how many there were.
pub(super) fn replace_all(
    t: &str,
    q: &str,
    o: FindOpts,
    with: &str,
    keep: bool,
) -> Result<(String, usize), String> {
    let mut out = String::with_capacity(t.len());
    let (mut at, mut n) = (0, 0);
    each(t, q, o, &mut |a, b, g| {
        out.push_str(&t[at..a]);
        out.push_str(&replacement(with, o, keep, g));
        at = b;
        n += 1;
        true
    })?;
    out.push_str(&t[at..]);
    Ok((out, n))
}

/// `with` with $$, $&, $1 to $99 and $<name> put in, as JavaScript's `replace` does.
fn expand(with: &str, group: Group) -> String {
    let mut out = String::with_capacity(with.len());
    let b = with.as_bytes();
    let digit = |i: usize| {
        b.get(i)
            .filter(|c| c.is_ascii_digit())
            .map(|c| (c - b'0') as usize)
    };
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'$' {
            match b.get(i + 1) {
                Some(b'$') => {
                    out.push('$');
                    i += 2;
                    continue;
                }
                Some(b'&') => {
                    out.push_str(&group("0").flatten().unwrap_or_default());
                    i += 2;
                    continue;
                }
                Some(b'<') => {
                    if let Some(end) = with[i + 2..].find('>') {
                        if let Some(g) = group(&with[i + 2..i + 2 + end]) {
                            out.push_str(&g.unwrap_or_default());
                            i += end + 3;
                            continue;
                        }
                    }
                }
                Some(_) => {
                    if let Some(d1) = digit(i + 1) {
                        // Two digits when there's a group with that number, else one.
                        let two = digit(i + 2)
                            .map(|d2| d1 * 10 + d2)
                            .filter(|&n| n > 0)
                            .and_then(|n| group(&n.to_string()).map(|g| (g, 3)));
                        let one = || {
                            (d1 > 0)
                                .then(|| group(&d1.to_string()).map(|g| (g, 2)))
                                .flatten()
                        };
                        if let Some((g, len)) = two.or_else(one) {
                            out.push_str(&g.unwrap_or_default());
                            i += len;
                            continue;
                        }
                    }
                }
                None => {}
            }
        }
        let c = with[i..].chars().next().unwrap_or_default();
        out.push(c);
        i += c.len_utf8().max(1);
    }
    out
}

/// `with` in the case of `matched`: all capitals, all small letters, or a capital first.
pub(super) fn keep_case(matched: &str, with: &str) -> String {
    let letters: Vec<char> = matched.chars().filter(|c| c.is_alphabetic()).collect();
    let Some(&first) = letters.first() else {
        return with.to_string();
    };
    if letters.len() > 1 && letters.iter().all(|c| c.is_uppercase()) {
        with.to_uppercase()
    } else if letters.iter().all(|c| c.is_lowercase()) {
        with.to_lowercase()
    } else if first.is_uppercase() {
        let mut cs = with.chars();
        cs.next()
            .map_or_else(String::new, |c| c.to_uppercase().chain(cs).collect())
    } else {
        with.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: FindOpts = FindOpts {
        case: false,
        word: false,
        regex: false,
    };

    #[test]
    fn plain_text_any_case_or_not() {
        assert_eq!(
            find("Foo foo FOO", "foo", PLAIN, 10).unwrap(),
            [(0, 3), (4, 7), (8, 11)]
        );
        assert_eq!(
            find(
                "Foo foo FOO",
                "foo",
                FindOpts {
                    case: true,
                    ..PLAIN
                },
                10
            )
            .unwrap(),
            [(4, 7)]
        );
        assert_eq!(find("aaa", "a", PLAIN, 2).unwrap().len(), 2);
        assert!(find("abc", "", PLAIN, 10).unwrap().is_empty());
        assert_eq!(
            replace_all("Foo foo FOO bar", "foo", PLAIN, "x", false).unwrap(),
            ("x x x bar".into(), 3)
        );
        assert_eq!(
            replace_all("aaa", "aa", PLAIN, "b", false).unwrap(),
            ("ba".into(), 1)
        );
        // $1 is only put in with a regular expression.
        assert_eq!(
            replace_all("a", "a", PLAIN, "$&", false).unwrap(),
            ("$&".into(), 1)
        );
    }

    #[test]
    fn replacing_keeps_the_case() {
        assert_eq!(
            replace_all("Foo foo FOO", "foo", PLAIN, "bar", true)
                .unwrap()
                .0,
            "Bar bar BAR"
        );
        assert_eq!(keep_case("A", "bar"), "Bar");
        assert_eq!(keep_case("fOO", "bar"), "bar");
        assert_eq!(keep_case("123", "bar"), "bar");
        assert_eq!(keep_case("Élan", "ça"), "Ça");
    }

    #[test]
    fn groups_are_put_in() {
        let g = |n: &str| match n {
            "0" => Some(Some("ab".into())),
            "1" => Some(Some("a".into())),
            "2" => Some(None),
            "x" => Some(Some("X".into())),
            _ => None,
        };
        assert_eq!(
            expand("[$1|$2|$&|$$|$<x>|$<y>|$3|$12|$0|$]", &g),
            "[a||ab|$|X|$<y>|$3|a2|$0|$]"
        );
        assert_eq!(expand("é$1é", &g), "éaé");
    }

    #[test]
    fn words_and_regexes() {
        let word = FindOpts {
            word: true,
            ..PLAIN
        };
        assert_eq!(
            find("a ab a_ (a) ä a", "a", word, 10).unwrap(),
            [(0, 1), (9, 10), (15, 16)]
        );
        let re = FindOpts {
            regex: true,
            ..PLAIN
        };
        assert_eq!(find("x1 y22\nz", r"\d+", re, 10).unwrap(), [(1, 2), (4, 6)]);
        // Empty matches are passed over; ^ is at each line's start.
        assert_eq!(find("ab\nab", "^a?", re, 10).unwrap(), [(0, 1), (3, 4)]);
        assert_eq!(find("ÉTÉ été", "été", PLAIN, 10).unwrap().len(), 2);
        assert!(find("a", "(", re, 10).unwrap_err().contains("unclosed"));
        assert_eq!(
            replace_all(
                "john smith",
                r"(\w+) (?<last>\w+)",
                re,
                "$<last>, $1",
                false
            )
            .unwrap()
            .0,
            "smith, john"
        );
        assert_eq!(
            replacement_at("a1 b2", r"[a-z](\d)", re, "<$1>", false, 3).as_deref(),
            Some("<2>")
        );
        // A regex's special characters are plain text when it's not a regex.
        assert_eq!(find("a.b axb", "a.b", PLAIN, 10).unwrap(), [(0, 3)]);
    }
}
