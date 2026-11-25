//! Checks a book for what readers trip on.

use super::*;

/// Something wrong with a book, found by `Book::check`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Issue {
    /// Readers may refuse the book, or show it wrongly. Otherwise it's untidy but usually works.
    pub serious: bool,
    pub text: String,
}

impl Book {
    /// Every file in the book: (path inside the zip, bytes).
    pub fn files(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.files.iter().map(|(n, d)| (n.as_str(), d.as_slice()))
    }

    /// Replaces a file's bytes (the caller keeps its type the same).
    pub fn put_file(&mut self, path: &str, data: Vec<u8>) {
        self.set_file(path, data);
    }

    /// Looks for what makes readers refuse or misshow a book: missing files and metadata, broken
    /// chapters, links to files that aren't there.
    pub fn check(&self) -> Vec<Issue> {
        let mut out = vec![];
        let mut bad = |serious: bool, text: String| out.push(Issue { serious, text });
        let p = match self.package() {
            Ok(p) => p,
            Err(e) => {
                bad(true, e);
                return out;
            }
        };
        for (what, l) in [
            ("title", "title"),
            ("language", "language"),
            ("identifier", "identifier"),
        ] {
            if p.dc(l).iter().all(|e| e.text.is_empty()) {
                bad(true, format!("The book has no {what}."));
            }
        }
        let mut ids = std::collections::HashSet::new();
        for i in &p.items {
            if !ids.insert(i.id.as_str()) {
                bad(
                    true,
                    format!("Two files in the manifest have the id “{}”.", i.id),
                );
            }
            if i.path.is_empty() || self.file(&i.path).is_none() {
                bad(
                    true,
                    format!(
                        "{} is listed but isn't in the book.",
                        if i.path.is_empty() { &i.id } else { &i.path }
                    ),
                );
            }
        }
        if p.spine.is_empty() {
            bad(true, "The reading order (spine) is empty.".into());
        }
        for id in &p.spine {
            if p.item_by_id(id).is_none() {
                bad(
                    true,
                    format!("The reading order names “{id}”, which isn't in the manifest."),
                );
            }
        }
        if p.version.starts_with('3')
            && !p
                .items
                .iter()
                .any(|i| i.props.split_whitespace().any(|x| x == "nav"))
        {
            bad(false, "No table of contents (nav document).".into());
        }
        if p.version.starts_with('2')
            && p.toc_id
                .as_deref()
                .and_then(|id| p.item_by_id(id))
                .is_none()
        {
            bad(false, "No table of contents (NCX).".into());
        }
        if p.cover().is_none() {
            bad(false, "No cover image is marked.".into());
        }
        let listed: std::collections::HashSet<&str> =
            p.items.iter().map(|i| i.path.as_str()).collect();
        let unlisted: Vec<&str> = self
            .files
            .iter()
            .map(|(n, _)| n.as_str())
            .filter(|n| {
                *n != "mimetype"
                    && !n.starts_with("META-INF/")
                    && *n != self.opf
                    && !listed.contains(n)
            })
            .collect();
        if !unlisted.is_empty() {
            let more = if unlisted.len() > 3 {
                format!(" and {} more", unlisted.len() - 3)
            } else {
                String::new()
            };
            bad(
                false,
                format!(
                    "Files not in the manifest: {}{more}.",
                    unlisted[..unlisted.len().min(3)].join(", ")
                ),
            );
        }
        let mut missing = 0;
        for i in p.items.iter().filter(|i| i.media.contains("html")) {
            let Ok(text) = self.text(&i.path) else {
                continue;
            };
            if let Err(e) = xml::check(&text) {
                bad(true, format!("{}: {e}", i.path));
                continue;
            }
            let Ok(toks) = xml::tokenize(&text) else {
                continue;
            };
            for t in toks.iter().filter(|t| t.is_start()) {
                let href = match t.local() {
                    "img" => t.attr("src"),
                    "link" => t.attr("href"),
                    "image" => t.attr_local("href"),
                    "a" => t.attr("href"),
                    _ => None,
                };
                let Some(target) = href.and_then(|h| resolve(&i.path, h)) else {
                    continue;
                };
                if self.file(&target).is_none() {
                    missing += 1;
                    if missing <= 5 {
                        bad(
                            true,
                            format!("{} links to {target}, which isn't in the book.", i.path),
                        );
                    }
                }
            }
        }
        if missing > 5 {
            bad(
                true,
                format!("…and {} more links to missing files.", missing - 5),
            );
        }
        out
    }
}
