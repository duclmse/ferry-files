//! Moving the start of a chapter to the end of the chapter before it, or its end to the start of
//! the chapter after it, for books whose chapter files were split in the wrong place. Both files
//! are spliced: only the part moved changes, and the rest keeps its exact bytes.

use super::*;
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use std::collections::HashSet;

/// Where the chapter is cut: the index of one of the top-level elements in its <body>, or a
/// place in its text (a byte offset, counting each line break as one byte), which picks the
/// element it falls in (or the first after it); with `true`, the element after that one, so the
/// element at the offset goes with what comes before it.
#[derive(Debug, Clone, Copy)]
pub enum Cut {
    Block(usize),
    Offset(usize, bool),
}

/// What a move did, for the sentence that says so.
#[derive(Debug, Clone, Serialize)]
pub struct Moved {
    /// The chapter the part went to.
    pub to: String,
    /// Top-level elements moved (paragraphs, headings…).
    pub blocks: usize,
    /// Links changed to follow the part moved (in the contents, other chapters, or the part).
    pub links: usize,
}

/// The top-level elements inside <body>: their byte range and local name.
pub(super) fn blocks(text: &str, toks: &[Tok], path: &str) -> Result<Vec<(usize, usize, String)>> {
    let open = toks
        .iter()
        .position(|t| t.kind == Kind::Open && t.local() == "body")
        .ok_or_else(|| format!("{path} has no <body>."))?;
    let close = xml::matching_close(toks, open)
        .ok_or_else(|| format!("{path}: <body> is never closed."))?;
    let mut out = vec![];
    let mut k = open + 1;
    while k < close {
        let t = &toks[k];
        match t.kind {
            Kind::Open => {
                let end = xml::matching_close(toks, k).ok_or_else(|| {
                    format!(
                        "{path}, line {}: <{}> is never closed.",
                        xml::line_of(text, t.start),
                        t.name
                    )
                })?;
                out.push((t.start, toks[end].end, t.local().to_string()));
                k = end + 1;
            }
            Kind::Empty => {
                out.push((t.start, t.end, t.local().to_string()));
                k += 1;
            }
            _ => k += 1,
        }
    }
    Ok(out)
}

/// The top-level element `cut` names among `bl` (the chapter's, from `blocks`), or their count.
pub(super) fn cut_block(text: &str, bl: &[(usize, usize, String)], cut: Cut) -> usize {
    match cut {
        Cut::Block(k) => k.min(bl.len()),
        Cut::Offset(off, through) => {
            let off = real_offset(text, off);
            let k = bl.iter().position(|b| b.1 > off).unwrap_or(bl.len());
            if through {
                (k + 1).min(bl.len())
            } else {
                k
            }
        }
    }
}

/// The byte offset in `text` of `lf`, an offset in the same text with each "\r\n" as one byte
/// (as a text box holds it).
fn real_offset(text: &str, lf: usize) -> usize {
    let b = text.as_bytes();
    let (mut i, mut n) = (0, 0);
    while i < b.len() && n < lf {
        i += if b[i] == b'\r' && b.get(i + 1) == Some(&b'\n') {
            2
        } else {
            1
        };
        n += 1;
    }
    i
}

impl Book {
    /// The local names of the top-level elements in a chapter's <body>, in order.
    pub fn block_names(&self, path: &str) -> Result<Vec<String>> {
        let text = self.text(path)?;
        Ok(blocks(&text, &xml::tokenize(&text)?, path)?
            .into_iter()
            .map(|b| b.2)
            .collect())
    }

