//! A YAML reader for config files (`ferry.yaml`): the YAML that docker-compose files, workflows
//! and app settings are written in, as YAML 1.2 reads it.
//!
//! It reads maps and lists by indentation and in brackets (`[a, b]`, `{x: 1}`), plain, 'single'
//! and "double" quoted text, `|` and `>` text over several lines, comments, and several
//! documents split by `---`. Plain values that are numbers, `true`/`false` or `null` become
//! those; anything else, and anything quoted, stays text (so `no` and `on` are text, as in YAML
//! 1.2). Keys are always text.
//!
//! What it doesn't read is refused with the line it's on, never guessed at: anchors and aliases
//! (`&a`, `*a`), tags (`!!str`), and `? ` keys.

use super::Data;

/// The documents in `text`: one for most files, none read as one empty (null) document.
pub(in crate::scripts) fn parse(text: &str) -> Result<Vec<Data>, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let lines = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
        .collect();
    let mut p = Parser { lines, i: 0 };
    p.documents()
}

fn at(line: usize, msg: impl std::fmt::Display) -> String {
    format!("line {}: {msg}", line + 1)
}

fn indent(l: &str) -> usize {
    l.len() - l.trim_start_matches(' ').len()
}

/// Empty, or only a comment.
fn blank(l: &str) -> bool {
    let t = l.trim_start_matches([' ', '\t']);
    t.is_empty() || t.starts_with('#')
}

/// `---` or `...` at the start of a line, alone or before a space.
fn marker(l: &str, m: &str) -> bool {
    l.strip_prefix(m)
        .is_some_and(|r| r.is_empty() || r.starts_with([' ', '\t']))
}

fn is_marker(l: &str) -> bool {
    marker(l, "---") || marker(l, "...")
}

/// A list item: `-` alone or before a space.
fn is_item(c: &str) -> bool {
    c == "-" || c.starts_with("- ")
}

/// Where the comment starts in `l` from `from`: a `#` at `from` or after a space or tab.
fn comment_at(l: &str, from: usize) -> Option<usize> {
    let b = l.as_bytes();
    (from..b.len()).find(|&k| b[k] == b'#' && (k == from || matches!(b[k - 1], b' ' | b'\t')))
}

struct Parser {
    lines: Vec<String>,
    /// The next line to read.
    i: usize,
}

impl Parser {
    /// The next line with something on it, from `self.i` (which moves to it).
    fn next(&mut self) -> Result<Option<usize>, String> {
        while self.i < self.lines.len() {
            let l = &self.lines[self.i];
            if !blank(l) {
                let lead = &l[..l.len() - l.trim_start_matches([' ', '\t']).len()];
                if lead.contains('\t') {
                    return Err(at(self.i, "a tab indents this line; YAML needs spaces"));
                }
                return Ok(Some(self.i));
            }
            self.i += 1;
        }
        Ok(None)
    }

    fn documents(&mut self) -> Result<Vec<Data>, String> {
        let mut docs = Vec::new();
        loop {
            let Some(i) = self.next()? else { break };
            let l = &self.lines[i];
            if l.starts_with('%') {
                // A directive (%YAML 1.2): nothing to read in it.
                self.i += 1;
                continue;
            }
            if marker(l, "...") {
                self.i += 1;
                continue;
            }
            if marker(l, "---") {
                // What follows on the line belongs to the document.
                self.lines[i].replace_range(0..3, "   ");
            }
            let v = match self.next()? {
                Some(j) if !is_marker(&self.lines[j]) => self.block(-1)?,
                _ => Data::Null,
            };
            docs.push(v);
            match self.next()? {
                None => break,
                Some(j) if is_marker(&self.lines[j]) => {}
                Some(j) => {
                    return Err(at(
                        j,
                        "this line isn't indented to fit with the lines above it",
                    ))
                }
            }
        }
        if docs.is_empty() {
            docs.push(Data::Null);
        }
        Ok(docs)
    }

    /// The node starting at the next line, which is indented more than `parent`.
    fn block(&mut self, parent: isize) -> Result<Data, String> {
        let i = self.i;
        let n = indent(&self.lines[i]);
        let c = &self.lines[i][n..];
        if is_item(c) {
            return self.seq(n);
        }
        if c.starts_with(['|', '>']) {
            return self.block_text(parent, i, n);
        }
        self.unsupported(i, n)?;
        if self.key_at(i, n)?.is_some() {
            return self.map(n);
        }
        self.value(parent, i, n)
    }

