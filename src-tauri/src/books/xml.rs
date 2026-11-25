//! A small XML tokenizer that keeps byte offsets. Edits splice the original text at those offsets,
//! so everything an edit doesn't touch stays byte-for-byte the same (formatting, entities, comments).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Open,
    Close,
    Empty,
    Text,
    Comment,
    CData,
    Pi,
    Doctype,
}

#[derive(Debug, Clone)]
pub struct Tok {
    pub kind: Kind,
    pub start: usize,
    pub end: usize,
    /// Tag name with any prefix (`dc:title`); empty for non-tags.
    pub name: String,
    /// Attributes with entities decoded.
    pub attrs: Vec<(String, String)>,
    /// Where each attribute's value is written (without its quotes), in the order of `attrs`.
    pub vals: Vec<(usize, usize)>,
}

impl Tok {
    fn plain(kind: Kind, start: usize, end: usize) -> Tok {
        Tok { kind, start, end, name: String::new(), attrs: vec![], vals: vec![] }
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// An attribute matched by local name, whatever its prefix (`epub:type` or `type`).
    pub fn attr_local(&self, local_name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| local(k) == local_name).map(|(_, v)| v.as_str())
    }

    pub fn local(&self) -> &str {
        local(&self.name)
    }

    /// The attribute `name` and where its value is written.
    pub fn attr_at(&self, name: &str) -> Option<(&str, (usize, usize))> {
        let k = self.attrs.iter().position(|(n, _)| n == name)?;
        Some((self.attrs[k].1.as_str(), self.vals[k]))
    }

    pub fn is_start(&self) -> bool {
        matches!(self.kind, Kind::Open | Kind::Empty)
    }
}

pub fn local(name: &str) -> &str {
    name.rsplit_once(':').map_or(name, |(_, l)| l)
}

pub fn line_of(s: &str, pos: usize) -> usize {
    s[..pos.min(s.len())].matches('\n').count() + 1
}

mod entities;
use entities::ENTITIES;

pub fn tokenize(s: &str) -> Result<Vec<Tok>, String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let rest = &s[i..];
        if !rest.starts_with('<') {
            let end = rest.find('<').map_or(s.len(), |k| i + k);
            out.push(Tok::plain(Kind::Text, i, end));
            i = end;
            continue;
        }
        let special = if rest.starts_with("<!--") {
            Some(("-->", Kind::Comment, "comment"))
        } else if rest.starts_with("<![CDATA[") {
            Some(("]]>", Kind::CData, "CDATA section"))
        } else if rest.starts_with("<?") {
            Some(("?>", Kind::Pi, "<?…?> instruction"))
        } else if rest.starts_with("<!") {
            Some((">", Kind::Doctype, "<!DOCTYPE>"))
        } else {
            None
        };
        if let Some((pat, kind, what)) = special {
            let end = if kind == Kind::Doctype { doctype_end(rest) } else { rest[2..].find(pat).map(|k| k + 2 + pat.len()) };
            let end = end.ok_or_else(|| format!("Line {}: this {what} never ends.", line_of(s, i)))?;
            out.push(Tok::plain(kind, i, i + end));
            i += end;
            continue;
        }
        let tok = tag(s, i)?;
        i = tok.end;
        out.push(tok);
    }
    Ok(out)
}

/// End of a `<!DOCTYPE …>`, which may hold an internal subset in [ ].
fn doctype_end(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    let mut quote = None;
    for (k, c) in s.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '[') => depth += 1,
            (None, ']') => depth -= 1,
            (None, '>') if depth <= 0 => return Some(k + 1),
            _ => {}
        }
    }
    None
}

fn tag(s: &str, start: usize) -> Result<Tok, String> {
    let b = s.as_bytes();
    let unclosed = || format!("Line {}: this tag never ends (no \">\").", line_of(s, start));
    let mut i = start + 1;
    let closing = b.get(i) == Some(&b'/');
    if closing {
        i += 1;
    }
    let name_start = i;
    while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' && b[i] != b'/' {
        i += 1;
    }
    let name = s[name_start..i].to_string();
    if name.is_empty() {
        return Err(format!("Line {}: a \"<\" that doesn't start a tag. Write &lt; for a literal <.", line_of(s, start)));
    }
    let (mut attrs, mut vals) = (vec![], vec![]);
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        match b.get(i) {
            None => return Err(unclosed()),
            Some(b'>') => {
                let kind = if closing { Kind::Close } else { Kind::Open };
                return Ok(Tok { kind, start, end: i + 1, name, attrs, vals });
            }
            Some(b'/') if b.get(i + 1) == Some(&b'>') && !closing => {
                return Ok(Tok { kind: Kind::Empty, start, end: i + 2, name, attrs, vals });
            }
            Some(b'/') => i += 1,
            Some(_) => {
                let an = i;
                while i < b.len() && !b[i].is_ascii_whitespace() && !matches!(b[i], b'=' | b'>' | b'/') {
                    i += 1;
                }
                let aname = s[an..i].to_string();
                while i < b.len() && b[i].is_ascii_whitespace() {
                    i += 1;
                }
                let mut value = String::new();
                let mut span = (i, i);
                if b.get(i) == Some(&b'=') {
                    i += 1;
                    while i < b.len() && b[i].is_ascii_whitespace() {
                        i += 1;
                    }
                    match b.get(i) {
                        Some(&q @ (b'"' | b'\'')) => {
                            let vs = i + 1;
                            let ve = s[vs..].find(q as char).map(|k| vs + k).ok_or_else(unclosed)?;
                            value = unescape(&s[vs..ve]);
                            span = (vs, ve);
                            i = ve + 1;
                        }
                        _ => {
                            let vs = i;
                            while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' {
                                i += 1;
                            }
                            value = unescape(&s[vs..i]);
                            span = (vs, i);
                        }
                    }
                }
                attrs.push((aname, value));
                vals.push(span);
            }
        }
    }
}