    /// Moves part of the chapter at `path` to the chapter next to it in reading order. To the
    /// next chapter (`next`): the element at `cut` and all after it, put at the start of that
    /// chapter's body. To the previous one: everything before the element at `cut`, put at the
    /// end of that chapter's body. `names`, when given, are the top-level elements the caller
    /// sees (the visual view); the move is refused if the file's differ, so the wrong part is
    /// never moved.
    pub fn move_part(
        &mut self,
        path: &str,
        next: bool,
        cut: Cut,
        names: Option<&[String]>,
    ) -> Result<Moved> {
        let chapters = self.chapters()?;
        let at = chapters
            .iter()
            .position(|c| c.path == path)
            .ok_or_else(|| format!("{path} isn't one of the book's chapters."))?;
        let other = match (next, at) {
            (true, _) if at + 1 < chapters.len() => &chapters[at + 1],
            (true, _) => {
                return Err(
                    "This is the last chapter, so there's no next chapter to move it to.".into(),
                )
            }
            (false, 0) => {
                return Err(
                    "This is the first chapter, so there's no previous chapter to move it to."
                        .into(),
                )
            }
            (false, _) => &chapters[at - 1],
        };
        let text = self.text(path)?;
        xml::check(&text).map_err(|e| {
            format!(
                "{} isn't valid XHTML, so nothing was moved. Fix it in Source first: {e}",
                file_name(path)
            )
        })?;
        let toks = xml::tokenize(&text)?;
        let (s, e) = Self::body_range(&toks, path)?;
        let bl = blocks(&text, &toks, path)?;
        if let Some(names) = names {
            if names.len() != bl.len()
                || names
                    .iter()
                    .zip(&bl)
                    .any(|(n, b)| !n.eq_ignore_ascii_case(&b.2))
            {
                return Err("The visual view doesn't match this chapter's XHTML here. Do the move from the Source view.".into());
            }
        }
        let k = cut_block(&text, &bl, cut);
        let inner = &text[s..e];
        let lead = &inner[..inner.len() - inner.trim_start().len()];
        let trail = &inner[inner.trim_end().len()..];
        // Where the part moved starts and ends, how many elements it has, and this chapter without it.
        let (ms, me, count, kept) = if next {
            if k >= bl.len() {
                return Err(
                    "Nothing comes after this in the chapter, so there's nothing to move.".into(),
                );
            }
            let cut = bl[k].0;
            (
                cut,
                e,
                bl.len() - k,
                format!(
                    "{}{}{trail}{}",
                    &text[..s],
                    text[s..cut].trim_end(),
                    &text[e..]
                ),
            )
        } else {
            if k == 0 {
                return Err(
                    "Nothing comes before this in the chapter, so there's nothing to move.".into(),
                );
            }
            let cut = if k < bl.len() { bl[k].0 } else { e };
            (s, cut, k, format!("{}{lead}{}", &text[..s], &text[cut..]))
        };
        let part = text[ms..me].trim();
        if dir_of(path) != dir_of(&other.path) && part.to_ascii_lowercase().contains("url(") {
            return Err(format!(
                "{} and {} are in different folders, and the part has styles that name files (url()), which would break. Move it from the Source view.",
                file_name(path),
                file_name(&other.path)
            ));
        }
        // What links can point to in the part: its ids, and old-style <a name>s.
        let anchors: HashSet<String> = toks
            .iter()
            .filter(|t| t.start >= ms && t.end <= me && t.is_start())
            .flat_map(|t| {
                [
                    t.attr("id"),
                    if t.local() == "a" {
                        t.attr("name")
                    } else {
                        None
                    },
                ]
            })
            .flatten()
            .map(String::from)
            .collect();

        let otext = self.text(&other.path)?;
        xml::check(&otext).map_err(|e| {
            format!(
                "{} isn't valid XHTML, so nothing was moved. Fix it in Source first: {e}",
                file_name(&other.path)
            )
        })?;
        let otoks = xml::tokenize(&otext)?;
        let (os, oe) = Self::body_range(&otoks, &other.path)?;
        let at = if next {
            os
        } else {
            os + otext[os..oe].trim_end().len()
        };
        let joined = format!("{}\n{part}{}", &otext[..at], &otext[at..]);
        let part_at = (at + 1, at + 1 + part.len());

        // Links follow the part: to its anchors, from anywhere; and from it, to everything else.
        let moves = Moves {
            from: path,
            to: &other.path,
            anchors: &anchors,
        };
        let (kept, a) = moves.relink(&kept, &|_| path, path)?;
        let (joined, b) = moves.relink(
            &joined,
            &|k| {
                if k >= part_at.0 && k < part_at.1 {
                    path
                } else {
                    &other.path
                }
            },
            &other.path,
        )?;
        let mut links = a + b;
        let mut others = vec![];
        if !anchors.is_empty() {
            let p = self.package()?;
            for i in p.items.iter().filter(|i| {
                i.path != path
                    && i.path != other.path
                    && (i.media.contains("html") || i.media.contains("dtbncx"))
            }) {
                let Ok(t) = self.text(&i.path) else { continue };
                let Ok((changed, n)) = moves.relink(&t, &|_| &i.path, &i.path) else {
                    continue;
                };
                if n > 0 {
                    xml::check(&changed).map_err(|e| {
                        format!(
                            "Changing the links in {} would break it, so nothing was moved: {e}",
                            file_name(&i.path)
                        )
                    })?;
                    links += n;
                    others.push((i.path.clone(), changed));
                }
            }
        }
        xml::check(&kept).map_err(|e| {
            format!(
                "Moving that would break {}, so nothing was moved: {e}",
                file_name(path)
            )
        })?;
        xml::check(&joined).map_err(|e| {
            format!(
                "Moving that would break {}, so nothing was moved: {e}",
                file_name(&other.path)
            )
        })?;
        let to = other.label.clone();
        let other = other.path.clone();
        self.set_file(path, kept.into_bytes());
        self.set_file(&other, joined.into_bytes());
        for (p, t) in others {
            self.set_file(&p, t.into_bytes());
        }
        Ok(Moved {
            to,
            blocks: count,
            links,
        })
    }
}