    /// Refuses what this reader doesn't read, at line `i`, column `col`.
    fn unsupported(&self, i: usize, col: usize) -> Result<(), String> {
        let c = &self.lines[i][col..];
        let what = match c.as_bytes().first() {
            Some(b'&') => "anchors (&name) aren't read: write the value out in full",
            Some(b'*') => "aliases (*name) aren't read: write the value out in full",
            Some(b'!') => "tags (!name) aren't read",
            Some(b'?') if c.len() == 1 || c[1..].starts_with([' ', '\t']) => {
                "keys written with \"? \" aren't read"
            }
            Some(b'@' | b'`') => "a value can't start with @ or `; put it in quotes",
            _ => return Ok(()),
        };
        Err(at(i, what))
    }

    /// The key at line `i`, column `col`, and where its value starts, when the line is
    /// `key: value` (or `key:`).
    fn key_at(&self, i: usize, col: usize) -> Result<Option<(String, usize)>, String> {
        let l = &self.lines[i];
        let b = l.as_bytes();
        let colon =
            |k: usize| b.get(k) == Some(&b':') && matches!(b.get(k + 1), None | Some(b' ' | b'\t'));
        match b.get(col) {
            Some(&q @ (b'"' | b'\'')) => {
                let (key, li, after) = self.quoted(i, col, q)?;
                if li != i {
                    return Ok(None);
                }
                let k =
                    after + (l[after..].len() - l[after..].trim_start_matches([' ', '\t']).len());
                Ok(colon(k).then(|| (key, k + 1)))
            }
            Some(b'[' | b'{') | None => Ok(None),
            Some(_) => {
                let end = comment_at(l, col).unwrap_or(l.len());
                Ok((col..end)
                    .find(|&k| colon(k))
                    .map(|k| (l[col..k].trim_end().to_string(), k + 1)))
            }
        }
    }

    /// A map whose keys are at column `n`.
    fn map(&mut self, n: usize) -> Result<Data, String> {
        let mut out: Vec<(String, Data)> = Vec::new();
        loop {
            let Some(i) = self.next()? else { break };
            let l = &self.lines[i];
            let k = indent(l);
            if k < n || is_marker(l) {
                break;
            }
            if k > n {
                return Err(at(i, "this line is indented more than the key above it"));
            }
            if is_item(&l[n..]) {
                return Err(at(i, "a list item (- ) where a key: value was expected"));
            }
            self.unsupported(i, n)?;
            let Some((key, from)) = self.key_at(i, n)? else {
                return Err(at(i, "a key: value was expected here"));
            };
            if out.iter().any(|(k, _)| *k == key) {
                return Err(at(i, format!("the key \"{key}\" is there twice")));
            }
            let v = self.after_key(i, n, from)?;
            out.push((key, v));
        }
        Ok(Data::Map(out))
    }

    /// The value of a key at column `n` of line `i`, which starts at column `from`.
    fn after_key(&mut self, i: usize, n: usize, from: usize) -> Result<Data, String> {
        let l = &self.lines[i];
        let col = from + (l[from..].len() - l[from..].trim_start_matches([' ', '\t']).len());
        if col >= l.len() || comment_at(l, col) == Some(col) {
            // On the lines below: more indented, or a list at the key's own indent.
            self.i = i + 1;
            return Ok(match self.next()? {
                Some(j) if indent(&self.lines[j]) > n && !is_marker(&self.lines[j]) => {
                    self.block(n as isize)?
                }
                Some(j) if indent(&self.lines[j]) == n && is_item(&self.lines[j][n..]) => {
                    self.seq(n)?
                }
                _ => Data::Null,
            });
        }
        self.unsupported(i, col)?;
        if l[col..].starts_with(['|', '>']) {
            return self.block_text(n as isize, i, col);
        }
        self.value(n as isize, i, col)
    }

