//! Book info, chapters, cover and stats.

use super::*;

impl Book {
    pub fn meta(&self) -> Result<Meta> {
        let p = self.package()?;
        let first = |l: &str| p.dc(l).first().map(|e| e.text.clone()).unwrap_or_default();
        let all = |l: &str| p.dc(l).iter().map(|e| e.text.clone()).collect();
        let ids = p.dc("identifier");
        let identifier = ids.iter().find(|e| e.attr("id") == Some(&p.unique_id)).or(ids.first()).map(|e| e.text.clone());
        Ok(Meta {
            title: first("title"),
            creators: all("creator"),
            language: first("language"),
            publisher: first("publisher"),
            date: first("date"),
            identifier: identifier.unwrap_or_default(),
            subjects: all("subject"),
            description: first("description"),
        })
    }

    pub fn set_meta(&mut self, m: &Meta) -> Result<()> {
        let clean = |v: &[String]| v.iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect::<Vec<_>>();
        let one = |s: &str| clean(&[s.to_string()]);
        for (v, what) in [(&m.title, "a title"), (&m.language, "a language"), (&m.identifier, "an identifier")] {
            if v.trim().is_empty() {
                return Err(format!("The book needs {what}."));
            }
        }
        let p = self.package()?;
        let mut edits: Vec<(usize, usize, String)> = vec![];
        let mut inserts: BTreeMap<usize, String> = BTreeMap::new();
        let at = p.meta_insert_at();
        let indent = p.indent_of(p.elems.first().map_or(at, |e| e.start));

        let ids = p.dc("identifier");
        let uid = ids.iter().find(|e| e.attr("id") == Some(&p.unique_id)).or(ids.first()).copied();
        let fields: Vec<(&str, Vec<&Elem>, Vec<String>)> = vec![
            ("title", p.dc("title").into_iter().take(1).collect(), one(&m.title)),
            ("creator", p.dc("creator"), clean(&m.creators)),
            ("language", p.dc("language").into_iter().take(1).collect(), one(&m.language)),
            ("publisher", p.dc("publisher").into_iter().take(1).collect(), one(&m.publisher)),
            ("date", p.dc("date").into_iter().take(1).collect(), one(&m.date)),
            ("subject", p.dc("subject"), clean(&m.subjects)),
            ("description", p.dc("description").into_iter().take(1).collect(), one(&m.description)),
            ("identifier", uid.into_iter().collect(), one(&m.identifier)),
        ];

        for (local, old, new) in fields {
            for k in 0..old.len().max(new.len()) {
                match (old.get(k), new.get(k)) {
                    (Some(e), Some(v)) if &e.text == v => {}
                    (Some(e), Some(v)) if e.empty => {
                        let open = p.text[e.start..e.end].trim_end_matches("/>").trim_end();
                        edits.push((e.start, e.end, format!("{open}>{}</{}>", xml::escape_text(v), e.name)));
                    }
                    (Some(e), Some(v)) => edits.push((e.inner.0, e.inner.1, xml::escape_text(v))),
                    (Some(e), None) => {
                        edits.push((line_start(&p.text, e.start), e.end, String::new()));
                        // EPUB 3 <meta refines="#id"> entries describe this element; drop them too.
                        if let Some(id) = e.attr("id") {
                            let target = format!("#{id}");
                            for r in p.elems.iter().filter(|r| r.attr("refines") == Some(target.as_str())) {
                                edits.push((line_start(&p.text, r.start), r.end, String::new()));
                            }
                        }
                    }
                    (None, Some(v)) => {
                        let pos = old.last().map_or(at, |e| e.end);
                        let tag = format!("{}:{local}", p.dc_prefix);
                        inserts.entry(pos).or_default().push_str(&format!("\n{indent}<{tag}>{}</{tag}>", xml::escape_text(v)));
                    }
                    (None, None) => {}
                }
            }
        }
        if edits.is_empty() && inserts.is_empty() {
            return Ok(());
        }
        edits.extend(inserts.into_iter().map(|(pos, s)| (pos, pos, s)));
        dedupe(&mut edits);
        let text = xml::splice(&p.text, edits);
        xml::check(&text).map_err(|e| format!("Updating the book info broke the package document: {e}"))?;
        let opf = self.opf.clone();
        self.set_file(&opf, text.into_bytes());
        Ok(())
    }

