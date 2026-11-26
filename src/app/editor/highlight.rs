//! The layer under the editing box: code colours, line numbers and find marks as HTML, and the
//! text offsets it needs.

use super::*;

/// Text up to this size is drawn over the editing box, with colours, line numbers and find marks;
/// bigger text is edited plain.
pub(crate) const LAYER_MAX: usize = 512 * 1024;
/// Colours are asked for this long after typing stops.
pub(super) const COLOUR_DELAY: std::time::Duration = std::time::Duration::from_millis(250);
/// Matches marked, at most.
pub(super) const FIND_MAX: usize = 10_000;
/// The editor's line height in px (`.code-edit` in pane.css): for scrolling to a line in plain mode.
pub(super) const LINE_PX: i32 = 20;
/// Lines painted above and below those shown, at least.
pub(super) const MARGIN: usize = 60;

/// The class of each kind of run (`highlight::Kind` in the backend), as the preview's.
pub(super) const TOKEN_CLASSES: [&str; 17] = [
    "",
    "tok-comment",
    "tok-string",
    "tok-number",
    "tok-keyword",
    "tok-type",
    "tok-function",
    "tok-tag",
    "tok-attr",
    "tok-heading",
    "tok-link",
    "tok-url",
    "tok-bold",
    "tok-italic",
    "tok-quote",
    "tok-list",
    "tok-code",
];

/// The runs over `from..to` of text the `runs` cover.
pub(super) fn slice_runs(runs: &[(u8, u32)], from: usize, to: usize) -> Vec<(u8, u32)> {
    let mut out = vec![];
    let mut at = 0usize;
    for &(k, n) in runs {
        let end = at + n as usize;
        let (a, b) = (at.max(from), end.min(to));
        if a < b {
            out.push((k, (b - a) as u32));
        }
        at = end;
        if at >= to {
            break;
        }
    }
    out
}