    /// A list whose `-` are at column `n`.
    fn seq(&mut self, n: usize) -> Result<Data, String> {
        let mut out = Vec::new();
        loop {
            let Some(i) = self.next()? else { break };
            let l = &self.lines[i];
            let k = indent(l);
            if k < n || is_marker(l) {
                break;
            }
            if k > n {
                return Err(at(
                    i,
                    "this line is indented more than the list item above it",
                ));
            }
            if !is_item(&l[n..]) {
                break;
            }
            let col = n + 1 + (l[n + 1..].len() - l[n + 1..].trim_start_matches(' ').len());
            if col >= l.len() || comment_at(l, col) == Some(col) {
                self.i = i + 1;
                out.push(match self.next()? {
                    Some(j) if indent(&self.lines[j]) > n && !is_marker(&self.lines[j]) => {
                        self.block(n as isize)?
                    }
                    _ => Data::Null,
                });
                continue;
            }
            // The item's content as if it were a line of its own, indented to where it starts:
            // a map in it then goes on at that indent on the lines below.
            self.lines[i].replace_range(n..n + 1, " ");
            out.push(self.block(n as isize)?);
        }
        Ok(Data::List(out))
    }

    /// A value written on line `i` from column `col`: quoted, in brackets, or plain (which may
    /// go on over lines indented more than `parent`).
    fn value(&mut self, parent: isize, i: usize, col: usize) -> Result<Data, String> {
        let (v, li, after) = match self.lines[i].as_bytes()[col] {
            q @ (b'"' | b'\'') => {
                let (s, li, after) = self.quoted(i, col, q)?;
                (Data::Str(s), li, after)
            }
            b'[' | b'{' => {
                let mut cur = (i, col);
                let v = self.flow(&mut cur)?;
                (v, cur.0, cur.1)
            }
            _ => return self.plain(parent, i, col),
        };
        let l = &self.lines[li];
        let rest = l[after..].trim_start_matches([' ', '\t']);
        if !rest.is_empty() && !(rest.starts_with('#') && rest.len() < l.len() - after) {
            return Err(at(
                li,
                format!("\"{}\" after the value isn't read", rest.trim_end()),
            ));
        }
        self.i = li + 1;
        Ok(v)
    }

    /// Plain text from column `col` of line `i`, and on the lines below it indented more than
    /// `parent`, each line joined to the one before with a space.
    fn plain(&mut self, parent: isize, i: usize, col: usize) -> Result<Data, String> {
        let one = |l: &str, from: usize, li: usize| -> Result<String, String> {
            let t = l[from..comment_at(l, from).unwrap_or(l.len())].trim();
            let b = t.as_bytes();
            if (0..b.len())
                .any(|k| b[k] == b':' && matches!(b.get(k + 1), None | Some(b' ' | b'\t')))
            {
                return Err(at(
                    li,
                    "\": \" in a value: put the value in quotes, or check the indentation",
                ));
            }
            Ok(t.to_string())
        };
        let mut text = one(&self.lines[i], col, i)?;
        let first_only = comment_at(&self.lines[i], col).is_some();
        let mut j = i + 1;
        let mut end = i + 1;
        let mut breaks = 0;
        while !first_only && j < self.lines.len() {
            let l = &self.lines[j];
            if l.trim().is_empty() {
                breaks += 1;
                j += 1;
                continue;
            }
            if (indent(l) as isize) <= parent || is_marker(l) || blank(l) {
                break;
            }
            let more = one(l, indent(l), j)?;
            if breaks == 0 {
                text.push(' ');
            } else {
                text.extend(std::iter::repeat_n('\n', breaks));
            }
            text.push_str(&more);
            breaks = 0;
            j += 1;
            end = j;
            if comment_at(l, indent(l)).is_some() {
                break;
            }
        }
        self.i = end;
        Ok(if end == i + 1 {
            resolve(&text)
        } else {
            Data::Str(text)
        })
    }