/// A part moving from one chapter to another, for its links.
struct Moves<'a> {
    from: &'a str,
    to: &'a str,
    anchors: &'a HashSet<String>,
}

/// Characters written as %xx in an address made here.
const HREF: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}');

impl Moves<'_> {
    /// `text` (the file `file`) with each link (href, src) changed where the part moving changes
    /// what it should say, and how many were. `was(k)` is the file the text at byte `k` was in:
    /// links in the part moved were written for the chapter it came from.
    fn relink<'f>(
        &self,
        text: &str,
        was: &dyn Fn(usize) -> &'f str,
        file: &str,
    ) -> Result<(String, usize)> {
        let toks = xml::tokenize(text)?;
        let mut edits = vec![];
        for t in toks.iter().filter(|t| t.is_start()) {
            let was = was(t.start);
            for (k, (a, v)) in t.attrs.iter().enumerate() {
                if !matches!(xml::local(a), "href" | "src" | "poster" | "data") {
                    continue;
                }
                let Some((target, frag)) = target(was, v) else {
                    continue;
                };
                // The part's anchors are in the other chapter now.
                let target =
                    if target == self.from && frag.is_some_and(|f| self.anchors.contains(f)) {
                        self.to.to_string()
                    } else {
                        target
                    };
                if self::target(file, v).is_some_and(|(t, f)| t == target && f == frag) {
                    continue;
                }
                let mut href = if target == file && frag.is_some() {
                    String::new()
                } else {
                    utf8_percent_encode(&relative(file, &target), HREF).to_string()
                };
                if let Some(f) = frag {
                    href = format!("{href}#{f}");
                }
                let (s, e) = t.vals[k];
                edits.push((s, e, xml::escape_attr(&href)));
            }
        }
        let n = edits.len();
        Ok((
            if n == 0 {
                text.to_string()
            } else {
                xml::splice(text, edits)
            },
            n,
        ))
    }
}

/// The file and fragment a link in the file `base` goes to. None for links out of the book.
fn target<'a>(base: &str, href: &'a str) -> Option<(String, Option<&'a str>)> {
    let frag = href
        .split_once('#')
        .map(|(_, f)| f)
        .filter(|f| !f.is_empty());
    if href.starts_with('#') {
        return Some((base.to_string(), frag));
    }
    Some((resolve(base, href)?, frag))
}

#[cfg(test)]
mod tests {
    #[test]
    fn offsets_count_crlf_as_one() {
        assert_eq!(super::real_offset("a\r\nb", 2), 3);
        assert_eq!(super::real_offset("a\nb", 2), 2);
        assert_eq!(super::real_offset("ab", 9), 2);
    }
}
