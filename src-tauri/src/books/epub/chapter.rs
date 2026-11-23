//! A chapter’s text: as shown in the visual editor, its body, and setting either.

use super::*;

impl Book {
    pub(super) fn body_range(toks: &[Tok], path: &str) -> Result<(usize, usize)> {
        let open = toks.iter().position(|t| t.kind == Kind::Open && t.local() == "body").ok_or_else(|| format!("{path} has no <body>."))?;
        let close = xml::matching_close(toks, open).ok_or_else(|| format!("{path}: <body> is never closed."))?;
        Ok((toks[open].end, toks[close].start))
    }

    /// The chapter as HTML for the visual editor: self-closed non-void tags expanded, the XML
    /// declaration dropped, and `<base href=base>` added so its images and styles load.
    pub fn visual_html(&self, path: &str, base: &str) -> Result<String> {
        let text = self.text(path)?;
        xml::check(&text)?;
        let toks = xml::tokenize(&text)?;
        Self::body_range(&toks, path)?;
        let mut out = String::with_capacity(text.len() + 200);
        let mut based = false;
        for t in &toks {
            let raw = &text[t.start..t.end];
            match t.kind {
                Kind::Pi if t.start == 0 || text[..t.start].trim().is_empty() => {}
                Kind::Doctype => out.push_str("<!DOCTYPE html>"),
                Kind::Empty if !VOID.contains(&t.local().to_ascii_lowercase().as_str()) => {
                    out.push_str(raw.trim_end_matches("/>").trim_end());
                    out.push_str(&format!("></{}>", t.name));
                }
                Kind::Open if t.local() == "head" && !based => {
                    out.push_str(raw);
                    out.push_str(&format!(r#"<base href="{}">"#, xml::escape_attr(base)));
                    based = true;
                }
                _ => out.push_str(raw),
            }
        }
        if !based {
            out.insert_str(0, &format!(r#"<base href="{}">"#, xml::escape_attr(base)));
        }
        Ok(out)
    }

    pub fn body_inner(&self, path: &str) -> Result<String> {
        let text = self.text(path)?;
        let toks = xml::tokenize(&text)?;
        let (s, e) = Self::body_range(&toks, path)?;
        Ok(text[s..e].to_string())
    }

    /// Replaces the chapter's whole source after checking it's well-formed.
    pub fn set_source(&mut self, path: &str, source: &str) -> Result<()> {
        self.text(path)?;
        xml::check(source)?;
        if self.text(path)? != source {
            self.set_file(path, source.as_bytes().to_vec());
        }
        Ok(())
    }

    /// Replaces what's inside <body>, keeping the head and the <body> tag itself.
    pub fn set_body(&mut self, path: &str, inner: &str) -> Result<()> {
        let text = self.text(path)?;
        let toks = xml::tokenize(&text)?;
        let (s, e) = Self::body_range(&toks, path)?;
        if &text[s..e] == inner {
            return Ok(());
        }
        xml::check(&format!("<body>{inner}</body>")).map_err(|e| format!("The edited chapter isn't valid XHTML: {e}"))?;
        let new = format!("{}{inner}{}", &text[..s], &text[e..]);
        self.set_file(path, new.into_bytes());
        Ok(())
    }
}