/// Index of the token that closes the element opened at `toks[i]`. Assumes balanced tags.
pub fn matching_close(toks: &[Tok], i: usize) -> Option<usize> {
    if toks[i].kind == Kind::Empty {
        return Some(i);
    }
    let mut depth = 0;
    for (k, t) in toks.iter().enumerate().skip(i + 1) {
        match t.kind {
            Kind::Open => depth += 1,
            Kind::Close if depth == 0 => return Some(k),
            Kind::Close => depth -= 1,
            _ => {}
        }
    }
    None
}

/// Checks that `s` is well-formed: tags balanced, one root element, no stray < or &.
/// The error names the line, in words a person can act on.
pub fn check(s: &str) -> Result<(), String> {
    let toks = tokenize(s)?;
    let mut stack: Vec<(&str, usize)> = vec![];
    let mut roots = 0;
    for t in &toks {
        match t.kind {
            Kind::Open => {
                if stack.is_empty() {
                    roots += 1;
                }
                stack.push((&t.name, t.start));
            }
            Kind::Empty if stack.is_empty() => roots += 1,
            Kind::Close => match stack.pop() {
                Some((n, _)) if n == t.name => {}
                Some((n, at)) => return Err(format!("Line {}: </{}> doesn't match <{n}> opened on line {}.", line_of(s, t.start), t.name, line_of(s, at))),
                None => return Err(format!("Line {}: </{}> has no start tag.", line_of(s, t.start), t.name)),
            },
            Kind::Text => {
                let text = &s[t.start..t.end];
                if stack.is_empty() && !text.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}').is_empty() {
                    return Err(format!("Line {}: text outside the root element.", line_of(s, t.start)));
                }
                if let Some(k) = bare_ampersand(text) {
                    return Err(format!("Line {}: a bare \"&\". Write &amp; for a literal &.", line_of(s, t.start + k)));
                }
            }
            _ => {}
        }
    }
    if let Some((n, at)) = stack.pop() {
        return Err(format!("Line {}: <{n}> is never closed.", line_of(s, at)));
    }
    if roots != 1 {
        return Err("The document needs exactly one root element.".into());
    }
    Ok(())
}

/// Offset of the first `&` that doesn't start an entity reference like `&amp;` or `&#160;`.
fn bare_ampersand(text: &str) -> Option<usize> {
    text.match_indices('&').map(|(k, _)| k).find(|&k| {
        let rest = &text[k + 1..];
        let Some(end) = rest.find(';') else { return true };
        let name = &rest[..end];
        let ok = if let Some(n) = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
            !n.is_empty() && n.chars().all(|c| c.is_ascii_hexdigit())
        } else if let Some(n) = name.strip_prefix('#') {
            !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())
        } else {
            name.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_')
                && name.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
        };
        !ok
    })
}

fn entity(name: &str) -> Option<char> {
    if let Some(n) = name.strip_prefix('#') {
        let v = match n.strip_prefix('x').or_else(|| n.strip_prefix('X')) {
            Some(h) => u32::from_str_radix(h, 16).ok()?,
            None => n.parse().ok()?,
        };
        return char::from_u32(v);
    }
    if name == "apos" {
        return Some('\'');
    }
    // XML's five, and the HTML names EPUB 2 books may use (the XHTML DTDs define them).
    let k = ENTITIES.binary_search_by(|(n, _)| (*n).cmp(name)).ok()?;
    char::from_u32(ENTITIES[k].1)
}

/// The length of the entity reference at the start of `s` (which starts with `&`), if it is
/// one, and its character, if it's one this file knows.
fn entity_at(s: &str) -> Option<(usize, Option<char>)> {
    let end = s.get(1..)?.find(';').filter(|&e| e <= 32 && !s[1..e + 1].contains(char::is_whitespace))?;
    Some((end + 2, entity(&s[1..end + 1])))
}