    /// Quoted text starting at the quote `q` at column `col` of line `i`: the text, and the
    /// line and column just after the closing quote. It may go on over several lines.
    fn quoted(&self, i: usize, col: usize, q: u8) -> Result<(String, usize, usize), String> {
        let mut out = String::new();
        let (mut li, mut pos) = (i, col + 1);
        loop {
            let l = &self.lines[li];
            let mut chars = l[pos..].char_indices().peekable();
            while let Some((k, c)) = chars.next() {
                let k = pos + k;
                if c == q as char {
                    if q == b'\'' && chars.peek().map(|p| p.1) == Some('\'') {
                        chars.next();
                        out.push('\'');
                        continue;
                    }
                    return Ok((out, li, k + 1));
                }
                if c == '\\' && q == b'"' {
                    let Some((_, e)) = chars.next() else {
                        // A \ at the end of the line: the line goes on with no space.
                        break;
                    };
                    let hex = |n: usize, chars: &mut std::iter::Peekable<std::str::CharIndices>| {
                        let h: String = (0..n).filter_map(|_| chars.next().map(|x| x.1)).collect();
                        u32::from_str_radix(&h, 16)
                            .ok()
                            .filter(|_| h.len() == n)
                            .and_then(char::from_u32)
                            .ok_or_else(|| at(li, format!("\\{e}{h} isn't a character")))
                    };
                    out.push(match e {
                        '0' => '\0',
                        'a' => '\x07',
                        'b' => '\x08',
                        't' | '\t' => '\t',
                        'n' => '\n',
                        'v' => '\x0b',
                        'f' => '\x0c',
                        'r' => '\r',
                        'e' => '\x1b',
                        ' ' => ' ',
                        '"' => '"',
                        '/' => '/',
                        '\\' => '\\',
                        'N' => '\u{85}',
                        '_' => '\u{a0}',
                        'L' => '\u{2028}',
                        'P' => '\u{2029}',
                        'x' => hex(2, &mut chars)?,
                        'u' => hex(4, &mut chars)?,
                        'U' => hex(8, &mut chars)?,
                        e => return Err(at(li, format!("\\{e} isn't an escape YAML knows"))),
                    });
                    if chars.peek().is_none() && l.ends_with('\\') && !out.ends_with('\\') {
                        break;
                    }
                    continue;
                }
                out.push(c);
            }
            // The line ends inside the quotes: the break becomes a space, or the empty lines
            // after it become line breaks. A \ ending the line joins it to the next as it is.
            let joined = q == b'"' && {
                let t = l.trim_end_matches([' ', '\t']);
                let slashes = t.len() - t.trim_end_matches('\\').len();
                slashes % 2 == 1
            };
            if !joined {
                let t = out.trim_end_matches([' ', '\t']).len();
                out.truncate(t);
            }
            li += 1;
            let mut breaks = 0;
            while li < self.lines.len() && self.lines[li].trim().is_empty() {
                breaks += 1;
                li += 1;
            }
            if li >= self.lines.len() {
                return Err(at(i, "the quotes are never closed"));
            }
            if !joined {
                if breaks == 0 {
                    out.push(' ');
                } else {
                    out.extend(std::iter::repeat_n('\n', breaks));
                }
            }
            let l = &self.lines[li];
            pos = l.len() - l.trim_start_matches([' ', '\t']).len();
        }
    }

    /// Skips spaces, line ends and comments inside brackets.
    fn flow_space(&self, cur: &mut (usize, usize), open: usize) -> Result<(), String> {
        loop {
            let Some(l) = self.lines.get(cur.0) else {
                return Err(at(open, "the bracket is never closed"));
            };
            let b = l.as_bytes();
            while cur.1 < b.len() && matches!(b[cur.1], b' ' | b'\t') {
                cur.1 += 1;
            }
            if cur.1 < b.len() && b[cur.1] != b'#' {
                return Ok(());
            }
            *cur = (cur.0 + 1, 0);
        }
    }