/// `runs` for `old` moved onto `new`: kept where the start and the end are unchanged; what's
/// between is plain until it's coloured again.
pub(crate) fn shift_runs(runs: &[(u8, u32)], old: &str, new: &str) -> Vec<(u8, u32)> {
    let mut pre = old
        .bytes()
        .zip(new.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    while !new.is_char_boundary(pre) || !old.is_char_boundary(pre) {
        pre -= 1;
    }
    let room = old.len().min(new.len()) - pre;
    let mut suf = old
        .bytes()
        .rev()
        .zip(new.bytes().rev())
        .take(room)
        .take_while(|(a, b)| a == b)
        .count();
    while !new.is_char_boundary(new.len() - suf) || !old.is_char_boundary(old.len() - suf) {
        suf -= 1;
    }
    let mut out = slice_runs(runs, 0, pre);
    let mid = new.len() - pre - suf;
    if mid > 0 {
        out.push((0, mid as u32));
    }
    out.extend(slice_runs(runs, old.len() - suf, old.len()));
    out
}

/// Whether `runs` cover `t` exactly, ending on character boundaries.
pub(super) fn runs_fit(runs: &[(u8, u32)], t: &str) -> bool {
    let mut at = 0usize;
    for &(_, n) in runs {
        at += n as usize;
        if !t.is_char_boundary(at) {
            return false;
        }
    }
    at == t.len()
}

pub(super) fn escape_into(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
}

/// The text as HTML for the layer under the editing box: a `<span class="ln">` per line in `rich`
/// (by number; its number in `data-n`, shown by CSS), with a `<span>` per coloured run and a
/// `<mark>` per match. The lines before and after are each one plain `<span class="far">`: they're
/// out of view, and a long file is then a few hundred nodes, not a few per line. Every line ends
/// with a line break, so an empty last line has its height. The text is escaped.
pub(crate) fn code_html(
    t: &str,
    runs: Option<&[(u8, u32)]>,
    hits: &[(usize, usize)],
    shown: Option<usize>,
    rich: Range<usize>,
) -> String {
    let runs = runs.filter(|r| runs_fit(r, t));
    // (end, class) per run.
    let mut ends = vec![];
    if let Some(runs) = runs {
        let mut at = 0;
        for &(k, n) in runs {
            at += n as usize;
            ends.push((at, TOKEN_CLASSES.get(k as usize).copied().unwrap_or("")));
        }
    }
    // The rich lines' bytes, `a..b`; b is None when they go on to the last line.
    let (first, a) = line_start(t, rich.start.max(1)).map_or((1, 0), |a| (rich.start.max(1), a));
    let b = if rich.end <= first {
        Some(a)
    } else {
        line_start(t, rich.end)
    };
    let mut out = String::with_capacity((a + 64).max(4096));
    if a > 0 {
        out.push_str("<span class=\"far\">");
        escape_into(&mut out, &t[..a]);
        out.push_str("</span>");
    }
    if b != Some(a) {
        let stop = b.unwrap_or(t.len());
        let (mut ri, mut hi, mut line, mut pos) = (0, 0, first, a);
        let open = |out: &mut String, line: usize| {
            out.push_str("<span class=\"ln\" data-n=\"");
            out.push_str(&line.to_string());
            out.push_str("\">");
        };
        open(&mut out, line);
        while pos < stop {
            while ends.get(ri).is_some_and(|e| e.0 <= pos) {
                ri += 1;
            }
            while hits.get(hi).is_some_and(|h| h.1 <= pos) {
                hi += 1;
            }
            let (class, run_end) = ends.get(ri).map_or(("", t.len()), |e| (e.1, e.0));
            let (hit, hit_edge) = match hits.get(hi) {
                Some(&(s, e)) if pos >= s => (Some(hi), e),
                Some(&(s, _)) => (None, s),
                None => (None, t.len()),
            };
            let nl = t[pos..].find('\n').map_or(t.len(), |i| pos + i + 1);
            let mut end = run_end.min(hit_edge).min(nl);
            while !t.is_char_boundary(end) {
                end += 1;
            }
            let piece = &t[pos..end];
            if let Some(h) = hit {
                out.push_str(if shown == Some(h) {
                    "<mark class=\"hit on\">"
                } else {
                    "<mark class=\"hit\">"
                });
            }
            if !class.is_empty() {
                out.push_str("<span class=\"");
                out.push_str(class);
                out.push_str("\">");
            }
            escape_into(&mut out, piece);
            if !class.is_empty() {
                out.push_str("</span>");
            }
            if hit.is_some() {
                out.push_str("</mark>");
            }
            pos = end;
            if piece.ends_with('\n') {
                line += 1;
                out.push_str("</span>");
                // The rich lines end at a line's start; the last line is opened even when empty.
                if pos < stop || b.is_none() {
                    open(&mut out, line);
                }
            }
        }
        if b.is_none() {
            // The last line has no line break of its own (or is the empty one after the last).
            out.push_str("\n</span>");
        }
    }
    if let Some(b) = b {
        out.push_str("<span class=\"far\">");
        escape_into(&mut out, &t[b..]);
        out.push_str("\n</span>");
    }
    out
}

/// Where line `k` (from 1) starts in `t`, in bytes; None past the last line.
pub(super) fn line_start(t: &str, k: usize) -> Option<usize> {
    match k {
        0 => None,
        1 => Some(0),
        k => t.match_indices('\n').nth(k - 2).map(|(i, _)| i + 1),
    }
}

/// Where each line starts in the layer's text (UTF-16, as a DOM `Range` counts). The text has LF
/// line breaks only (`to_lf`).
pub(super) fn utf16_starts(t: &str) -> Vec<u32> {
    let mut out = vec![0];
    let mut at = 0u32;
    for c in t.chars() {
        at += c.len_utf16() as u32;
        if c == '\n' {
            out.push(at);
        }
    }
    out
}

/// Lines painted (by number) for the lines shown, `first..end`: as many again above and below,
/// at least `MARGIN`.
pub(super) fn band(first: usize, end: usize) -> Range<usize> {
    let m = (end - first).max(MARGIN);
    first.saturating_sub(m).max(1)..end + m
}

/// `t` with LF line breaks, as the editing box keeps it (a lone CR is one too); whether most of
/// its lines ended in CRLF, and whether it had both.
pub(super) fn to_lf(t: String) -> (String, bool, bool) {
    if !t.contains('\r') {
        return (t, false, false);
    }
    let crlf = t.matches("\r\n").count();
    let lf = t.matches('\n').count() - crlf;
    (
        t.replace("\r\n", "\n").replace('\r', "\n"),
        crlf > lf,
        crlf > 0 && lf > 0,
    )
}

/// (lines, characters) in a selection; (0, 0) when it's empty. A last line break doesn't start a
/// line of its own.
pub(super) fn selected(s: &str) -> (usize, usize) {
    if s.is_empty() {
        return (0, 0);
    }
    let breaks = s.matches('\n').count();
    (
        breaks + 1 - usize::from(s.ends_with('\n')),
        s.chars().count(),
    )
}

/// The byte in `t` at UTF-16 offset `u` (a textarea's selection is in UTF-16).
pub(super) fn byte_at(t: &str, u: u32) -> usize {
    let mut n = 0u32;
    for (i, c) in t.char_indices() {
        if n >= u {
            return i;
        }
        n += c.len_utf16() as u32;
    }
    t.len()
}

pub(super) fn utf16_at(t: &str, b: usize) -> u32 {
    t.get(..b).map_or(0, |s| s.encode_utf16().count() as u32)
}

/// What one indent is in `t`: a tab if lines start with tabs, else four spaces.
pub(super) fn indent_unit(t: &str) -> &'static str {
    if t.lines().take(2000).any(|l| l.starts_with('\t')) {
        "\t"
    } else {
        "    "
    }
}

