//! The book's table of contents, as entries to add and remove: in the EPUB 3 nav document and
//! the EPUB 2 NCX, whichever the book has (both when it has both).
//!
//! An entry's depth follows the EPUB standard: the contents are nested lists (`<ol>` in `<li>`
//! in the nav, `<navPoint>` in `<navPoint>` in the NCX), and depth 1 is the outer list. A new
//! entry goes where its place falls in reading order. Its level is said relative to the entry
//! before it: a chapter beside it, a section one level up (a part, say, holding what follows),
//! a subsection one level in, under it.
//!
//! Adding or removing rewrites the list's structure (the `<ol>`s and `<li>`s, or the
//! `<navPoint>`s) but keeps each entry's own markup: its `<li>` tag and its link (or label and
//! content) keep their exact bytes. The rest of both files is untouched.

use super::part::{blocks, cut_block};
use super::*;

/// An entry of the contents, as the editor lists it.
#[derive(Debug, Clone, Serialize)]
pub struct TocEntry {
    pub label: String,
    /// The chapter it opens, and the place in it (an id), if it names one.
    pub path: String,
    pub fragment: Option<String>,
    /// 1 for the outer list.
    pub depth: usize,
}

/// Where a new entry goes, relative to the entry before it in reading order.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// One level up from it.
    Section,
    /// At its level.
    Chapter,
    /// One level in, under it.
    Subsection,
}

/// One entry of a list being rewritten: its own markup, kept as it was.
#[derive(Debug, Clone)]
struct Node {
    /// The nav's `<li …>` tag, or the NCX's `<navPoint …>` tag (its playOrder is renumbered).
    open: String,
    /// The nav's `<a>` (or `<span>`); the NCX's `<navLabel>…</navLabel>` and `<content/>`.
    body: String,
    label: String,
    path: String,
    fragment: Option<String>,
    depth: usize,
}

/// A table of contents' list, read from its file.
struct List {
    file: String,
    text: String,
    /// What's between the outer list's tags (the nav's `<ol>`, the NCX's `<navMap>`).
    inner: (usize, usize),
    /// The outer list's indent, to indent the new markup by.
    indent: String,
    nodes: Vec<Node>,
    ncx: bool,
}

fn indent_at(text: &str, pos: usize) -> String {
    let start = text[..pos].rfind('\n').map_or(0, |k| k + 1);
    text[start..pos]
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect()
}

fn target(base: &str, href: &str) -> (String, Option<String>) {
    let frag = href
        .split_once('#')
        .map(|(_, f)| {
            percent_encoding::percent_decode_str(f)
                .decode_utf8_lossy()
                .to_string()
        })
        .filter(|f| !f.is_empty());
    (resolve(base, href).unwrap_or_default(), frag)
}

impl List {
    /// The nav document's table of contents (its `<nav epub:type="toc">`, else its first nav).
    fn nav(file: &str, text: String) -> Option<List> {
        let toks = xml::tokenize(&text).ok()?;
        let navs: Vec<usize> = (0..toks.len())
            .filter(|&k| toks[k].kind == Kind::Open && toks[k].local() == "nav")
            .collect();
        let nav = navs
            .iter()
            .copied()
            .find(|&k| {
                toks[k]
                    .attr_local("type")
                    .is_some_and(|v| v.split_whitespace().any(|x| x == "toc"))
            })
            .or(navs.first().copied())?;
        let end = xml::matching_close(&toks, nav)?;
        let ol = (nav + 1..end).find(|&k| toks[k].kind == Kind::Open && toks[k].local() == "ol")?;
        let ol_end = xml::matching_close(&toks, ol)?;
        let mut nodes = vec![];
        Self::nav_items(file, &text, &toks, ol, ol_end, 1, &mut nodes);
        Some(List {
            indent: indent_at(&text, toks[ol].start),
            inner: (toks[ol].end, toks[ol_end].start),
            file: file.into(),
            text,
            nodes,
            ncx: false,
        })
    }

