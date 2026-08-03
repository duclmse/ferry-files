use nucleo::{Config as NucleoConfig, Matcher, Utf32Str};
use rayon::prelude::*;
use regex::Regex;

use crate::error::{AppError, Result};
use crate::store::{FileRecord, FileStore};

use super::types::PostFilter;

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
    }
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

pub(super) fn fuzzy_score(
    candidates: &[FileRecord],
    term:       &str,
    threshold:  f32,
    limit:      usize,
) -> Vec<FileRecord> {
    let mut needle_buf = Vec::new();
    let needle = Utf32Str::new(term, &mut needle_buf);

    let mut scored: Vec<(u16, &FileRecord)> = candidates
        .par_iter()
        .filter_map(|r| {
            let mut matcher = Matcher::new(NucleoConfig::DEFAULT);
            let mut hay_buf = Vec::new();
            let haystack    = Utf32Str::new(&r.name, &mut hay_buf);
            matcher
                .fuzzy_match(haystack, needle)
                .map(|score| (score, r))
        })
        .collect();

    let max_possible = 1024u16;
    scored.retain(|(s, _)| (*s as f32 / max_possible as f32) >= threshold);
    scored.sort_unstable_by(|(a, _), (b, _)| b.cmp(a));
    scored.into_iter().take(limit).map(|(_, r)| r.clone()).collect()
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
