//! Find and replace across the chapters.
//!
//! Text is searched a run at a time: the text of a paragraph (or heading, list item…) across the
//! formatting inside it, so a phrase half in bold is still found. A replacement changes only the
//! characters matched, in the text they're written in; everything else keeps its bytes, entity
//! references included.

use super::*;

/// A chapter's matches listed with their text around them; past this, only counted.
const SNIPPETS_MAX: usize = 200;

/// Elements inside a run of text, which a phrase may cross. Any other tag (a paragraph, a line
/// break, a picture) ends the run.
const INLINE: &[&str] = &[
    "a", "abbr", "b", "bdi", "bdo", "big", "cite", "code", "data", "del", "dfn", "em", "font", "i", "ins", "kbd", "mark", "q", "s", "samp", "small", "span",
    "strike", "strong", "sub", "sup", "time", "tt", "u", "var",
];

/// A text token in a run: where its decoded text starts in the run's, and for each place in that
/// text, where it is in the chapter (see `xml::decode_at`).
struct Piece {
    from: usize,
    at: Vec<Option<usize>>,
}

/// A run of text, decoded, and the tokens it's made of.
struct Run {
    plain: String,
    pieces: Vec<Piece>,
}

impl Run {
    /// The edits that put `with` in place of `plain[a..b]`: the part of the match in its first
    /// token is replaced, the parts in the others removed. None when an end falls inside a
    /// character reference that isn't decoded, which is left alone.
    fn edits(&self, (a, b): (usize, usize), with: &str) -> Option<Vec<(usize, usize, String)>> {
        let mut out = vec![];
        for p in &self.pieces {
            let to = p.from + p.at.len() - 1;
            let (lo, hi) = (a.max(p.from), b.min(to));
            if lo < hi {
                let text = if out.is_empty() { with.to_string() } else { String::new() };
                out.push((p.at[lo - p.from]?, p.at[hi - p.from]?, text));
            }
        }
        Some(out)
    }
}

/// `with` in the case of `matched`: all capitals, all small letters, or a capital first (as the
/// text editor's Preserve case does).
fn keep_case(matched: &str, with: &str) -> String {
    let letters: Vec<char> = matched.chars().filter(|c| c.is_alphabetic()).collect();
    let Some(&first) = letters.first() else { return with.to_string() };
    if letters.len() > 1 && letters.iter().all(|c| c.is_uppercase()) {
        with.to_uppercase()
    } else if letters.iter().all(|c| c.is_lowercase()) {
        with.to_lowercase()
    } else if first.is_uppercase() {
        let mut cs = with.chars();
        cs.next().map_or_else(String::new, |c| c.to_uppercase().chain(cs).collect())
    } else {
        with.to_string()
    }
}

impl Book {
    /// The runs of text inside <body>, outside <script> and <style>.
    fn runs(text: &str, toks: &[Tok]) -> Vec<Run> {
        let mut runs: Vec<Vec<(usize, usize)>> = vec![vec![]];
        let (mut in_body, mut skip) = (false, 0usize);
        for t in toks {
            let l = t.local();
            match t.kind {
                Kind::Open if l == "body" => in_body = true,
                Kind::Close if l == "body" => in_body = false,
                _ if !in_body => {}
                Kind::Open if l == "script" || l == "style" => skip += 1,
                Kind::Close if l == "script" || l == "style" => skip = skip.saturating_sub(1),
                _ if skip > 0 => {}
                Kind::Text => runs.last_mut().unwrap().push((t.start, t.end)),
                Kind::Open | Kind::Close | Kind::Empty if INLINE.contains(&l) => {}
                Kind::Comment | Kind::Pi => {}
                _ => runs.push(vec![]),
            }
        }
        runs.into_iter()
            .filter(|r| r.iter().any(|&(s, e)| !text[s..e].trim().is_empty()))
            .map(|r| {
                let mut run = Run { plain: String::new(), pieces: vec![] };
                for (s, e) in r {
                    let (plain, at) = xml::decode_at(text, s, e);
                    run.pieces.push(Piece { from: run.plain.len(), at });
                    run.plain.push_str(&plain);
                }
                run
            })
            .collect()
    }

