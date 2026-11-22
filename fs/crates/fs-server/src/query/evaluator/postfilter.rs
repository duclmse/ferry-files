use std::borrow::Cow;

use nucleo::{Config as NucleoConfig, Matcher, Utf32Str};
use rayon::prelude::*;
use regex::Regex;

use crate::error::{AppError, Result};
use crate::query::glob::{fold_case, wildcard_match};
use crate::store::{FileRecord, FileStore};

use super::types::{Branch, PostFilter};

// ---------------------------------------------------------------------------
// Post-filter application
// ---------------------------------------------------------------------------

pub fn apply_post_filters(
    records: Vec<FileRecord>,
    filters: &[PostFilter],
    store: &FileStore,
) -> Vec<FileRecord> {
    records
        .into_iter()
        .filter(|r| {
            for f in filters {
                if !post_filter_match(f, r, store) {
                    return false;
                }
            }
            true
        })
        .collect()
}

fn post_filter_match(f: &PostFilter, r: &FileRecord, store: &FileStore) -> bool {
    match f {
        PostFilter::Regex { re, on_path } => re.is_match(if *on_path { &r.path } else { &r.name }),
        PostFilter::Child(name) => store
            .has_child(&r.path, &crate::query::glob::like_escape(name))
            .unwrap_or(false),
        PostFilter::WholeWord(w, cs) => is_whole_word(&folded(&r.name, *cs), &folded(w, *cs)),
        PostFilter::Literal {
            text,
            whole,
            on_path,
            case_sensitive,
        } => {
            let hay = folded(if *on_path { &r.path } else { &r.name }, *case_sensitive);
            let text = folded(text, *case_sensitive);
            if *whole {
                hay == text
            } else {
                hay.contains(&*text)
            }
        }
        PostFilter::ArchiveGlob {
            pattern,
            case_sensitive,
        } => r.archive_path.as_deref().is_some_and(|a| {
            wildcard_match(
                &folded(a, *case_sensitive),
                &folded(pattern, *case_sensitive),
            )
        }),
        PostFilter::Glob {
            pattern,
            on_path,
            case_sensitive,
        } => {
            let hay = if *on_path { &r.path } else { &r.name };
            wildcard_match(
                &folded(hay, *case_sensitive),
                &folded(pattern, *case_sensitive),
            )
        }
        PostFilter::StartWith {
            text,
            case_sensitive,
        } => folded(&r.name, *case_sensitive).starts_with(&*folded(text, *case_sensitive)),
        PostFilter::EndWith {
            text,
            case_sensitive,
        } => folded(&r.name, *case_sensitive).ends_with(&*folded(text, *case_sensitive)),
        PostFilter::ArchiveExclude => r.archive_path.is_none(),
        PostFilter::Any(branches) => branches.iter().any(|b| branch_match(b, r, store)),
        PostFilter::Not(b) => !branch_match(b, r, store),
    }
}

fn folded(s: &str, case_sensitive: bool) -> Cow<'_, str> {
    if case_sensitive {
        Cow::Borrowed(s)
    } else {
        Cow::Owned(fold_case(s))
    }
}

fn branch_match(b: &Branch, r: &FileRecord, store: &FileStore) -> bool {
    b.sql.iter().all(|c| c.matches(r)) && b.post.iter().all(|f| post_filter_match(f, r, store))
}