    /// The `<li>`s of the `<ol>` from `toks[ol]` to `toks[end]`, and theirs, at `depth`.
    fn nav_items(
        file: &str,
        text: &str,
        toks: &[Tok],
        ol: usize,
        end: usize,
        depth: usize,
        out: &mut Vec<Node>,
    ) {
        let mut k = ol + 1;
        while k < end {
            let t = &toks[k];
            if t.kind != Kind::Open || t.local() != "li" {
                k += 1;
                continue;
            }
            let li_end = xml::matching_close(toks, k).unwrap_or(end);
            let (mut body, mut label, mut to) =
                (String::new(), String::new(), (String::new(), None));
            let mut j = k + 1;
            let mut sub = None;
            while j < li_end {
                let c = &toks[j];
                if c.is_start() && matches!(c.local(), "a" | "span") && body.is_empty() {
                    let close = xml::matching_close(toks, j).unwrap_or(j);
                    body = text[c.start..toks[close].end].to_string();
                    label = squash(&xml::inner_text(text, &toks[j + 1..close.max(j + 1)]));
                    if let Some(h) = c.attr("href") {
                        to = target(file, h);
                    }
                    j = close + 1;
                } else if c.kind == Kind::Open && c.local() == "ol" {
                    sub = Some((j, xml::matching_close(toks, j).unwrap_or(li_end)));
                    break;
                } else {
                    j += 1;
                }
            }
            out.push(Node {
                open: text[t.start..t.end].to_string(),
                body,
                label,
                path: to.0,
                fragment: to.1,
                depth,
            });
            if let Some((s, e)) = sub {
                Self::nav_items(file, text, toks, s, e, depth + 1, out);
            }
            k = li_end + 1;
        }
    }

    /// The NCX's `<navMap>`.
    fn ncx(file: &str, text: String) -> Option<List> {
        let toks = xml::tokenize(&text).ok()?;
        let map = toks
            .iter()
            .position(|t| t.kind == Kind::Open && t.local() == "navMap")?;
        let end = xml::matching_close(&toks, map)?;
        let mut nodes = vec![];
        Self::nav_points(file, &text, &toks, map, end, 1, &mut nodes);
        Some(List {
            indent: indent_at(&text, toks[map].start),
            inner: (toks[map].end, toks[end].start),
            file: file.into(),
            text,
            nodes,
            ncx: true,
        })
    }

    fn nav_points(
        file: &str,
        text: &str,
        toks: &[Tok],
        from: usize,
        end: usize,
        depth: usize,
        out: &mut Vec<Node>,
    ) {
        let mut k = from + 1;
        while k < end {
            let t = &toks[k];
            if t.kind != Kind::Open || t.local() != "navPoint" {
                k += 1;
                continue;
            }
            let p_end = xml::matching_close(toks, k).unwrap_or(end);
            let (mut body, mut label, mut to) =
                (String::new(), String::new(), (String::new(), None));
            let mut j = k + 1;
            let mut first_child = None;
            while j < p_end {
                let c = &toks[j];
                if c.kind == Kind::Open && c.local() == "navLabel" {
                    let close = xml::matching_close(toks, j).unwrap_or(j);
                    body.push_str(&text[c.start..toks[close].end]);
                    label = squash(&xml::inner_text(text, &toks[j + 1..close.max(j + 1)]));
                    j = close + 1;
                } else if c.is_start() && c.local() == "content" {
                    let close = xml::matching_close(toks, j).unwrap_or(j);
                    body.push_str(&text[c.start..toks[close].end]);
                    if let Some(src) = c.attr("src") {
                        to = target(file, src);
                    }
                    j = close + 1;
                } else if c.kind == Kind::Open && c.local() == "navPoint" {
                    first_child = Some(j);
                    break;
                } else {
                    j += 1;
                }
            }
            out.push(Node {
                open: text[t.start..t.end].to_string(),
                body,
                label,
                path: to.0,
                fragment: to.1,
                depth,
            });
            if let Some(c) = first_child {
                Self::nav_points(file, text, toks, c - 1, p_end, depth + 1, out);
            }
            k = p_end + 1;
        }
    }

    /// The list's markup from its nodes, nested by their depths.
    fn render(&self) -> String {
        let (list, end_item) = if self.ncx {
            ("", "</navPoint>")
        } else {
            ("ol", "</li>")
        };
        let mut out = String::new();
        let mut order = 0;
        let mut k = 0;
        self.render_from(&mut k, 1, &mut out, &mut order, list, end_item);
        out.push('\n');
        out.push_str(&self.indent);
        out
    }