    /// A value in brackets, or in a list or map in brackets, at `cur` (which moves past it).
    fn flow(&self, cur: &mut (usize, usize)) -> Result<Data, String> {
        let open = cur.0;
        self.flow_space(cur, open)?;
        let byte = |cur: &(usize, usize)| self.lines[cur.0].as_bytes()[cur.1];
        match byte(cur) {
            b'[' => {
                cur.1 += 1;
                let mut items = Vec::new();
                loop {
                    self.flow_space(cur, open)?;
                    if byte(cur) == b']' {
                        cur.1 += 1;
                        return Ok(Data::List(items));
                    }
                    items.push(self.flow(cur)?);
                    self.flow_space(cur, open)?;
                    match byte(cur) {
                        b',' => cur.1 += 1,
                        b']' => {}
                        _ => return Err(at(cur.0, "a , or ] was expected here")),
                    }
                }
            }
            b'{' => {
                cur.1 += 1;
                let mut out: Vec<(String, Data)> = Vec::new();
                loop {
                    self.flow_space(cur, open)?;
                    if byte(cur) == b'}' {
                        cur.1 += 1;
                        return Ok(Data::Map(out));
                    }
                    let key = match self.flow(cur)? {
                        Data::Str(s) => s,
                        Data::Null => String::new(),
                        Data::List(_) | Data::Map(_) => {
                            return Err(at(cur.0, "a key can't be a list or map"))
                        }
                        other => other.text(),
                    };
                    if out.iter().any(|(k, _)| *k == key) {
                        return Err(at(cur.0, format!("the key \"{key}\" is there twice")));
                    }
                    self.flow_space(cur, open)?;
                    let v = if byte(cur) == b':' {
                        cur.1 += 1;
                        self.flow_space(cur, open)?;
                        if matches!(byte(cur), b',' | b'}') {
                            Data::Null
                        } else {
                            self.flow(cur)?
                        }
                    } else {
                        Data::Null
                    };
                    out.push((key, v));
                    self.flow_space(cur, open)?;
                    match byte(cur) {
                        b',' => cur.1 += 1,
                        b'}' => {}
                        _ => return Err(at(cur.0, "a , or } was expected here")),
                    }
                }
            }
            q @ (b'"' | b'\'') => {
                let (s, li, after) = self.quoted(cur.0, cur.1, q)?;
                *cur = (li, after);
                Ok(Data::Str(s))
            }
            _ => {
                self.unsupported(cur.0, cur.1)?;
                let l = &self.lines[cur.0];
                let b = l.as_bytes();
                let start = cur.1;
                let mut k = start;
                while k < b.len() {
                    let c = b[k];
                    if matches!(c, b',' | b'[' | b']' | b'{' | b'}')
                        || (c == b':'
                            && matches!(
                                b.get(k + 1),
                                None | Some(b' ' | b'\t' | b',' | b'[' | b']' | b'{' | b'}')
                            ))
                        || (c == b'#' && k > start && matches!(b[k - 1], b' ' | b'\t'))
                    {
                        break;
                    }
                    k += 1;
                }
                let t = l[start..k].trim();
                if t.is_empty() {
                    return Err(at(cur.0, "a value was expected here"));
                }
                cur.1 = k;
                Ok(resolve(t))
            }
        }
    }

    /// `|` or `>` text, its header at column `col` of line `i`, on the lines below indented more
    /// than `parent`.
    fn block_text(&mut self, parent: isize, i: usize, col: usize) -> Result<Data, String> {
        let l = &self.lines[i];
        let folded = l.as_bytes()[col] == b'>';
        let mut chomp = None;
        let mut step = None;
        let mut k = col + 1;
        for c in l[col + 1..].chars() {
            match c {
                '+' | '-' if chomp.is_none() => chomp = Some(c),
                '1'..='9' if step.is_none() => step = Some(c as usize - '0' as usize),
                _ => break,
            }
            k += 1;
        }
        let rest = l[k..].trim_start_matches([' ', '\t']);
        if !rest.is_empty() && !(rest.starts_with('#') && rest.len() < l.len() - k) {
            return Err(at(
                i,
                "after | or > only -, + and a digit (or a comment) can come",
            ));
        }
        let base = parent.max(-1);
        let mut j = i + 1;
        let width = match step {
            Some(s) => (base.max(0) as usize) + s,
            None => {
                let first = (j..self.lines.len()).find(|&x| !self.lines[x].trim().is_empty());
                match first {
                    Some(x) if indent(&self.lines[x]) as isize > base => indent(&self.lines[x]),
                    _ => usize::MAX,
                }
            }
        };
        let mut body: Vec<String> = Vec::new();
        while j < self.lines.len() {
            let l = &self.lines[j];
            if l.trim().is_empty() {
                body.push(l.get(width..).unwrap_or("").to_string());
            } else if width != usize::MAX && indent(l) >= width && !(width == 0 && is_marker(l)) {
                body.push(l[width..].to_string());
            } else {
                break;
            }
            j += 1;
        }
        self.i = j;
        let trailing = body.iter().rev().take_while(|l| l.is_empty()).count();
        body.truncate(body.len() - trailing);
        let mut text = if folded {
            let mut out = String::new();
            let (mut first, mut empties, mut last_more) = (true, 0, false);
            for l in &body {
                if l.is_empty() {
                    empties += 1;
                    continue;
                }
                let more = l.starts_with([' ', '\t']);
                if first {
                    out.extend(std::iter::repeat_n('\n', empties));
                } else if !more && !last_more {
                    if empties == 0 {
                        out.push(' ');
                    } else {
                        out.extend(std::iter::repeat_n('\n', empties));
                    }
                } else {
                    out.extend(std::iter::repeat_n('\n', empties + 1));
                }
                out.push_str(l);
                (first, empties, last_more) = (false, 0, more);
            }
            out
        } else {
            body.join("\n")
        };
        let has_text = !body.is_empty();
        match chomp {
            Some('-') => {}
            Some(_) => text.extend(std::iter::repeat_n('\n', trailing + usize::from(has_text))),
            None if has_text => text.push('\n'),
            None => {}
        }
        Ok(Data::Str(text))
    }
}