    /// The matches of `q` in a chapter, each with the edits that would replace it (with a
    /// regular expression, `$1` in the replacement is what its first group matched).
    fn matches(text: &str, toks: &[Tok], q: &Query, re: Option<&regex::Regex>) -> Vec<(Snippet, Vec<(usize, usize, String)>)> {
        let mut out = vec![];
        for run in Self::runs(text, toks) {
            let found: Vec<((usize, usize), String)> = match re {
                Some(re) => re
                    .captures_iter(&run.plain)
                    .filter_map(|c| {
                        let m = c.get(0).filter(|m| !m.is_empty())?;
                        let mut with = String::new();
                        c.expand(&q.replace, &mut with);
                        Some(((m.start(), m.end()), with))
                    })
                    .collect(),
                None => find_all(&run.plain, &q.find, q.match_case, q.whole_word).into_iter().map(|m| (m, q.replace.clone())).collect(),
            };
            for (m, with) in found {
                let with = if q.preserve_case { keep_case(&run.plain[m.0..m.1], &with) } else { with };
                if let Some(edits) = run.edits(m, &xml::escape_text(&with)) {
                    out.push((snippet(&run.plain, m), edits));
                }
            }
        }
        out
    }

    /// The regular expression `q` asks for, if it asks for one.
    fn regex(q: &Query) -> Result<Option<regex::Regex>> {
        if !q.regex {
            return Ok(None);
        }
        let pattern = if q.whole_word { format!(r"\b(?:{})\b", q.find) } else { q.find.clone() };
        regex::RegexBuilder::new(&pattern)
            .case_insensitive(!q.match_case)
            .build()
            .map(Some)
            .map_err(|e| format!("That regular expression doesn't work: {}", e.to_string().lines().last().unwrap_or("").trim()))
    }

    pub fn find(&self, q: &Query) -> Result<Found> {
        let mut found = Found::default();
        if q.find.trim().is_empty() {
            return Ok(found);
        }
        let re = Self::regex(q)?;
        for ch in self.chapters()? {
            let Some((text, toks)) = self.text(&ch.path).ok().and_then(|t| xml::tokenize(&t).ok().map(|k| (t, k))) else {
                found.skipped.push(ch.label);
                continue;
            };
            let ms = Self::matches(&text, &toks, q, re.as_ref());
            if !ms.is_empty() {
                found.total += ms.len();
                let snippets = ms.into_iter().map(|m| m.0).collect::<Vec<_>>();
                found.hits.push(Hit { path: ch.path, label: ch.label, count: snippets.len(), snippets: snippets.into_iter().take(SNIPPETS_MAX).collect() });
            }
        }
        Ok(found)
    }

    /// Replaces the first match in the chapter at `path` that starts at or after byte `from` of
    /// its XHTML. Returns where the replacement ends (where to go on from), or None when no match
    /// is left there.
    pub fn replace_one(&mut self, q: &Query, path: &str, from: usize) -> Result<Option<usize>> {
        if q.find.trim().is_empty() {
            return Ok(None);
        }
        let re = Self::regex(q)?;
        let text = self.text(path)?;
        let toks = xml::tokenize(&text)?;
        let Some((_, edits)) = Self::matches(&text, &toks, q, re.as_ref()).into_iter().find(|(_, e)| e.first().is_some_and(|e| e.0 >= from)) else {
            return Ok(None);
        };
        let end = edits[0].0 + edits[0].2.len();
        let new = xml::splice(&text, edits);
        self.set_file(path, new.into_bytes());
        Ok(Some(end))
    }

    /// Replaces every match in chapter text (never in markup). Returns how many were replaced.
    pub fn replace_all(&mut self, q: &Query) -> Result<usize> {
        if q.find.trim().is_empty() {
            return Ok(0);
        }
        let re = Self::regex(q)?;
        let mut total = 0;
        for ch in self.chapters()? {
            let Ok(text) = self.text(&ch.path) else { continue };
            let Ok(toks) = xml::tokenize(&text) else { continue };
            let ms = Self::matches(&text, &toks, q, re.as_ref());
            if !ms.is_empty() {
                total += ms.len();
                let new = xml::splice(&text, ms.into_iter().flat_map(|m| m.1).collect());
                self.set_file(&ch.path, new.into_bytes());
            }
        }
        Ok(total)
    }
}