    fn render_from(
        &self,
        k: &mut usize,
        depth: usize,
        out: &mut String,
        order: &mut usize,
        list: &str,
        end_item: &str,
    ) {
        // Nested lists sit one step in; each entry under the list's own indent.
        let pad = |d: usize| {
            format!(
                "{}{}",
                self.indent,
                "  ".repeat(if list.is_empty() { d } else { 2 * d - 1 })
            )
        };
        while *k < self.nodes.len() && self.nodes[*k].depth == depth {
            let n = &self.nodes[*k];
            *order += 1;
            let open = if self.ncx {
                renumbered(&n.open, *order)
            } else {
                n.open.clone()
            };
            out.push_str(&format!("\n{}{open}{}", pad(depth), n.body));
            *k += 1;
            if *k < self.nodes.len() && self.nodes[*k].depth > depth {
                if !list.is_empty() {
                    out.push_str(&format!("\n{}  <{list}>", pad(depth)));
                }
                self.render_from(k, depth + 1, out, order, list, end_item);
                if !list.is_empty() {
                    out.push_str(&format!("\n{}  </{list}>", pad(depth)));
                }
                out.push_str(&format!("\n{}{end_item}", pad(depth)));
            } else {
                out.push_str(end_item);
            }
        }
    }

    /// The file with the list rewritten, checked; and for the NCX, its dtb:depth made right.
    fn written(&self) -> Result<String> {
        let mut text = xml::splice(
            &self.text,
            vec![(self.inner.0, self.inner.1, self.render())],
        );
        if self.ncx {
            let depth = self.nodes.iter().map(|n| n.depth).max().unwrap_or(1);
            if let Ok(toks) = xml::tokenize(&text) {
                if let Some((_, at)) = toks
                    .iter()
                    .find(|t| {
                        t.is_start() && t.local() == "meta" && t.attr("name") == Some("dtb:depth")
                    })
                    .and_then(|t| t.attr_at("content"))
                {
                    text = xml::splice(&text, vec![(at.0, at.1, depth.to_string())]);
                }
            }
        }
        xml::check(&text).map_err(|e| {
            format!(
                "Changing the contents would break {}, so nothing was changed: {e}",
                file_name(&self.file)
            )
        })?;
        Ok(text)
    }
}

/// A `<navPoint …>` tag with its playOrder set to `n` (added when it has none).
fn renumbered(open: &str, n: usize) -> String {
    let Ok(toks) = xml::tokenize(&format!("{open}</navPoint>")) else {
        return open.to_string();
    };
    match toks.first().and_then(|t| t.attr_at("playOrder")) {
        Some((_, at)) => format!("{}{n}{}", &open[..at.0], &open[at.1..]),
        None => {
            let end = open.len() - 1;
            format!("{} playOrder=\"{n}\">", open[..end].trim_end())
        }
    }
}

/// Depths that make a list: the first at 1, each at most one deeper than the one before.
fn normalize(nodes: &mut [Node]) {
    let mut last = 0;
    for n in nodes.iter_mut() {
        n.depth = n.depth.clamp(1, last + 1);
        last = n.depth;
    }
}

impl Book {
    fn toc_lists(&self) -> Result<Vec<List>> {
        let p = self.package()?;
        let mut lists = vec![];
        if let Some(nav) = p
            .items
            .iter()
            .find(|i| i.props.split_whitespace().any(|x| x == "nav"))
        {
            if let Some(l) = self
                .text(&nav.path)
                .ok()
                .and_then(|t| List::nav(&nav.path, t))
            {
                lists.push(l);
            }
        }
        let ncx = p
            .toc_id
            .as_deref()
            .and_then(|id| p.item_by_id(id))
            .or_else(|| {
                p.items
                    .iter()
                    .find(|i| i.media == "application/x-dtbncx+xml")
            });
        if let Some(ncx) = ncx {
            if let Some(l) = self
                .text(&ncx.path)
                .ok()
                .and_then(|t| List::ncx(&ncx.path, t))
            {
                lists.push(l);
            }
        }
        Ok(lists)
    }