    /// Sets <meta property="dcterms:modified"> to now, when the book has one (EPUB 3).
    pub fn touch_modified(&mut self) {
        let Ok(p) = self.package() else { return };
        let Some(e) = p.elems.iter().find(|e| e.local == "meta" && e.attr("property") == Some("dcterms:modified")) else {
            return;
        };
        if e.empty {
            return;
        }
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let text = xml::splice(&p.text, vec![(e.inner.0, e.inner.1, now)]);
        let opf = self.opf.clone();
        self.set_file(&opf, text.into_bytes());
    }

    pub fn chapters(&self) -> Result<Vec<ChapterRef>> {
        let p = self.package()?;
        let labels = self.toc_labels(&p);
        Ok(p.spine
            .iter()
            .filter_map(|id| p.item_by_id(id))
            .filter(|i| i.media.contains("html") && !i.path.is_empty())
            .map(|i| ChapterRef { path: i.path.clone(), label: labels.get(&i.path).cloned().unwrap_or_else(|| unlisted_label(&i.path, &i.props)) })
            .collect())
    }

    /// Chapter path → its first title in the table of contents (EPUB 3 nav, else the EPUB 2 NCX).
    pub(super) fn toc_labels(&self, p: &Package) -> HashMap<String, String> {
        let mut out = HashMap::new();
        if let Some(nav) = p.items.iter().find(|i| i.props.split_whitespace().any(|x| x == "nav")) {
            if let Some((s, toks)) = self.text(&nav.path).ok().and_then(|s| xml::tokenize(&s).ok().map(|t| (s, t))) {
                let navs: Vec<usize> = (0..toks.len()).filter(|&k| toks[k].kind == Kind::Open && toks[k].local() == "nav").collect();
                let toc = navs
                    .iter()
                    .copied()
                    .find(|&k| toks[k].attr_local("type").is_some_and(|v| v.split_whitespace().any(|x| x == "toc")))
                    .or(navs.first().copied());
                if let Some(start) = toc {
                    let end = xml::matching_close(&toks, start).unwrap_or(toks.len() - 1);
                    for k in start..end {
                        let t = &toks[k];
                        if t.kind == Kind::Open && t.local() == "a" {
                            if let (Some(href), Some(close)) = (t.attr("href"), xml::matching_close(&toks, k)) {
                                let label = squash(&xml::inner_text(&s, &toks[k + 1..close]));
                                if let Some(path) = resolve(&nav.path, href) {
                                    if !label.is_empty() {
                                        out.entry(path).or_insert(label);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if !out.is_empty() {
            return out;
        }
        let ncx = p.toc_id.as_deref().and_then(|id| p.item_by_id(id)).or_else(|| p.items.iter().find(|i| i.media == "application/x-dtbncx+xml"));
        if let Some(ncx) = ncx {
            if let Some((s, toks)) = self.text(&ncx.path).ok().and_then(|s| xml::tokenize(&s).ok().map(|t| (s, t))) {
                let mut label = String::new();
                for (k, t) in toks.iter().enumerate() {
                    if t.kind == Kind::Open && t.local() == "text" {
                        if let Some(close) = xml::matching_close(&toks, k) {
                            label = squash(&xml::inner_text(&s, &toks[k + 1..close]));
                        }
                    } else if t.is_start() && t.local() == "content" {
                        if let Some(path) = t.attr("src").and_then(|src| resolve(&ncx.path, src)) {
                            if !label.is_empty() {
                                out.entry(path).or_insert_with(|| label.clone());
                            }
                        }
                    }
                }
            }
        }
        out
    }

    /// Gives the chapter at `path` a new name in the table of contents: in the EPUB 3 nav and the
    /// EPUB 2 NCX, whichever the book has, the first entry for it in each (the one its name is
    /// read from). The entry's text is replaced; its link and the rest of the file keep their bytes.
    pub fn rename_chapter(&mut self, path: &str, label: &str) -> Result<()> {
        let label = squash(label);
        if label.is_empty() {
            return Err("Type a name for the chapter.".into());
        }
        let p = self.package()?;
        let mut changed = vec![];
        if let Some(nav) = p.items.iter().find(|i| i.props.split_whitespace().any(|x| x == "nav")) {
            if let Some((s, toks)) = self.text(&nav.path).ok().and_then(|s| xml::tokenize(&s).ok().map(|t| (s, t))) {
                let navs: Vec<usize> = (0..toks.len()).filter(|&k| toks[k].kind == Kind::Open && toks[k].local() == "nav").collect();
                let toc = navs
                    .iter()
                    .copied()
                    .find(|&k| toks[k].attr_local("type").is_some_and(|v| v.split_whitespace().any(|x| x == "toc")))
                    .or(navs.first().copied());
                if let Some(start) = toc {
                    let end = xml::matching_close(&toks, start).unwrap_or(toks.len() - 1);
                    let hit = (start..end).find_map(|k| {
                        let t = &toks[k];
                        let to = t.attr("href").and_then(|h| resolve(&nav.path, h));
                        (t.kind == Kind::Open && t.local() == "a" && to.as_deref() == Some(path))
                            .then(|| xml::matching_close(&toks, k).map(|c| (t.end, toks[c].start)))?
                    });
                    if let Some((a, b)) = hit {
                        changed.push((nav.path.clone(), xml::splice(&s, vec![(a, b, xml::escape_text(&label))])));
                    }
                }
            }
        }
        let ncx = p.toc_id.as_deref().and_then(|id| p.item_by_id(id)).or_else(|| p.items.iter().find(|i| i.media == "application/x-dtbncx+xml"));
        if let Some(ncx) = ncx {
            if let Some((s, toks)) = self.text(&ncx.path).ok().and_then(|s| xml::tokenize(&s).ok().map(|t| (s, t))) {
                let mut text = None;
                for (k, t) in toks.iter().enumerate() {
                    if t.kind == Kind::Open && t.local() == "text" {
                        text = xml::matching_close(&toks, k).map(|c| (t.end, toks[c].start));
                    } else if t.is_start() && t.local() == "content" && t.attr("src").and_then(|src| resolve(&ncx.path, src)).as_deref() == Some(path) {
                        if let Some((a, b)) = text {
                            changed.push((ncx.path.clone(), xml::splice(&s, vec![(a, b, xml::escape_text(&label))])));
                        }
                        break;
                    }
                }
            }
        }
        if changed.is_empty() {
            return Err(format!("{} isn't in the table of contents, so it has no name there to change.", file_name(path)));
        }
        for (f, text) in &changed {
            xml::check(text).map_err(|e| format!("Renaming the chapter would break {}, so nothing was changed: {e}", file_name(f)))?;
        }
        for (f, text) in changed {
            self.set_file(&f, text.into_bytes());
        }
        Ok(())
    }

    pub fn cover_path(&self) -> Option<String> {
        let p = self.package().ok()?;
        p.cover().map(|i| i.path.clone()).filter(|path| self.file(path).is_some())
    }

    /// Replaces the cover image, or adds one when the book has none.
    pub fn set_cover(&mut self, data: Vec<u8>, ext: &str) -> Result<()> {
        let media = media_for_ext(ext).ok_or("Use a JPEG, PNG, GIF, WebP or SVG image for the cover.")?;
        let p = self.package()?;
        let mut edits = vec![];
        let mut renamed = None;
        let path = if let Some(item) = p.cover() {
            if item.media == media {
                item.path.clone()
            } else {
                // A new type gets a name to match (cover.jpg becomes cover.png), and what names
                // the old one is changed to name the new.
                let ext = ext.to_ascii_lowercase();
                let (stem, _) = item.path.rsplit_once('.').filter(|(s, _)| !s.ends_with('/')).unwrap_or((&item.path, ""));
                let new = (0..)
                    .map(|n| if n == 0 { format!("{stem}.{ext}") } else { format!("{stem}-{n}.{ext}") })
                    .find(|p| self.file(p).is_none())
                    .unwrap_or_default();
                if let Some((_, at)) = item.tok.attr_at("media-type") {
                    edits.push((at.0, at.1, media.to_string()));
                }
                if let Some((_, at)) = item.tok.attr_at("href") {
                    edits.push((at.0, at.1, renamed_href(&p.text[at.0..at.1], &new[stem.len()..])));
                }
                renamed = Some((item.path.clone(), new.clone(), new[stem.len()..].to_string()));
                new
            }
        } else {
            let ext = ext.to_ascii_lowercase();
            let base = dir_of(&self.opf).to_string();
            let mut n = 0;
            let path = loop {
                let name = if n == 0 { format!("images/cover.{ext}") } else { format!("images/cover-{n}.{ext}") };
                if self.file(&format!("{base}{name}")).is_none() {
                    break format!("{base}{name}");
                }
                n += 1;
            };
            let mut n = 0;
            let id = loop {
                let id = if n == 0 { "cover-image".to_string() } else { format!("cover-image-{n}") };
                if p.item_by_id(&id).is_none() {
                    break id;
                }
                n += 1;
            };
            let props = if p.version.starts_with('3') { r#" properties="cover-image""# } else { "" };
            let last = p.items.last().map_or(p.manifest_close, |i| i.tok.end);
            let indent = p.indent_of(p.items.last().map_or(p.manifest_close, |i| i.tok.start));
            let href = xml::escape_attr(&relative(&self.opf, &path));
            edits.push((last, last, format!(r#"{}{indent}<item id="{id}" href="{href}" media-type="{media}"{props}/>"#, "\n")));
            if !p.elems.iter().any(|e| e.local == "meta" && e.attr("name") == Some("cover")) {
                let at = p.meta_insert_at();
                let indent = p.indent_of(p.elems.first().map_or(at, |e| e.start));
                edits.push((at, at, format!("\n{indent}<meta name=\"cover\" content=\"{id}\"/>")));
            }
            path
        };
        if !edits.is_empty() {
            let text = xml::splice(&p.text, edits);
            xml::check(&text).map_err(|e| format!("Updating the cover broke the package document: {e}"))?;
            let opf = self.opf.clone();
            self.set_file(&opf, text.into_bytes());
        }
        if let Some((old, new, tail)) = renamed {
            self.relink(&old, &tail)?;
            if let Some(f) = self.files.iter_mut().find(|(n, _)| *n == old) {
                f.0 = new;
            }
        }
        self.set_file(&path, data);
        Ok(())
    }

    /// Changes what names the file `old` in the book's pages and style sheets (links, pictures,
    /// `url()`) to name the file renamed from it: the same name up to its extension, and `tail`
    /// (".png", or "-1.png").
    fn relink(&mut self, old: &str, tail: &str) -> Result<()> {
        let name = tail;
        let p = self.package()?;
        let files: Vec<(String, bool)> = p
            .items
            .iter()
            .filter(|i| i.media.contains("html") || i.media.contains("svg") || i.media == "text/css")
            .map(|i| (i.path.clone(), i.media == "text/css"))
            .collect();
        for (f, css) in files {
            let Ok(text) = self.text(&f) else { continue };
            let mut edits = vec![];
            if css {
                for (s, e) in css_urls(&text, 0) {
                    if resolve(&f, &text[s..e]).as_deref() == Some(old) {
                        edits.push((s, e, renamed_href(&text[s..e], name)));
                    }
                }
            } else {
                let Ok(toks) = xml::tokenize(&text) else { continue };
                for t in toks.iter().filter(|t| t.is_start()) {
                    for (k, (a, v)) in t.attrs.iter().enumerate() {
                        let (s, e) = t.vals[k];
                        if matches!(xml::local(a), "href" | "src" | "poster" | "data") && resolve(&f, v).as_deref() == Some(old) {
                            edits.push((s, e, renamed_href(&text[s..e], name)));
                        } else if a == "style" {
                            for (us, ue) in css_urls(&text[s..e], s) {
                                if resolve(&f, &xml::unescape(&text[us..ue])).as_deref() == Some(old) {
                                    edits.push((us, ue, renamed_href(&text[us..ue], name)));
                                }
                            }
                        }
                    }
                    if t.kind == Kind::Open && t.local() == "style" {
                        // A <style> element's text: its url()s, up to </style>.
                        let end = text[t.end..].find("</").map_or(text.len(), |k| t.end + k);
                        for (s, e) in css_urls(&text[t.end..end], t.end) {
                            if resolve(&f, &text[s..e]).as_deref() == Some(old) {
                                edits.push((s, e, renamed_href(&text[s..e], name)));
                            }
                        }
                    }
                }
            }
            if !edits.is_empty() {
                let changed = xml::splice(&text, edits);
                if !css {
                    xml::check(&changed).map_err(|e| format!("Renaming the cover broke {}: {e}", file_name(&f)))?;
                }
                self.set_file(&f, changed.into_bytes());
            }
        }
        Ok(())
    }

    pub fn stats(&self) -> Result<Stats> {
        let p = self.package()?;
        let count = |f: &dyn Fn(&str) -> bool| p.items.iter().filter(|i| f(&i.media)).count();
        Ok(Stats {
            version: p.version.clone(),
            chapters: self.chapters()?.len(),
            files: self.files.len(),
            bytes: self.files.iter().map(|(_, d)| d.len() as u64).sum(),
            images: count(&|m| m.starts_with("image/")),
            styles: count(&|m| m == "text/css"),
            fonts: count(&|m| m.contains("font") || m.contains("opentype")),
        })
    }
}

/// The label of a chapter the table of contents doesn't name: the table of contents itself (the
/// EPUB 3 nav document, or a file named toc) says so; another, its file name.
pub(super) fn unlisted_label(path: &str, props: &str) -> String {
    let name = file_name(path);
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
    if props.split_whitespace().any(|x| x == "nav") || stem.eq_ignore_ascii_case("toc") {
        "Table of contents".to_string()
    } else {
        name.to_string()
    }
}
