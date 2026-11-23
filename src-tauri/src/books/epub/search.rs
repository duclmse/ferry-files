//! Find and replace across the chapters.

use super::*;

impl Book {
    /// Ranges of text tokens inside <body>, outside <script> and <style>.
    pub(super) fn body_texts(text: &str, toks: &[Tok]) -> Vec<(usize, usize)> {
        let mut out = vec![];
        let (mut in_body, mut skip) = (false, 0usize);
        for t in toks {
            let l = t.local();
            match t.kind {
                Kind::Open if l == "body" => in_body = true,
                Kind::Close if l == "body" => in_body = false,
                Kind::Open if in_body && (l == "script" || l == "style") => skip += 1,
                Kind::Close if in_body && (l == "script" || l == "style") => skip = skip.saturating_sub(1),
                Kind::Text if in_body && skip == 0 && !text[t.start..t.end].trim().is_empty() => out.push((t.start, t.end)),
                _ => {}
            }
        }
        out
    }

    pub fn find(&self, q: &Query) -> Result<Found> {
        let mut found = Found::default();
        if q.find.is_empty() {
            return Ok(found);
        }
        for ch in self.chapters()? {
            let Some(toks) = self.text(&ch.path).ok().and_then(|t| xml::tokenize(&t).ok().map(|k| (t, k))) else {
                found.skipped.push(ch.label);
                continue;
            };
            let (text, toks) = toks;
            let mut hit = Hit { path: ch.path.clone(), label: ch.label.clone(), count: 0, snippets: vec![] };
            for (s, e) in Self::body_texts(&text, &toks) {
                let plain = xml::unescape(&text[s..e]);
                for m in find_all(&plain, &q.find, q.match_case, q.whole_word) {
                    hit.count += 1;
                    if hit.snippets.len() < 3 {
                        hit.snippets.push(snippet(&plain, m));
                    }
                }
            }
            if hit.count > 0 {
                found.total += hit.count;
                found.hits.push(hit);
            }
        }
        Ok(found)
    }

    /// Replaces every match in chapter text (never in markup). Returns how many were replaced.
    pub fn replace_all(&mut self, q: &Query) -> Result<usize> {
        if q.find.is_empty() {
            return Ok(0);
        }
        let mut total = 0;
        for ch in self.chapters()? {
            let Ok(text) = self.text(&ch.path) else { continue };
            let Ok(toks) = xml::tokenize(&text) else { continue };
            let mut edits = vec![];
            for (s, e) in Self::body_texts(&text, &toks) {
                let plain = xml::unescape(&text[s..e]);
                let ms = find_all(&plain, &q.find, q.match_case, q.whole_word);
                if ms.is_empty() {
                    continue;
                }
                total += ms.len();
                let mut out = String::with_capacity(plain.len());
                let mut last = 0;
                for (a, b) in ms {
                    out.push_str(&plain[last..a]);
                    out.push_str(&q.replace);
                    last = b;
                }
                out.push_str(&plain[last..]);
                edits.push((s, e, xml::escape_text(&out)));
            }
            if !edits.is_empty() {
                let new = xml::splice(&text, edits);
                self.set_file(&ch.path, new.into_bytes());
            }
        }
        Ok(total)
    }
}