    /// The table of contents, in order: the nav's, else the NCX's.
    pub fn toc(&self) -> Result<Vec<TocEntry>> {
        Ok(self
            .toc_lists()?
            .into_iter()
            .next()
            .map(|l| {
                l.nodes
                    .into_iter()
                    .map(|n| TocEntry {
                        label: n.label,
                        path: n.path,
                        fragment: n.fragment,
                        depth: n.depth,
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Where a place is in reading order: its chapter's place in the spine, then where in the
    /// chapter its id is (0 for the chapter's start). Files out of the spine come last.
    fn reading_key(
        &self,
        spine: &[ChapterRef],
        ids: &mut HashMap<String, HashMap<String, usize>>,
        path: &str,
        fragment: Option<&str>,
    ) -> (usize, usize) {
        let Some(k) = spine.iter().position(|c| c.path == path) else {
            return (usize::MAX, 0);
        };
        let Some(f) = fragment else { return (k, 0) };
        let at = ids.entry(path.to_string()).or_insert_with(|| {
            let text = self.text(path).unwrap_or_default();
            xml::tokenize(&text)
                .map(|toks| {
                    toks.iter()
                        .filter(|t| t.is_start())
                        .filter_map(|t| t.attr("id").map(|id| (id.to_string(), t.start)))
                        .collect()
                })
                .unwrap_or_default()
        });
        (k, at.get(f).copied().unwrap_or(0))
    }

    /// Adds an entry to the contents for the place `cut` names in the chapter at `path`: its
    /// start when that's the chapter's first element, else that element, which gets an id if it
    /// has none. `label`, or else the element's text, names it; `level` places it (see `Level`).
    /// `names` are as for `move_part`.
    pub fn add_toc(
        &mut self,
        path: &str,
        cut: Cut,
        names: Option<&[String]>,
        label: &str,
        level: Level,
    ) -> Result<()> {
        let spine = self.chapters()?;
        if !spine.iter().any(|c| c.path == path) {
            return Err(format!(
                "{} isn't one of the book's chapters.",
                file_name(path)
            ));
        }
        let mut lists = self.toc_lists()?;
        if lists.is_empty() {
            return Err("This book has no table of contents to add to.".into());
        }
        let text = self.text(path)?;
        xml::check(&text).map_err(|e| {
            format!(
                "{} isn't valid XHTML, so nothing was added. Fix it in Source first: {e}",
                file_name(path)
            )
        })?;
        let toks = xml::tokenize(&text)?;
        let bl = blocks(&text, &toks, path)?;
        if let Some(names) = names {
            if names.len() != bl.len()
                || names
                    .iter()
                    .zip(&bl)
                    .any(|(n, b)| !n.eq_ignore_ascii_case(&b.2))
            {
                return Err("The visual view doesn't match this chapter's XHTML here. Add the entry from the Source view.".into());
            }
        }
        let k = cut_block(&text, &bl, cut).min(bl.len().saturating_sub(1));
        let start = bl.get(k).map(|b| b.0);
        let tok = start.and_then(|s| toks.iter().position(|t| t.start == s));
        let own = tok
            .map(|i| {
                squash(&xml::inner_text(
                    &text,
                    &toks[i + 1..xml::matching_close(&toks, i).unwrap_or(i).max(i + 1)],
                ))
            })
            .unwrap_or_default();
        let label = Some(squash(label))
            .filter(|l| !l.is_empty())
            .or_else(|| Some(own.chars().take(80).collect::<String>()).filter(|l| !l.is_empty()));
        let label = label
            .or_else(|| {
                spine
                    .iter()
                    .find(|c| c.path == path)
                    .map(|c| c.label.clone())
            })
            .unwrap_or_else(|| file_name(path).to_string());

        // The place: the chapter's start, or the element's id (given one if it has none).
        let mut chapter = None;
        let fragment = match tok.filter(|_| k > 0) {
            None => None,
            Some(i) => match toks[i].attr("id") {
                Some(id) => Some(id.to_string()),
                None => {
                    let taken: std::collections::HashSet<&str> =
                        toks.iter().filter_map(|t| t.attr("id")).collect();
                    let id = (1..)
                        .map(|n| format!("toc-{n}"))
                        .find(|id| !taken.contains(id.as_str()))
                        .unwrap_or_default();
                    let at = toks[i].start + 1 + toks[i].name.len();
                    let new = xml::splice(&text, vec![(at, at, format!(" id=\"{id}\""))]);
                    xml::check(&new).map_err(|e| {
                        format!(
                            "Giving the place an id would break {}: {e}",
                            file_name(path)
                        )
                    })?;
                    chapter = Some(new);
                    Some(id)
                }
            },
        };
        if let Some(new) = &chapter {
            // Its new id counts for the order below.
            self.set_file(path, new.clone().into_bytes());
        }

        let mut ids = HashMap::new();
        let key = self.reading_key(&spine, &mut ids, path, fragment.as_deref());
        let mut written = vec![];
        for l in &mut lists {
            if l.nodes
                .iter()
                .any(|n| n.path == path && n.fragment == fragment)
            {
                return Err(format!(
                    "“{label}” is in the contents already: an entry opens that place."
                ));
            }
            let keys: Vec<(usize, usize)> = l
                .nodes
                .iter()
                .map(|n| self.reading_key(&spine, &mut ids, &n.path, n.fragment.as_deref()))
                .collect();
            let at = keys.iter().position(|k| *k > key).unwrap_or(l.nodes.len());
            let before = at.checked_sub(1).map(|i| l.nodes[i].depth);
            let depth = match (level, before) {
                (_, None) => 1,
                (Level::Chapter, Some(d)) => d,
                (Level::Section, Some(d)) => d.saturating_sub(1).max(1),
                (Level::Subsection, Some(d)) => d + 1,
            };
            let href = {
                let rel = relative(&l.file, path);
                let rel: String = percent_encoding::utf8_percent_encode(&rel, HREF).to_string();
                match &fragment {
                    Some(f) => format!("{rel}#{f}"),
                    None => rel,
                }
            };
            let node = if l.ncx {
                let taken: std::collections::HashSet<String> = l
                    .nodes
                    .iter()
                    .filter_map(|n| {
                        xml::tokenize(&format!("{}</navPoint>", n.open))
                            .ok()?
                            .first()?
                            .attr("id")
                            .map(String::from)
                    })
                    .collect();
                let id = (1..)
                    .map(|n| format!("navPoint-toc-{n}"))
                    .find(|id| !taken.contains(id))
                    .unwrap_or_default();
                Node {
                    open: format!("<navPoint id=\"{id}\" playOrder=\"0\">"),
                    body: format!(
                        "<navLabel><text>{}</text></navLabel><content src=\"{}\"/>",
                        xml::escape_text(&label),
                        xml::escape_attr(&href)
                    ),
                    label: label.clone(),
                    path: path.to_string(),
                    fragment: fragment.clone(),
                    depth,
                }
            } else {
                Node {
                    open: "<li>".into(),
                    body: format!(
                        "<a href=\"{}\">{}</a>",
                        xml::escape_attr(&href),
                        xml::escape_text(&label)
                    ),
                    label: label.clone(),
                    path: path.to_string(),
                    fragment: fragment.clone(),
                    depth,
                }
            };
            l.nodes.insert(at, node);
            normalize(&mut l.nodes);
            written.push((l.file.clone(), l.written()?));
        }
        for (f, t) in written {
            self.set_file(&f, t.into_bytes());
        }
        Ok(())
    }

    /// Takes the contents' entry `index` (as `toc` lists them) out, from the nav and the NCX:
    /// what opens the same place there. What was under it moves up to take its place.
    pub fn remove_toc(&mut self, index: usize) -> Result<()> {
        let entry = self
            .toc()?
            .into_iter()
            .nth(index)
            .ok_or("That entry is no longer in the contents.")?;
        let mut written = vec![];
        for mut l in self.toc_lists()? {
            let Some(k) = l
                .nodes
                .iter()
                .position(|n| n.path == entry.path && n.fragment == entry.fragment)
            else {
                continue;
            };
            let depth = l.nodes[k].depth;
            l.nodes.remove(k);
            // Its children come up a level, so they stay together where it was.
            for n in l.nodes[k..].iter_mut().take_while(|n| n.depth > depth) {
                n.depth -= 1;
            }
            normalize(&mut l.nodes);
            written.push((l.file.clone(), l.written()?));
        }
        for (f, t) in written {
            self.set_file(&f, t.into_bytes());
        }
        Ok(())
    }
}

/// Characters written as %xx in an address made here.
const HREF: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
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
