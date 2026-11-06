use nucleo::{Config as NucleoConfig, Matcher, Utf32Str};
use rayon::prelude::*;
use regex::Regex;

use crate::error::{AppError, Result};
use crate::store::{FileRecord, FileStore};

use super::types::{Branch, PostFilter};

// ---------------------------------------------------------------------------
// Post-filter application
// ---------------------------------------------------------------------------

pub fn apply_post_filters(
    records: Vec<FileRecord>,
    filters: &[PostFilter],
    store:   &FileStore,
) -> Vec<FileRecord> {
    records.into_iter().filter(|r| {
        for f in filters {
            if !post_filter_match(f, r, store) {
                return false;
            }
        }
        true
    }).collect()
}

fn post_filter_match(f: &PostFilter, r: &FileRecord, store: &FileStore) -> bool {
    match f {
        PostFilter::Regex(re, _cs) => re.is_match(&r.path),
        PostFilter::Child(name)    => store.has_child(&r.path, name).unwrap_or(false),
        PostFilter::WholeWord(w, cs) => {
            let text = if *cs { r.name.clone() } else { r.name.to_ascii_lowercase() };
            let word = if *cs { w.clone()      } else { w.to_ascii_lowercase()      };
            is_whole_word(&text, &word)
        }
        PostFilter::StartWith(s) => r.name.starts_with(s.as_str()),
        PostFilter::EndWith(s)   => r.name.ends_with(s.as_str()),
        PostFilter::ArchiveExclude => r.archive_path.is_none(),
        PostFilter::Any(branches)  => branches.iter().any(|b| branch_match(b, r, store)),
        PostFilter::Not(b)         => !branch_match(b, r, store),
    }
}

fn branch_match(b: &Branch, r: &FileRecord, store: &FileStore) -> bool {
    b.sql.iter().all(|c| c.matches(r)) && b.post.iter().all(|f| post_filter_match(f, r, store))
}

fn is_whole_word(text: &str, word: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let word_chars: Vec<char> = word.chars().collect();
    if word_chars.is_empty() { return false; }
    let wlen = word_chars.len();
    for i in 0..=chars.len().saturating_sub(wlen) {
        let slice: Vec<char> = chars[i..i + wlen].to_vec();
        if slice == word_chars {
            let before_ok = i == 0 || !chars[i - 1].is_alphanumeric();
            let after_ok  = i + wlen == chars.len() || !chars[i + wlen].is_alphanumeric();
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
pub(super) fn fuzzy_rank(
    names:     &[&str],
    term:      &str,
    threshold: f32,
    limit:     usize,
) -> Vec<usize> {
    let term_lower: Vec<char> = term.to_lowercase().chars().collect();
    if term_lower.is_empty() {
        return Vec::new();
    }

    let mut needle_buf = Vec::new();
    let needle = Utf32Str::new(term, &mut needle_buf);
    let mut self_buf = Vec::new();
    let self_score = Matcher::new(NucleoConfig::DEFAULT)
        .fuzzy_match(Utf32Str::new(term, &mut self_buf), needle)
        .unwrap_or(0)
        .max(1) as f32;

    let mut scored: Vec<(f32, usize)> = names
        .par_iter()
        .enumerate()
        .map_init(
            || (Matcher::new(NucleoConfig::DEFAULT), Vec::new()),
            |(matcher, hay_buf), (i, name)| {
                let subseq = matcher
                    .fuzzy_match(Utf32Str::new(name, hay_buf), needle)
                    .map_or(0.0, |s| (s as f32 / self_score).min(1.0));
                let lower: Vec<char> = name.to_lowercase().chars().collect();
                (subseq.max(typo_similarity(&term_lower, &lower)), i)
            },
        )
        .filter(|(s, _)| *s >= threshold)
        .collect();

    // Ties keep candidate order so results are deterministic across runs.
    scored.sort_unstable_by(|(a, i), (b, j)| b.total_cmp(a).then(i.cmp(j)));
    scored.into_iter().take(limit).map(|(_, i)| i).collect()
}

/// `1 - d / len(term)` for the closest window of `name` (window lengths
/// `len(term) ± 1`, so one insertion or deletion is reachable).
fn typo_similarity(term: &[char], name: &[char]) -> f32 {
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
fn osa_distance(a: &[char], b: &[char]) -> usize {
    let cols = b.len() + 1;
    let mut d = vec![0usize; (a.len() + 1) * cols];
    for i in 0..=a.len() { d[i * cols] = i; }
    for (j, cell) in d.iter_mut().enumerate().take(cols) { *cell = j; }
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
        fuzzy_rank(&c, term, threshold, 10).into_iter().map(|i| c[i].to_owned()).collect()
    }

    #[test]
    fn transposed_term_finds_main_at_default_threshold() {
        assert_eq!(names("mian", 0.7).first().map(String::as_str), Some("main.rs"));
    }

    #[test]
    fn exact_short_term_passes_default_threshold() {
        assert_eq!(names("main", 0.7).first().map(String::as_str), Some("main.rs"));
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