/// A plain value as YAML 1.2 reads it: null, true or false, a number, or text.
fn resolve(t: &str) -> Data {
    match t {
        "" | "~" | "null" | "Null" | "NULL" => return Data::Null,
        "true" | "True" | "TRUE" => return Data::Bool(true),
        "false" | "False" | "FALSE" => return Data::Bool(false),
        ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" => {
            return Data::Float(f64::INFINITY)
        }
        "-.inf" | "-.Inf" | "-.INF" => return Data::Float(f64::NEG_INFINITY),
        ".nan" | ".NaN" | ".NAN" => return Data::Float(f64::NAN),
        _ => {}
    }
    let digits = |s: &str, radix: u32| !s.is_empty() && s.chars().all(|c| c.is_digit(radix));
    if let Some(h) = t.strip_prefix("0x") {
        if digits(h, 16) {
            if let Ok(n) = i64::from_str_radix(h, 16) {
                return Data::Int(n);
            }
        }
    }
    if let Some(o) = t.strip_prefix("0o") {
        if digits(o, 8) {
            if let Ok(n) = i64::from_str_radix(o, 8) {
                return Data::Int(n);
            }
        }
    }
    let unsigned = t.strip_prefix(['-', '+']).unwrap_or(t);
    if digits(unsigned, 10) {
        return match t.parse::<i64>() {
            Ok(n) => Data::Int(n),
            Err(_) => Data::Float(t.parse().unwrap_or(f64::NAN)),
        };
    }
    // [0-9]*(.[0-9]*)?([eE][-+]?[0-9]+)? with a digit somewhere before the exponent.
    let (mant, exp) = match unsigned.find(['e', 'E']) {
        Some(k) => (&unsigned[..k], Some(&unsigned[k + 1..])),
        None => (unsigned, None),
    };
    let (whole, frac) = mant.split_once('.').unwrap_or((mant, ""));
    let mant_ok = (whole.is_empty() || digits(whole, 10))
        && (frac.is_empty() || digits(frac, 10))
        && !(whole.is_empty() && frac.is_empty())
        && (mant.contains('.') || exp.is_some());
    let exp_ok = exp.is_none_or(|e| digits(e.strip_prefix(['-', '+']).unwrap_or(e), 10));
    if mant_ok && exp_ok {
        if let Ok(f) = t.parse::<f64>() {
            return Data::Float(f);
        }
    }
    Data::Str(t.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use Data::*;

    fn one(text: &str) -> Data {
        let mut d = parse(text).unwrap();
        assert_eq!(d.len(), 1, "{d:?}");
        d.remove(0)
    }
    fn s(t: &str) -> Data {
        Str(t.to_string())
    }
    fn map(kv: &[(&str, Data)]) -> Data {
        Map(kv.iter().map(|(k, v)| (k.to_string(), v.clone())).collect())
    }

    #[test]
    fn maps_and_lists_by_indentation() {
        let text = "\
# a compose file
version: '3.8'
services:
  web:
    image: nginx:1.25   # the web server
    ports:
      - \"80:80\"
      - 443:443
    environment:
      - DEBUG=1
  db:
    image: postgres
    restart: always
empty:
list at key indent:
- a
- b: 1
  c: 2
-
  - x
  - y
- - z
";
        let d = one(text);
        assert_eq!(
            d,
            map(&[
                ("version", s("3.8")),
                (
                    "services",
                    map(&[
                        (
                            "web",
                            map(&[
                                ("image", s("nginx:1.25")),
                                ("ports", List(vec![s("80:80"), s("443:443")])),
                                ("environment", List(vec![s("DEBUG=1")])),
                            ])
                        ),
                        (
                            "db",
                            map(&[("image", s("postgres")), ("restart", s("always"))])
                        ),
                    ])
                ),
                ("empty", Null),
                (
                    "list at key indent",
                    List(vec![
                        s("a"),
                        map(&[("b", Int(1)), ("c", Int(2))]),
                        List(vec![s("x"), s("y")]),
                        List(vec![s("z")]),
                    ])
                ),
            ])
        );
    }

    #[test]
    fn plain_values_as_yaml_1_2_reads_them() {
        let d = one(
            "a: 12\nb: -3\nc: 0x1F\nd: 0o17\ne: 1.5\nf: 2e3\ng: .5\nh: true\ni: False\nj: ~\n\
             k: null\nl: no\nm: on\nn: 1.2.3\no: .inf\np: 007\nq: 99999999999999999999\nr: +1\n",
        );
        let Map(kv) = d else { panic!() };
        let get = |k: &str| kv.iter().find(|(x, _)| x == k).unwrap().1.clone();
        assert_eq!(get("a"), Int(12));
        assert_eq!(get("b"), Int(-3));
        assert_eq!(get("c"), Int(31));
        assert_eq!(get("d"), Int(15));
        assert_eq!(get("e"), Float(1.5));
        assert_eq!(get("f"), Float(2000.0));
        assert_eq!(get("g"), Float(0.5));
        assert_eq!(get("h"), Bool(true));
        assert_eq!(get("i"), Bool(false));
        assert_eq!(get("j"), Null);
        assert_eq!(get("k"), Null);
        assert_eq!(get("l"), s("no"));
        assert_eq!(get("m"), s("on"));
        assert_eq!(get("n"), s("1.2.3"));
        assert_eq!(get("o"), Float(f64::INFINITY));
        assert_eq!(get("p"), Int(7));
        assert_eq!(get("q"), Float(1e20));
        assert_eq!(get("r"), Int(1));
    }

    #[test]
    fn quoted_text() {
        let d = one(
            "a: 'it''s # not a comment'\nb: \"tab\\there \\u00e9 \\\"q\\\"\"\n\
             c: \"one\n  two\n\n  three\"\nd: \"joined\\\n  up\"\n'quoted key': 1\ne: \"12\"\n",
        );
        assert_eq!(
            d,
            map(&[
                ("a", s("it's # not a comment")),
                ("b", s("tab\there é \"q\"")),
                ("c", s("one two\nthree")),
                ("d", s("joinedup")),
                ("quoted key", Int(1)),
                ("e", s("12")),
            ])
        );
    }

    #[test]
    fn text_over_several_lines() {
        let d = one(
            "lit: |\n  line 1\n    indented\n\n  line 3\nfold: >\n  a\n  b\n\n  c\n    kept\n  d\n\
             strip: |-\n  x\n\n\nkeep: |+\n  y\n\nplain: a\n  b\n\n  c\nlast: 1\n",
        );
        assert_eq!(
            d,
            map(&[
                ("lit", s("line 1\n  indented\n\nline 3\n")),
                ("fold", s("a b\nc\n  kept\nd\n")),
                ("strip", s("x")),
                ("keep", s("y\n\n")),
                ("plain", s("a b\nc")),
                ("last", Int(1)),
            ])
        );
        // In a list, and at the end of the file with no line break.
        assert_eq!(
            one("- |\n  a\n  b\n- >-\n  c\n  d"),
            List(vec![s("a\nb\n"), s("c d")])
        );
    }

    #[test]
    fn brackets_over_lines() {
        let d = one(
            "a: [1, two, \"three\", [x, y], {k: v}]\nb: {x: 1, 'y': [2], z}\nc: [\n  p,  # first\n  q,\n]\n\
             d: []\ne: {}\nf: {\"json\": true, \"n\": null}\ng: [http://x.org/a, a:b]\n",
        );
        assert_eq!(
            d,
            map(&[
                (
                    "a",
                    List(vec![
                        Int(1),
                        s("two"),
                        s("three"),
                        List(vec![s("x"), s("y")]),
                        map(&[("k", s("v"))]),
                    ])
                ),
                (
                    "b",
                    map(&[("x", Int(1)), ("y", List(vec![Int(2)])), ("z", Null)])
                ),
                ("c", List(vec![s("p"), s("q")])),
                ("d", List(vec![])),
                ("e", Map(vec![])),
                ("f", map(&[("json", Bool(true)), ("n", Null)])),
                ("g", List(vec![s("http://x.org/a"), s("a:b")])),
            ])
        );
        // A JSON file is YAML too.
        assert_eq!(
            one("{\"a\": [1, 2.5, \"x\"], \"b\": {\"c\": false}}"),
            map(&[
                ("a", List(vec![Int(1), Float(2.5), s("x")])),
                ("b", map(&[("c", Bool(false))])),
            ])
        );
    }

    #[test]
    fn several_documents() {
        assert_eq!(
            parse("%YAML 1.2\n---\na: 1\n---\n- b\n...\n--- c\n").unwrap(),
            vec![map(&[("a", Int(1))]), List(vec![s("b")]), s("c")]
        );
        assert_eq!(parse("").unwrap(), vec![Null]);
        assert_eq!(parse("# only a comment\n").unwrap(), vec![Null]);
        assert_eq!(
            parse("\u{feff}just text\r\n").unwrap(),
            vec![s("just text")]
        );
    }

    #[test]
    fn a_workflow_file() {
        let text = "\
name: CI
on:
  push:
    branches: [ main ]
  pull_request:

jobs:
  build:
    # every OS
    runs-on: ${{ matrix.os }}
    strategy:
      matrix:
        os: [ubuntu-latest, macos-latest]
    steps:
      - uses: actions/checkout@v4
      - name: Build
        run: |
          cargo build
          cargo test --lib

      - name: \"Quoted: name\"
        with:
            key: value # comment
        env: { A: 1 }
";
        let d = one(text);
        let step = |n: usize| {
            let Map(top) = &d else { panic!() };
            let Map(jobs) = &top[2].1 else { panic!() };
            let Map(build) = &jobs[0].1 else { panic!() };
            let List(steps) = &build[2].1 else {
                panic!("{:?}", build[2])
            };
            steps[n].clone()
        };
        let Map(top) = &d else { panic!() };
        assert_eq!(top[0], ("name".into(), s("CI")));
        assert_eq!(
            top[1].1,
            map(&[
                ("push", map(&[("branches", List(vec![s("main")]))])),
                ("pull_request", Null),
            ])
        );
        assert_eq!(step(0), map(&[("uses", s("actions/checkout@v4"))]));
        assert_eq!(
            step(1),
            map(&[
                ("name", s("Build")),
                ("run", s("cargo build\ncargo test --lib\n"))
            ])
        );
        assert_eq!(
            step(2),
            map(&[
                ("name", s("Quoted: name")),
                ("with", map(&[("key", s("value"))])),
                ("env", map(&[("A", Int(1))])),
            ])
        );
        let Map(jobs) = &top[2].1 else { panic!() };
        let Map(build) = &jobs[0].1 else { panic!() };
        assert_eq!(build[0], ("runs-on".into(), s("${{ matrix.os }}")));
    }

    #[test]
    fn refuses_with_the_line() {
        let err = |t: &str| parse(t).unwrap_err();
        assert!(
            err("a: 1\nb: &x 2\n").starts_with("line 2: anchors"),
            "{}",
            err("a: 1\nb: &x 2\n")
        );
        assert!(err("a: *x\n").starts_with("line 1: aliases"));
        assert!(err("a: !!str 1\n").starts_with("line 1: tags"));
        assert!(err("? a\n: b\n").starts_with("line 1: keys written"));
        assert!(err("a: 1\na: 2\n").contains("\"a\" is there twice"));
        assert!(
            err("a:\n    b: 1\n  c: 2\n").starts_with("line 3: "),
            "{}",
            err("a:\n    b: 1\n  c: 2\n")
        );
        assert!(
            err("a: b\n   c: d\n").starts_with("line 2: \": \""),
            "{}",
            err("a: b\n   c: d\n")
        );
        assert!(err("a:\n\tb: 1\n").starts_with("line 2: a tab"));
        assert!(err("a: \"open\nb: 1\n").starts_with("line 1: the quotes"));
        assert!(
            err("a: [1, 2\nb: 3\n").contains("a , or ] was expected"),
            "{}",
            err("a: [1, 2\nb: 3\n")
        );
        assert!(err("a: [1, 2").starts_with("line 1: the bracket"));
        assert!(err("a: 'x' y\n").contains("after the value"));
        assert!(err("- a\nb: 1\n").starts_with("line 2: "));
    }
}