/// Types `s` into the box where the selection is, as typing would (so ⌘Z undoes it).
pub(super) fn type_text(el: &web_sys::HtmlTextAreaElement, s: &str) {
    let typed = document()
        .dyn_into::<web_sys::HtmlDocument>()
        .ok()
        .and_then(|d| {
            d.exec_command_with_show_ui_and_value("insertText", false, s)
                .ok()
        })
        .unwrap_or(false);
    if !typed {
        let v = el.value();
        let (a, b) = (
            el.selection_start().ok().flatten().unwrap_or(0),
            el.selection_end().ok().flatten().unwrap_or(0),
        );
        let (a, b) = (byte_at(&v, a), byte_at(&v, b));
        let at = utf16_at(&v, a) + s.encode_utf16().count() as u32;
        el.set_value(&format!("{}{s}{}", &v[..a], &v[b..]));
        let _ = el.set_selection_range(at, at);
        if let Ok(ev) = web_sys::Event::new("input") {
            let _ = el.dispatch_event(&ev);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: Range<usize> = 0..usize::MAX;

    #[test]
    fn runs_follow_an_edit() {
        // "ab cd" coloured as [ab][ ][cd]; "X" typed after "ab".
        let runs = [(4, 2), (0, 1), (2, 2)];
        assert_eq!(
            shift_runs(&runs, "ab cd", "abX cd"),
            [(4, 2), (0, 1), (0, 1), (2, 2)]
        );
        // A letter deleted from the end.
        assert_eq!(shift_runs(&runs, "ab cd", "ab c"), [(4, 2), (0, 1), (2, 1)]);
        // Everything replaced.
        assert_eq!(shift_runs(&runs, "ab cd", "zz"), [(0, 2)]);
    }

    #[test]
    fn html_has_a_line_per_line_and_escapes() {
        let h = code_html("a<b\n\nc", None, &[], None, ALL);
        assert_eq!(h, "<span class=\"ln\" data-n=\"1\">a&lt;b\n</span><span class=\"ln\" data-n=\"2\">\n</span><span class=\"ln\" data-n=\"3\">c\n</span>");
        // A last empty line still gets a line.
        assert!(code_html("a\n", None, &[], None, ALL)
            .ends_with("<span class=\"ln\" data-n=\"2\">\n</span>"));
        assert_eq!(
            code_html("", None, &[], None, ALL),
            "<span class=\"ln\" data-n=\"1\">\n</span>"
        );
    }

    #[test]
    fn html_marks_matches_and_colours() {
        let t = "let x";
        // Runs that don't fit the text are left out.
        assert_eq!(
            code_html(t, Some(&[(4, 9)]), &[], None, ALL),
            "<span class=\"ln\" data-n=\"1\">let x\n</span>"
        );
        let h = code_html(
            t,
            Some(&[(4, 3), (0, 2)]),
            &find::find(t, "ET", find::FindOpts::default(), FIND_MAX).unwrap(),
            Some(0),
            ALL,
        );
        assert_eq!(h, "<span class=\"ln\" data-n=\"1\"><span class=\"tok-keyword\">l</span><mark class=\"hit on\"><span class=\"tok-keyword\">et</span></mark> x\n</span>");
    }

    #[test]
    fn only_the_rich_lines_get_colours_and_marks() {
        let t = "ab\nab\nab";
        let hits = find::find(t, "b", find::FindOpts::default(), FIND_MAX).unwrap();
        let h = code_html(t, Some(&[(4, 8)]), &hits, None, 2..3);
        assert_eq!(
            h,
            "<span class=\"far\">ab\n</span><span class=\"ln\" data-n=\"2\"><span class=\"tok-keyword\">a</span><mark class=\"hit\"><span class=\"tok-keyword\">b</span></mark><span class=\"tok-keyword\">\n</span></span><span class=\"far\">ab\n</span>"
        );
        // To the last line, and an empty last line.
        assert_eq!(
            code_html("a\nb", None, &[], None, 2..9),
            "<span class=\"far\">a\n</span><span class=\"ln\" data-n=\"2\">b\n</span>"
        );
        assert_eq!(
            code_html("a\n", None, &[], None, 1..2),
            "<span class=\"ln\" data-n=\"1\">a\n</span><span class=\"far\">\n</span>"
        );
        // Lines past the end: all of it, rich.
        assert_eq!(
            code_html("a", None, &[], None, 5..9),
            "<span class=\"ln\" data-n=\"1\">a\n</span>"
        );
        assert_eq!(line_start("a\nb\n", 3), Some(4));
        assert_eq!(line_start("a\nb\n", 4), None);
        assert_eq!(utf16_starts("a\n😀\nb"), [0, 2, 5]);
        assert_eq!(band(100, 110), 40..170);
        assert_eq!(band(1, 300), 1..599);
    }

    #[test]
    fn line_endings_and_selections() {
        assert_eq!(to_lf("a\nb".into()), ("a\nb".into(), false, false));
        assert_eq!(to_lf("a\r\nb\r\n".into()), ("a\nb\n".into(), true, false));
        assert_eq!(
            to_lf("a\r\nb\r\nc\n".into()),
            ("a\nb\nc\n".into(), true, true)
        );
        assert_eq!(
            to_lf("a\r\nb\nc\n".into()),
            ("a\nb\nc\n".into(), false, true)
        );
        assert_eq!(to_lf("a\rb".into()), ("a\nb".into(), false, false));
        assert_eq!(selected(""), (0, 0));
        assert_eq!(selected("ab"), (1, 2));
        assert_eq!(selected("a\nb"), (2, 3));
        assert_eq!(selected("a\n"), (1, 2));
        assert_eq!(selected("é😀"), (1, 2));
    }

    #[test]
    fn utf16_offsets() {
        let t = "é😀x";
        assert_eq!(byte_at(t, 0), 0);
        assert_eq!(byte_at(t, 1), 2);
        assert_eq!(byte_at(t, 3), 6);
        assert_eq!(utf16_at(t, 6), 3);
    }
}