/// The decoded text of `s[from..to]`, and for each place in it (each byte offset, and its end)
/// where that place is in `s`. A place inside a character written as a reference has none, and
/// neither has one inside a reference that isn't decoded (`&custom;`, kept as written), so an
/// edit never starts or ends there.
pub fn decode_at(s: &str, from: usize, to: usize) -> (String, Vec<Option<usize>>) {
    let (mut out, mut at) = (String::with_capacity(to - from), Vec::with_capacity(to - from + 1));
    let mut i = from;
    while i < to {
        let rest = &s[i..to];
        if let Some((len, c)) = rest.starts_with('&').then(|| entity_at(rest)).flatten() {
            let written = match c {
                Some(c) => c.encode_utf8(&mut [0; 4]).to_string(),
                None => rest[..len].to_string(),
            };
            at.push(Some(i));
            at.extend(std::iter::repeat_n(None, written.len() - 1));
            out.push_str(&written);
            i += len;
            continue;
        }
        let c = rest.chars().next().unwrap_or_default();
        at.extend((0..c.len_utf8()).map(|k| Some(i + k)));
        out.push(c);
        i += c.len_utf8();
    }
    at.push(Some(to));
    (out, at)
}

/// Decodes entity references. Unknown ones are left as written.
pub fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(k) = rest.find('&') {
        out.push_str(&rest[..k]);
        match entity_at(&rest[k..]) {
            Some((len, Some(c))) => {
                out.push(c);
                rest = &rest[k + len..];
            }
            _ => {
                out.push('&');
                rest = &rest[k + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

pub fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub fn escape_attr(s: &str) -> String {
    escape_text(s).replace('"', "&quot;")
}

/// The decoded text inside `toks[from..to]`, tags dropped.
pub fn inner_text(s: &str, toks: &[Tok]) -> String {
    let mut out = String::new();
    for t in toks {
        match t.kind {
            Kind::Text => out.push_str(&unescape(&s[t.start..t.end])),
            Kind::CData => out.push_str(&s[t.start + 9..t.end - 3]),
            _ => {}
        }
    }
    out
}

/// Applies (start, end, replacement) edits to `s`. Edits must not overlap; insertions have start == end.
pub fn splice(s: &str, mut edits: Vec<(usize, usize, String)>) -> String {
    edits.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    let mut out = s.to_string();
    for (start, end, text) in edits {
        out.replace_range(start..end, &text);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_and_offsets() {
        let s = r#"<?xml version="1.0"?><a x='1 &amp; 2'><b/>hi &amp; bye<!-- c --></a>"#;
        let t = tokenize(s).unwrap();
        let kinds: Vec<Kind> = t.iter().map(|t| t.kind).collect();
        assert_eq!(kinds, [Kind::Pi, Kind::Open, Kind::Empty, Kind::Text, Kind::Comment, Kind::Close]);
        assert_eq!(t[1].attr("x"), Some("1 & 2"));
        assert_eq!(&s[t[3].start..t[3].end], "hi &amp; bye");
        assert_eq!(unescape("hi &amp; bye &foo; &#x41;"), "hi & bye &foo; A");
        assert_eq!(matching_close(&t, 1), Some(5));
    }

    #[test]
    fn checks() {
        assert!(check("<a><b></b></a>").is_ok());
        assert!(check("<!DOCTYPE html [ <!ENTITY x 'y'> ]>\n<a>&x;</a>").is_ok());
        assert!(check("<a><b></a>").unwrap_err().contains("doesn't match"));
        assert!(check("<a>\n<b>").unwrap_err().contains("Line 2"));
        assert!(check("<a>fish & chips</a>").unwrap_err().contains("bare"));
        assert!(check("<a>1 < 2</a>").is_err());
        assert!(check("<a/><b/>").is_err());
    }

    #[test]
    fn decodes_with_places() {
        assert_eq!(unescape("&eacute;t&eacute; &custom; &#233;"), "été &custom; é");
        let s = "a&eacute;&x;b";
        let (plain, at) = decode_at(s, 0, s.len());
        assert_eq!(plain, "aé&x;b");
        assert_eq!(at.len(), plain.len() + 1);
        assert_eq!((at[0], at[1], at[2], at[3], at[4]), (Some(0), Some(1), None, Some(9), None));
        assert_eq!((at[6], at[7]), (Some(12), Some(13)));
        let t = tokenize(r#"<a href="x.html" b='1'/>"#).unwrap();
        assert_eq!(t[0].attr_at("href"), Some(("x.html", (9, 15))));
    }

    #[test]
    fn splices() {
        assert_eq!(splice("abcdef", vec![(1, 2, "X".into()), (4, 4, "Y".into()), (5, 6, "".into())]), "aXcdYe");
    }
}