fn is_whole_word(text: &str, word: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let word_chars: Vec<char> = word.chars().collect();
    if word_chars.is_empty() {
        return false;
    }
    let wlen = word_chars.len();
    for i in 0..=chars.len().saturating_sub(wlen) {
        let slice: Vec<char> = chars[i..i + wlen].to_vec();
        if slice == word_chars {
            let before_ok = i == 0 || !chars[i - 1].is_alphanumeric();
            let after_ok = i + wlen == chars.len() || !chars[i + wlen].is_alphanumeric();
            if before_ok && after_ok {
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Fuzzy scoring with nucleo
// ---------------------------------------------------------------------------

/// Scores each candidate name against `term` in `0.0..=1.0` and keeps those
/// at or above `threshold`, best first.
///
/// The score is the better of two signals:
/// - nucleo's subsequence score, normalised by the term's score against
///   itself. nucleo scores grow with needle length, so a fixed divisor made
///   short terms unreachable for any sensible threshold.
/// - a typo similarity, `1 - d / len(term)`, where `d` is the smallest
///   optimal-string-alignment distance between the term and any window of
///   the name. nucleo never matches out-of-order letters, so without this
///   `mian` could not find `main.rs`.
///
/// Returns indices into `names`, so callers can score borrowed names and
/// materialize only the winning rows.
pub(super) fn fuzzy_rank(names: &[&str], term: &str, threshold: f32, limit: usize) -> Vec<usize> {
    // nucleo ignores case only in the haystack and wants the needle
    // lowercase already: a capital in it matched nothing, and on some names
    // tripped an assertion inside nucleo (`fuzzy:Ad` against `AA_bab`).
    let term = term.to_lowercase();
    let term_lower: Vec<char> = term.chars().collect();
    if term_lower.is_empty() {
        return Vec::new();
    }

    let mut needle_buf = Vec::new();
    let needle = Utf32Str::new(&term, &mut needle_buf);
    let mut self_buf = Vec::new();
    let self_score = Matcher::new(NucleoConfig::DEFAULT)
        .fuzzy_match(Utf32Str::new(&term, &mut self_buf), needle)
        .unwrap_or(0)
        .max(1) as f32;

    let term_bytes = term.is_ascii().then_some(term.as_bytes());
    let mut scored: Vec<(f32, usize)> = names
        .par_iter()
        .enumerate()
        .map_init(
            || {
                (
                    Matcher::new(NucleoConfig::DEFAULT),
                    Vec::new(),
                    Scratch::default(),
                )
            },
            |(matcher, hay_buf, scratch), (i, name)| {
                let subseq = matcher
                    .fuzzy_match(Utf32Str::new(name, hay_buf), needle)
                    .map_or(0.0, |s| (s as f32 / self_score).min(1.0));
                let typo = match term_bytes {
                    // Most names are ASCII, where lowercasing is per byte and
                    // the distance can run on bytes without collecting chars.
                    Some(t) if name.is_ascii() => {
                        scratch.bytes.clear();
                        scratch
                            .bytes
                            .extend(name.bytes().map(|b| b.to_ascii_lowercase()));
                        typo_similarity(t, &scratch.bytes, &mut scratch.cells)
                    }
                    _ => {
                        scratch.chars.clear();
                        scratch.chars.extend(name.to_lowercase().chars());
                        typo_similarity(&term_lower, &scratch.chars, &mut scratch.cells)
                    }
                };
                (subseq.max(typo), i)
            },
        )
        .filter(|(s, _)| *s >= threshold)
        .collect();

    // Ties keep candidate order so results are deterministic across runs.
    scored.sort_unstable_by(|(a, i), (b, j)| b.total_cmp(a).then(i.cmp(j)));
    scored.into_iter().take(limit).map(|(_, i)| i).collect()
}

/// Rejects only names [`fuzzy_rank`] scores below the threshold, at a few
/// nanoseconds a name, so every name in a large index can be tested and the
/// candidate cap applies to names that could match, not to whichever rows a
/// store reads first. A name passes if either signal could reach it:
/// - nucleo scores only a name holding the term as a subsequence, ignoring
///   case;
/// - a typo similarity of at least the threshold allows at most `k` edits
///   between the term and a window of the name at most one longer than it.
///   A substitution or deletion loses one of the term's letters and an
///   insertion or transposition loses none, so some `len + 1` bytes of the
///   name hold all but `k` of them, counted with repeats.
///
/// A non-ASCII term or name always passes: case folding and nucleo's
/// normalization are not byte comparisons there.
pub(super) struct FuzzyPrefilter {
    /// The term in ASCII lowercase; `None` when it is not ASCII.
    term: Option<Vec<u8>>,
    /// How often each byte occurs in the term.
    counts: [u32; 128],
    /// Term letters a name must hold for the typo signal to reach the
    /// threshold; `usize::MAX` when no number of edits can.
    need: usize,
    /// [`letter_bits`] of the term, and how many of them a name must share.
    bits: u64,
    need_bits: u32,
}

/// A bit per lowercase letter and digit, other bytes sharing the rest.
const LETTER_BITS: [u64; 256] = {
    let mut t = [0u64; 256];
    let mut b = 0;
    while b < 256 {
        let lower = (b as u8).to_ascii_lowercase();
        t[b] = match lower {
            b'a'..=b'z' => 1 << (lower - b'a'),
            b'0'..=b'9' => 1 << (26 + lower - b'0'),
            _ => 1 << (36 + lower % 28),
        };
        b += 1;
    }
    t
};

fn letter_bits(text: &[u8]) -> u64 {
    text.iter().fold(0, |m, &b| m | LETTER_BITS[usize::from(b)])
}

impl FuzzyPrefilter {
    pub(super) fn new(term: &str, threshold: f32) -> Self {
        let n = term.chars().count();
        // The same arithmetic as `typo_similarity`, so the two agree at the
        // boundary.
        let max_edits = (0..=n)
            .rev()
            .find(|&d| n > 0 && 1.0 - d as f32 / n as f32 >= threshold);
        let need = max_edits.map_or(usize::MAX, |k| n - k);
        let term = term
            .is_ascii()
            .then(|| term.to_ascii_lowercase().into_bytes());
        let mut counts = [0u32; 128];
        for &b in term.iter().flatten() {
            counts[usize::from(b)] += 1;
        }
        // Each edit removes at most one occurrence of one letter, so a name
        // within `k` edits lacks at most `k` of the term's letters entirely,
        // and a subsequence lacks none. Bits shared by several letters
        // only make this weaker.
        let bits = term.as_deref().map_or(0, letter_bits);
        let need_bits = bits
            .count_ones()
            .saturating_sub(max_edits.map_or(0, |k| k as u32));
        Self {
            term,
            counts,
            need,
            bits,
            need_bits,
        }
    }

    pub(super) fn could_match(&self, name: &str) -> bool {
        let Some(term) = &self.term else { return true };
        if !name.is_ascii() || self.need == 0 {
            return true;
        }
        let name = name.as_bytes();
        // Rejects most names for the price of one table lookup a byte; the
        // window below zeroes a 512-byte table per name.
        if (letter_bits(name) & self.bits).count_ones() < self.need_bits {
            return false;
        }
        let width = term.len() + 1;
        let mut next = 0;
        // Term letters, with repeats, among the last `width` bytes.
        let mut window = [0u32; 128];
        let mut held = 0;
        for (i, b) in name.iter().map(u8::to_ascii_lowercase).enumerate() {
            if next < term.len() && term[next] == b {
                next += 1;
                if next == term.len() {
                    return true;
                }
            }
            if i >= width {
                let out = usize::from(name[i - width].to_ascii_lowercase());
                window[out] -= 1;
                if window[out] < self.counts[out] {
                    held -= 1;
                }
            }
            let b = usize::from(b);
            if window[b] < self.counts[b] {
                held += 1;
            }
            window[b] += 1;
            if held >= self.need {
                return true;
            }
        }
        false
    }
}

/// Per-thread buffers for [`fuzzy_rank`], so scoring a name allocates nothing.
#[derive(Default)]
struct Scratch {
    bytes: Vec<u8>,
    chars: Vec<char>,
    cells: Vec<usize>,
}

/// `1 - d / len(term)` for the closest window of `name` (window lengths
/// `len(term) ± 1`, so one insertion or deletion is reachable).
///
/// One alignment per start position: the last row of the term against the
/// longest window there holds the distances to the shorter windows too.
/// Aligning each window on its own, into a fresh matrix, was most of a
/// `fuzzy:` query's time once candidates reached the tens of thousands.
fn typo_similarity<T: Copy + PartialEq>(term: &[T], name: &[T], cells: &mut Vec<usize>) -> f32 {
    let n = term.len();
    let shortest = n.saturating_sub(1).max(1);
    let longest = n + 1;
    let mut best = n;
    if name.len() >= shortest {
        for start in 0..=name.len() - shortest {
            let window = &name[start..name.len().min(start + longest)];
            let row = osa_last_row(term, window, cells);
            if let Some(d) = row.get(shortest..).and_then(|r| r.iter().min()) {
                best = best.min(*d);
            }
            if best == 0 {
                return 1.0;
            }
        }
    }
    1.0 - best as f32 / n as f32
}

/// Optimal string alignment distances from `a` to every prefix of `b`.
fn osa_last_row<'c, T: Copy + PartialEq>(a: &[T], b: &[T], d: &'c mut Vec<usize>) -> &'c [usize] {
    let cols = b.len() + 1;
    d.clear();
    d.resize((a.len() + 1) * cols, 0);
    for i in 0..=a.len() {
        d[i * cols] = i;
    }
    for (j, cell) in d.iter_mut().enumerate().take(cols) {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut v = (d[(i - 1) * cols + j] + 1)
                .min(d[i * cols + j - 1] + 1)
                .min(d[(i - 1) * cols + j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(d[(i - 2) * cols + j - 2] + 1);
            }
            d[i * cols + j] = v;
        }
    }
    &d[a.len() * cols..]
}

/// The previous [`typo_similarity`], one fresh alignment per window, kept
/// to check the faster one against.
#[cfg(test)]
fn typo_similarity_reference(term: &[char], name: &[char]) -> f32 {
    let n = term.len();
    let mut best = n;
    for w in n.saturating_sub(1).max(1)..=n + 1 {
        if w > name.len() {
            break;
        }
        for window in name.windows(w) {
            best = best.min(osa_distance(term, window));
            if best == 0 {
                return 1.0;
            }
        }
    }
    1.0 - best as f32 / n as f32
}

/// Optimal string alignment distance: Levenshtein plus adjacent transposition.
#[cfg(test)]
fn osa_distance(a: &[char], b: &[char]) -> usize {
    let cols = b.len() + 1;
    let mut d = vec![0usize; (a.len() + 1) * cols];
    for i in 0..=a.len() {
        d[i * cols] = i;
    }
    for (j, cell) in d.iter_mut().enumerate().take(cols) {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut v = (d[(i - 1) * cols + j] + 1)
                .min(d[i * cols + j - 1] + 1)
                .min(d[(i - 1) * cols + j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(d[(i - 2) * cols + j - 2] + 1);
            }
            d[i * cols + j] = v;
        }
    }
    d[a.len() * cols + b.len()]
}

// ---------------------------------------------------------------------------
// Regex helper
// ---------------------------------------------------------------------------

pub(super) fn build_regex(pattern: &str, case_sensitive: bool) -> Result<Regex> {
    let re = if case_sensitive {
        Regex::new(pattern)
    } else {
        Regex::new(&format!("(?i){pattern}"))
    };
    re.map_err(|e| AppError::Query(format!("invalid regex '{pattern}': {e}")))
}

#[cfg(test)]
mod fuzzy_tests {
    use super::*;

    fn names(term: &str, threshold: f32) -> Vec<String> {
        let c = ["main.rs", "lib.rs", "Cargo.toml", "domain_model.rs"];
        fuzzy_rank(&c, term, threshold, 10)
            .into_iter()
            .map(|i| c[i].to_owned())
            .collect()
    }

    /// Every name `fuzzy_rank` keeps passes the prefilter, across typos,
    /// transpositions, case, repeats and thresholds.
    #[test]
    fn the_prefilter_keeps_every_name_the_ranker_keeps() {
        let names = [
            "main.rs",
            "mian.rs",
            "MAIN.RS",
            "domain_model.rs",
            "report_notes_12.txt",
            "reprot.pdf",
            "rpeort",
            "Draft_Index_9.md",
            "notes.txt",
            "aaab",
            "abab",
            "baaa",
            "x",
            "",
            "résumé.doc",
            "lib.rs",
            "Cargo.toml",
            "readme",
            "README.md",
            "ReAdMe",
            "config.json",
            "cnofig.json",
            "confg",
            "conf",
            "data_backup_2024.zip",
        ];
        for term in [
            "mian", "reprot", "aab", "readme", "config", "x", "ab", "résumé", "zzqx",
        ] {
            for threshold in [0.0, 0.3, 0.5, 0.7, 0.9, 1.0] {
                let filter = FuzzyPrefilter::new(term, threshold);
                for i in fuzzy_rank(&names, term, threshold, usize::MAX) {
                    assert!(
                        filter.could_match(names[i]),
                        "{term:?} at {threshold} keeps {:?}",
                        names[i]
                    );
                }
            }
        }
    }

    /// Random short names and terms over a few letters, where typos,
    /// repeats and windows collide far more often than in real names.
    #[test]
    fn the_prefilter_never_drops_a_random_match() {
        let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        // The second alphabet spreads over the letter, digit and shared bits
        // of the letter mask.
        for alphabet in [&b"abcdA_."[..], b"aqZ019_.-~#"] {
            let mut word = |max: u64| -> String {
                let len = 1 + next(max);
                (0..len)
                    .map(|_| alphabet[next(alphabet.len() as u64) as usize] as char)
                    .collect()
            };
            let names: Vec<String> = (0..400).map(|_| word(12)).collect();
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            for _ in 0..200 {
                let term = word(6);
                for threshold in [0.4, 0.6, 0.7, 0.8] {
                    let filter = FuzzyPrefilter::new(&term, threshold);
                    for i in fuzzy_rank(&names, &term, threshold, usize::MAX) {
                        assert!(
                            filter.could_match(names[i]),
                            "{term:?} at {threshold} keeps {:?}",
                            names[i]
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn typo_similarity_agrees_with_aligning_each_window() {
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let letters: Vec<char> = "abcab_é".chars().collect();
        let mut word = |max: u64| -> Vec<char> {
            (0..next(max + 1))
                .map(|_| letters[next(letters.len() as u64) as usize])
                .collect()
        };
        let mut cells = Vec::new();
        for _ in 0..20_000 {
            let term = word(7);
            if term.is_empty() {
                continue;
            }
            let name = word(14);
            let want = typo_similarity_reference(&term, &name);
            assert_eq!(
                typo_similarity(&term, &name, &mut cells),
                want,
                "{term:?} {name:?}"
            );
            let (t, n): (String, String) = (term.iter().collect(), name.iter().collect());
            if t.is_ascii() && n.is_ascii() {
                let got = typo_similarity(t.as_bytes(), n.as_bytes(), &mut cells);
                assert_eq!(got, want, "{t:?} {n:?} as bytes");
            }
        }
    }

    #[test]
    fn a_term_with_capitals_scores_like_its_lowercase() {
        let c = ["main.rs", "A_b", "MAIN.RS"];
        assert_eq!(
            fuzzy_rank(&c, "Main", 0.7, 10),
            fuzzy_rank(&c, "main", 0.7, 10)
        );
        // nucleo asserted on this pair: it wants a lowercase needle.
        assert_eq!(fuzzy_rank(&["AA_bab.dbcb"], "Ad", 0.0, 10), [0]);
    }

    #[test]
    fn the_prefilter_rejects_names_far_from_the_term() {
        let filter = FuzzyPrefilter::new("reprot", 0.7);
        assert!(filter.could_match("report_notes.txt"), "a subsequence");
        assert!(filter.could_match("rpeort"), "a transposition");
        assert!(!filter.could_match("Draft_Index_9.md"));
        assert!(!filter.could_match("image.jpg"));
        assert!(
            !FuzzyPrefilter::new("mian", 0.7).could_match("aaaa_12.bin"),
            "the letters are there, but not within one window"
        );
        assert!(FuzzyPrefilter::new("reprot", 0.0).could_match("image.jpg"));
    }

    #[test]
    fn transposed_term_finds_main_at_default_threshold() {
        assert_eq!(
            names("mian", 0.7).first().map(String::as_str),
            Some("main.rs")
        );
    }

    #[test]
    fn exact_short_term_passes_default_threshold() {
        assert_eq!(
            names("main", 0.7).first().map(String::as_str),
            Some("main.rs")
        );
        assert_eq!(names("lib", 0.7), vec!["lib.rs"]);
    }

    #[test]
    fn unrelated_term_is_filtered() {
        assert!(names("zzqx", 0.7).is_empty());
    }

    #[test]
    fn osa_counts_transposition_as_one() {
        let a: Vec<char> = "mian".chars().collect();
        let b: Vec<char> = "main".chars().collect();
        assert_eq!(osa_distance(&a, &b), 1);
    }
}
