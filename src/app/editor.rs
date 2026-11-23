//! The text editor: opening, saving and closing a text file.

use super::*;
use std::ops::Range;

impl Ctx {
    pub(super) fn open_text(self, p: Pane, path: String) {
        self.open_text_on(p, p.src.get_untracked(), path);
    }

    /// Opens text file `path` on `src` in the editor, from pane `p` (a script, from Settings).
    pub(super) fn open_text_on(self, p: Pane, src: Src, path: String) {
        self.busy.set(true);
        spawn_local(async move {
            match call::<TextFile>("read_text", json!({ "src": src.key(), "path": path })).await {
                Ok(f) => {
                    let (text, crlf, mixed) = to_lf(f.text);
                    self.editor.set(Some(Editor { src, pane: p.id, path, version: f.version, saved_text: text.as_str().into(), crlf, mixed }));
                    self.text.set(text);
                    self.status.set((String::new(), ""));
                    self.confirm_discard.set(false);
                    self.finding.set(false);
                }
                Err(e) => p.error(e),
            }
            self.busy.set(false);
        });
    }

    pub(super) fn save(self) {
        self.save_in(self.full_slot());
    }

    pub(super) fn save_in(self, slot: Slot) {
        let Some(ed) = slot.editor.get_untracked() else { return };
        if self.busy.get_untracked() {
            return;
        }
        let text = slot.text.get_untracked();
        self.busy.set(true);
        slot.status.set(("Saving…".into(), ""));
        spawn_local(async move {
            // With the file's own line endings.
            let crlf = ed.crlf.then(|| text.replace('\n', "\r\n"));
            let args = json!({ "src": ed.src.key(), "path": ed.path, "text": crlf.as_ref().unwrap_or(&text), "base_version": ed.version, "create": false });
            drop(crlf);
            match call::<WriteResult>("write_text", args).await {
                Ok(r) => {
                    let note = if r.backup.is_some() { format!(" The old version is backed up on {}.", crate::os::the_computer()) } else { String::new() };
                    slot.status.set((format!("Saved to {}.{note}", self.place(ed.src)), "ok"));
                    self.log(format!("Saved {} on {}", base(&ed.path), self.place(ed.src)));
                    let same = |e: &Editor| e.src == ed.src && e.path == ed.path;
                    let saved = Editor { version: r.version, saved_text: text.into(), ..ed.clone() };
                    if slot.editor.with_untracked(|e| e.as_ref().is_some_and(same)) {
                        slot.editor.set(Some(saved));
                        slot.ask.set(false);
                    } else if let Some(p) = slot.pane {
                        // Another tab was shown meanwhile: the saved file is in the stash.
                        p.docs.update(|ds| {
                            for d in ds {
                                if let Doc::Text { ed, .. } = d {
                                    if same(ed) {
                                        *ed = saved.clone();
                                    }
                                }
                            }
                        });
                    }
                }
                Err(e) => slot.status.set((e, "error")),
            }
            self.busy.set(false);
        });
    }

    /// Opens a text file in the pane's Editor view: in its tab if it's open, else in place of the
    /// shown file, or in a new tab when the shown file has unsaved changes.
    pub(super) fn open_doc(self, p: Pane, path: String) {
        let src = p.src.get_untracked();
        reveal(p, &path);
        if let Some(i) = p.docs.with_untracked(|ds| ds.iter().position(|d| d.src() == src && d.path() == path)) {
            self.show_doc(p, i);
            return;
        }
        self.busy.set(true);
        spawn_local(async move {
            match call::<TextFile>("read_text", json!({ "src": src.key(), "path": path })).await {
                Ok(f) => {
                    let (text, crlf, mixed) = to_lf(f.text);
                    let ed = Editor { src, pane: p.id, path, version: f.version, saved_text: text.as_str().into(), crlf, mixed };
                    // Its text is in the editor while it's shown.
                    let doc = Doc::Text { ed: ed.clone(), text: None };
                    let at = p.doc_at.get_untracked();
                    // A shown file with changes, or one that isn't text (kept open), stays.
                    let keep = p.slot().dirty() || p.docs.with_untracked(|ds| matches!(ds.get(at), Some(Doc::File { .. })));
                    stash(p);
                    p.docs.update(|ds| {
                        if ds.is_empty() {
                            ds.push(doc);
                        } else if keep {
                            ds.insert(at + 1, doc);
                        } else {
                            let last = ds.len() - 1;
                            ds[at.min(last)] = doc;
                        }
                    });
                    let at = if keep { at + 1 } else { at.min(p.docs.with_untracked(Vec::len) - 1) };
                    show(p, at, ed, text);
                    p.peek_on.set(false);
                }
                Err(e) => p.error(e),
            }
            self.busy.set(false);
        });
    }

    /// Shows a file that isn't text in the Editor view's preview tab, in place of the one there;
    /// or in its own tab, when it's kept open.
    pub(super) fn peek_doc(self, p: Pane, e: Entry) {
        let src = p.src.get_untracked();
        if let Some(i) = p.docs.with_untracked(|ds| ds.iter().position(|d| matches!(d, Doc::File { .. }) && d.src() == src && d.path() == e.path)) {
            self.show_doc(p, i);
            return;
        }
        reveal(p, &e.path);
        p.doc_peek.set(Some(e));
        p.peek_on.set(true);
    }

    /// Keeps the preview tab's file open in a tab of its own, after the shown one.
    pub(super) fn keep_peek(self, p: Pane) {
        let Some(entry) = p.doc_peek.get_untracked() else { return };
        let src = p.src.get_untracked();
        let n = p.docs.with_untracked(Vec::len);
        let at = if n == 0 { 0 } else { (p.doc_at.get_untracked() + 1).min(n) };
        p.docs.update(|ds| ds.insert(at, Doc::File { src, entry }));
        if p.doc_at.get_untracked() >= at && n > 0 {
            p.doc_at.update(|a| *a += 1);
        }
        p.doc_peek.set(None);
        self.show_doc(p, at);
    }

    /// Closes the Editor view's preview tab.
    pub(super) fn close_peek(self, p: Pane) {
        p.doc_peek.set(None);
        p.peek_on.set(false);
    }

    /// After `old` on `src` was renamed or moved to `new`: open files in it (or it) and the
    /// preview tab follow it.
    pub(super) fn doc_moved(self, src: Src, old: &str, new: &str) {
        let moved = |path: &mut String| {
            if let Some(rest) = path.strip_prefix(old).filter(|r| r.is_empty() || r.starts_with('/')) {
                *path = format!("{new}{rest}");
            }
        };
        for p in self.panes {
            p.docs.update(|ds| {
                for d in ds.iter_mut().filter(|d| d.src() == src) {
                    match d {
                        Doc::Text { ed, .. } => moved(&mut ed.path),
                        Doc::File { entry, .. } => {
                            moved(&mut entry.path);
                            entry.name = base(&entry.path);
                        }
                    }
                }
            });
            p.doc.update(|d| {
                if let Some(ed) = d.as_mut().filter(|ed| ed.src == src) {
                    moved(&mut ed.path);
                }
            });
            if p.src.get_untracked() == src {
                p.doc_peek.update(|e| {
                    if let Some(e) = e.as_mut() {
                        moved(&mut e.path);
                        e.name = base(&e.path);
                    }
                });
            }
        }
    }

    /// Shows the Editor view's tab `i`.
    pub(super) fn show_doc(self, p: Pane, i: usize) {
        p.peek_on.set(false);
        let n = p.docs.with_untracked(Vec::len);
        let here =
            i == p.doc_at.get_untracked() && (p.doc.with_untracked(Option::is_some) || p.docs.with_untracked(|ds| matches!(ds.get(i), Some(Doc::File { .. }))));
        if here || i >= n {
            return;
        }
        stash(p);
        put(p, i);
    }

    /// Closes the Editor view's tab `i`; with unsaved changes, unless `force`, it's shown and asks.
    pub(super) fn close_doc(self, p: Pane, i: usize, force: bool) {
        let at = p.doc_at.get_untracked();
        let dirty = if i == at { p.slot().dirty() } else { p.docs.with_untracked(|ds| ds.get(i).is_some_and(Doc::dirty)) };
        if dirty && !force {
            self.show_doc(p, i);
            p.doc_ask.set(true);
            return;
        }
        if i >= p.docs.with_untracked(Vec::len) {
            return;
        }
        p.docs.update(|ds| {
            ds.remove(i);
        });
        let n = p.docs.with_untracked(Vec::len);
        if i == at {
            let next = i.min(n.saturating_sub(1));
            match n {
                1.. => put(p, next),
                0 => {
                    p.doc.set(None);
                    p.doc_text.set(String::new());
                    p.doc_at.set(0);
                    p.doc_ask.set(false);
                    // The preview tab, if there's one, is what's left to show.
                    if p.doc_peek.with_untracked(Option::is_some) {
                        p.peek_on.set(true);
                    }
                }
            }
        } else if i < at {
            p.doc_at.set(at - 1);
        }
    }

    pub(super) fn close_editor(self, force: bool) {
        if !force && self.dirty() {
            self.confirm_discard.set(true);
            return;
        }
        self.leave_editor();
        self.confirm_discard.set(false);
        self.reload_all();
    }

    /// Closes the editor and lets go of its text (a file of up to a few MB, held twice).
    pub(super) fn leave_editor(self) {
        self.editor.set(None);
        self.text.set(String::new());
    }
}

/// Keeps the shown tab's file in its entry of `docs`, and its text if it has changes: unchanged,
/// it's made again from the saved text when shown.
fn stash(p: Pane) {
    let Some(ed) = p.doc.get_untracked() else { return };
    let at = p.doc_at.get_untracked();
    let text = p.doc_text.try_update_untracked(std::mem::take).unwrap_or_default();
    let text = (text != *ed.saved_text).then_some(text);
    p.docs.update(|ds| {
        if let Some(d) = ds.get_mut(at) {
            *d = Doc::Text { ed, text };
        }
    });
}

/// Shows tab `at`: a text file in the editor (its text moved there, not copied), or a kept file
/// as the preview shows it.
fn put(p: Pane, at: usize) {
    let taken = p.docs.try_update(|ds| {
        ds.get_mut(at).map(|d| match d {
            Doc::Text { ed, text } => (ed.path.clone(), Some((ed.clone(), text.take().unwrap_or_else(|| ed.saved_text.to_string())))),
            Doc::File { entry, .. } => (entry.path.clone(), None),
        })
    });
    let Some(Some((path, text))) = taken else { return };
    reveal(p, &path);
    match text {
        Some((ed, text)) => show(p, at, ed, text),
        None => {
            p.doc.set(None);
            p.doc_text.set(String::new());
            p.doc_at.set(at);
            p.doc_ask.set(false);
            p.doc_status.set((String::new(), ""));
        }
    }
}

/// Puts tab `at`'s file in the editor. Its text goes first: the editor reads it when it's made.
fn show(p: Pane, at: usize, ed: Editor, text: String) {
    p.doc_text.set(text);
    p.doc.set(Some(ed));
    p.doc_at.set(at);
    p.doc_ask.set(false);
    p.doc_status.set((String::new(), ""));
}

/// Opens the folders in the Editor view's tree down to `path`, so its row is shown.
fn reveal(p: Pane, path: &str) {
    let root = p.cwd.get_untracked();
    let mut dir = parent(path);
    let mut open = vec![];
    while dir.len() > root.len() && dir.starts_with(&root) {
        open.push(dir.clone());
        dir = parent(&dir);
    }
    if !open.is_empty() {
        p.tree_open.update(|o| o.extend(open));
    }
}

/// Text up to this size is drawn over the editing box, with colours, line numbers and find marks;
/// bigger text is edited plain.
pub(crate) const LAYER_MAX: usize = 512 * 1024;
/// Colours are asked for this long after typing stops.
const COLOUR_DELAY: std::time::Duration = std::time::Duration::from_millis(250);
/// Matches marked, at most.
const FIND_MAX: usize = 10_000;
/// The editor's line height in px (`.code-edit` in pane.css): for scrolling to a line in plain mode.
const LINE_PX: i32 = 20;
/// Lines painted above and below those shown, at least.
const MARGIN: usize = 60;

/// The class of each kind of run (`highlight::Kind` in the backend), as the preview's.
const TOKEN_CLASSES: [&str; 17] = [
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
fn slice_runs(runs: &[(u8, u32)], from: usize, to: usize) -> Vec<(u8, u32)> {
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
    let mut pre = old.bytes().zip(new.bytes()).take_while(|(a, b)| a == b).count();
    while !new.is_char_boundary(pre) || !old.is_char_boundary(pre) {
        pre -= 1;
    }
    let room = old.len().min(new.len()) - pre;
    let mut suf = old.bytes().rev().zip(new.bytes().rev()).take(room).take_while(|(a, b)| a == b).count();
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
fn runs_fit(runs: &[(u8, u32)], t: &str) -> bool {
    let mut at = 0usize;
    for &(_, n) in runs {
        at += n as usize;
        if !t.is_char_boundary(at) {
            return false;
        }
    }
    at == t.len()
}

fn escape_into(out: &mut String, s: &str) {
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
pub(crate) fn code_html(t: &str, runs: Option<&[(u8, u32)]>, hits: &[(usize, usize)], shown: Option<usize>, rich: Range<usize>) -> String {
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
    let b = if rich.end <= first { Some(a) } else { line_start(t, rich.end) };
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
                out.push_str(if shown == Some(h) { "<mark class=\"hit on\">" } else { "<mark class=\"hit\">" });
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
fn line_start(t: &str, k: usize) -> Option<usize> {
    match k {
        0 => None,
        1 => Some(0),
        k => t.match_indices('\n').nth(k - 2).map(|(i, _)| i + 1),
    }
}

/// Where each line starts in the layer's text (UTF-16, as a DOM `Range` counts). The text has LF
/// line breaks only (`to_lf`).
fn utf16_starts(t: &str) -> Vec<u32> {
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
fn band(first: usize, end: usize) -> Range<usize> {
    let m = (end - first).max(MARGIN);
    first.saturating_sub(m).max(1)..end + m
}

/// `t` with LF line breaks, as the editing box keeps it (a lone CR is one too); whether most of
/// its lines ended in CRLF, and whether it had both.
fn to_lf(t: String) -> (String, bool, bool) {
    if !t.contains('\r') {
        return (t, false, false);
    }
    let crlf = t.matches("\r\n").count();
    let lf = t.matches('\n').count() - crlf;
    (t.replace("\r\n", "\n").replace('\r', "\n"), crlf > lf, crlf > 0 && lf > 0)
}

/// (lines, characters) in a selection; (0, 0) when it's empty. A last line break doesn't start a
/// line of its own.
fn selected(s: &str) -> (usize, usize) {
    if s.is_empty() {
        return (0, 0);
    }
    let breaks = s.matches('\n').count();
    (breaks + 1 - usize::from(s.ends_with('\n')), s.chars().count())
}

/// The byte in `t` at UTF-16 offset `u` (a textarea's selection is in UTF-16).
fn byte_at(t: &str, u: u32) -> usize {
    let mut n = 0u32;
    for (i, c) in t.char_indices() {
        if n >= u {
            return i;
        }
        n += c.len_utf16() as u32;
    }
    t.len()
}

fn utf16_at(t: &str, b: usize) -> u32 {
    t.get(..b).map_or(0, |s| s.encode_utf16().count() as u32)
}

/// What one indent is in `t`: a tab if lines start with tabs, else four spaces.
fn indent_unit(t: &str) -> &'static str {
    if t.lines().take(2000).any(|l| l.starts_with('\t')) {
        "\t"
    } else {
        "    "
    }
}

/// Types `s` into the box where the selection is, as typing would (so ⌘Z undoes it).
fn type_text(el: &web_sys::HtmlTextAreaElement, s: &str) {
    let typed =
        document().dyn_into::<web_sys::HtmlDocument>().ok().and_then(|d| d.exec_command_with_show_ui_and_value("insertText", false, s).ok()).unwrap_or(false);
    if !typed {
        let v = el.value();
        let (a, b) = (el.selection_start().ok().flatten().unwrap_or(0), el.selection_end().ok().flatten().unwrap_or(0));
        let (a, b) = (byte_at(&v, a), byte_at(&v, b));
        let at = utf16_at(&v, a) + s.encode_utf16().count() as u32;
        el.set_value(&format!("{}{s}{}", &v[..a], &v[b..]));
        let _ = el.set_selection_range(at, at);
        if let Ok(ev) = web_sys::Event::new("input") {
            let _ = el.dispatch_event(&ev);
        }
    }
}

/// The text editor: code colours, line numbers, wrap, the current line, and find (⌘F). Full width
/// for `Ctx::full_slot`, or in a pane's Editor view, where the find box is always shown. The text
/// is typed in a `<textarea>` whose letters are clear; a layer under it shows them, in colour,
/// scrolled with it.
#[component]
pub(super) fn TextEditor(file: Slot) -> impl IntoView {
    let slot = file;
    let ctx = expect_context::<Ctx>();
    let ta = NodeRef::<leptos::html::Textarea>::new();
    let layer = NodeRef::<leptos::html::Pre>::new();
    let find_box = NodeRef::<leptos::html::Input>::new();
    let name = slot.editor.with_untracked(|e| e.as_ref().map(|e| base(&e.path).to_string()).unwrap_or_default());
    let language = code_language(&name).unwrap_or("Text");
    let name = StoredValue::new(name);
    // Small enough to draw under the box.
    let drawn_on = slot.text.with_untracked(|t| t.len() <= LAYER_MAX);
    // Lines painted with colours and marks (by number); the rest are plain.
    let painted = StoredValue::new(0..0usize);
    // Where each line starts in the layer's text, when it was painted.
    let starts = StoredValue::new(Vec::<u32>::new());
    let runs = StoredValue::new(None::<Vec<(u8, u32)>>);
    // Colours stopped partway (far in, or slow): the rest is plain.
    let stopped = RwSignal::new(false);
    // Counts edits, so colours asked for before one are dropped.
    let edits = StoredValue::new(0u64);
    let colour_timer = StoredValue::new_local(None::<TimeoutHandle>);
    let caret = RwSignal::new((1usize, 1usize));
    // Lines and characters selected: (0, 0) when nothing is.
    let picked = RwSignal::new((0usize, 0usize));
    let finding = slot.find;
    let FindState { query, with, opts, keep, replacing } = ctx.find_state;
    let replace_box = NodeRef::<leptos::html::Input>::new();
    // What's wrong with the regular expression.
    let find_err = RwSignal::new(None::<String>);
    let hits = RwSignal::new(Vec::<(usize, usize)>::new());
    let shown = RwSignal::new(None::<usize>);
    let (wrap, numbers) = (ctx.wrap_text, ctx.line_numbers);
    let lines = Memo::new(move |_| slot.text.with(|t| t.matches('\n').count() + 1));
    let (crlf, mixed) = slot.editor.with_untracked(|e| e.as_ref().map_or((false, false), |e| (e.crlf, e.mixed)));
    // Files are read as UTF-8 only; a byte order mark is kept as the text's first character.
    let encoding = Memo::new(move |_| slot.text.with(|t| if t.starts_with('\u{feff}') { "UTF-8 with BOM" } else { "UTF-8" }));

    // The current line gets a highlight in the layer.
    let mark_line = move |line: usize| {
        let Some(l) = layer.get_untracked() else { return };
        if let Ok(Some(old)) = l.query_selector(".ln.cur") {
            let _ = old.class_list().remove_1("cur");
        }
        if let Ok(Some(el)) = l.query_selector(&format!(":scope > .ln[data-n=\"{line}\"]")) {
            let _ = el.class_list().add_1("cur");
        }
    };
    // The top of line `k` (from 1) in the layer, in px: its `.ln`, or in a plain part, where its
    // first character is.
    let line_top = move |k: usize| -> Option<f64> {
        let l = layer.get_untracked()?;
        let p = painted.get_value();
        if p.contains(&k) {
            let e = l.query_selector(&format!(":scope > .ln[data-n=\"{k}\"]")).ok().flatten()?;
            return Some(e.dyn_into::<web_sys::HtmlElement>().ok()?.offset_top() as f64);
        }
        let far = if k < p.start { l.first_element_child() } else { l.last_element_child() }?;
        if !far.class_list().contains("far") {
            return None;
        }
        let node = far.first_child()?;
        let (from, at) = starts.with_value(|s| Some((if k < p.start { 0 } else { *s.get(p.end - 1)? }, *s.get(k - 1)?)))?;
        let r = document().create_range().ok()?;
        r.set_start(&node, at - from).ok()?;
        r.set_end(&node, at - from + 1).ok()?;
        Some(r.get_bounding_client_rect().top() - l.get_bounding_client_rect().top() + l.scroll_top() as f64)
    };
    // The lines in view (by number, `first..end`).
    let in_view = move || -> Range<usize> {
        let Some(t) = ta.get_untracked() else { return 1..2 };
        let (top, h) = (t.scroll_top(), t.client_height().max(LINE_PX));
        let n = lines.get_untracked();
        // How many lines start at or above `y`; a guess by the line height before it's painted.
        let above = |y: i32| {
            let (mut lo, mut hi) = (1, n + 1);
            while lo < hi {
                let mid = (lo + hi) / 2;
                match line_top(mid) {
                    Some(at) if at <= y as f64 => lo = mid + 1,
                    Some(_) => hi = mid,
                    None => return (y / LINE_PX) as usize + 1,
                }
            }
            lo - 1
        };
        above(top).max(1)..above(top + h) + 1
    };
    let draw_at = move |rich: Range<usize>| {
        let Some(l) = layer.get_untracked() else { return };
        let html =
            slot.text.with_untracked(|t| runs.with_value(|r| hits.with_untracked(|h| code_html(t, r.as_deref(), h, shown.get_untracked(), rich.clone()))));
        l.set_inner_html(&html);
        starts.set_value(slot.text.with_untracked(|t| utf16_starts(t)));
        painted.set_value(rich);
        mark_line(caret.get_untracked().0);
    };
    let sync_scroll = move || {
        if let (Some(t), Some(l)) = (ta.get_untracked(), layer.get_untracked()) {
            l.set_scroll_top(t.scroll_top());
            l.set_scroll_left(t.scroll_left());
        }
        // Painted again when the lines in view get near the edge of those painted.
        if drawn_on {
            let v = in_view();
            let (from, to) = (v.start.saturating_sub(MARGIN / 2).max(1), v.end + MARGIN / 2);
            if painted.with_value(|p| from < p.start || to > p.end) {
                draw_at(band(v.start, v.end));
            }
        }
    };
    let draw = move || {
        if !drawn_on {
            return;
        }
        let v = in_view();
        draw_at(band(v.start, v.end));
        sync_scroll();
    };
    let update_caret = move || {
        let Some(el) = ta.get_untracked() else { return };
        let a = el.selection_start().ok().flatten().unwrap_or(0);
        let b = el.selection_end().ok().flatten().unwrap_or(a);
        // The caret is the end the selection was made towards.
        let back = el.selection_direction().ok().flatten().is_some_and(|d| d == "backward");
        let (line, col, sel) = slot.text.with_untracked(|t| {
            let (a, b) = (byte_at(t, a), byte_at(t, b));
            let at = if back { a } else { b };
            let before = &t[..at];
            let line_start = before.rfind('\n').map_or(0, |i| i + 1);
            (before.matches('\n').count() + 1, before[line_start..].chars().count() + 1, selected(&t[a..b]))
        });
        if caret.get_untracked().0 != line {
            mark_line(line);
        }
        caret.set((line, col));
        picked.set(sel);
    };
    let colour_now = move || {
        if !drawn_on || !ctx.colour_code.get_untracked() {
            return;
        }
        let at = edits.get_value();
        let text = slot.text.get_untracked();
        let name = name.get_value();
        spawn_local(async move {
            let Ok(c) = call::<crate::api::TextColours>("colour_text", json!({ "name": name, "text": text })).await else { return };
            if edits.try_get_value() != Some(at) {
                return;
            }
            runs.set_value(c.runs);
            stopped.set(c.stopped);
            draw();
        });
    };
    let colour_soon = move || {
        if let Some(h) = colour_timer.try_update_value(Option::take).flatten() {
            h.clear();
        }
        let h = set_timeout_with_handle(colour_now, COLOUR_DELAY);
        colour_timer.set_value(h.ok());
    };
    on_cleanup(move || {
        if let Some(h) = colour_timer.try_update_value(Option::take).flatten() {
            h.clear();
        }
    });
    let refind = move || {
        if finding.get_untracked() {
            let q = query.get_untracked();
            match slot.text.with_untracked(|t| find::find(t, &q, opts.get_untracked(), FIND_MAX)) {
                Ok(h) => {
                    find_err.set(None);
                    hits.set(h);
                }
                Err(e) => {
                    find_err.set(Some(e));
                    hits.set(vec![]);
                }
            }
            shown.set(shown.get_untracked().filter(|&i| i < hits.with_untracked(Vec::len)));
        }
    };
    let edited = move || {
        let Some(el) = ta.get_untracked() else { return };
        let v = el.value();
        edits.update_value(|n| *n += 1);
        if drawn_on {
            // The text in the slot is still the one the colours are for.
            let shifted = slot.text.with_untracked(|old| runs.with_value(|r| r.as_ref().map(|r| shift_runs(r, old, &v))));
            runs.set_value(shifted);
        }
        slot.text.set(v);
        // A message about what was done ("Saved.", "Replaced 3 matches.") is old after an edit.
        if slot.status.with_untracked(|(_, k)| *k == "ok") {
            slot.status.set((String::new(), ""));
        }
        refind();
        update_caret();
        draw();
        colour_soon();
    };
    // Shows match `i`: selected in the box and scrolled to.
    let show_hit = move |i: usize| {
        let Some((a, b)) = hits.with_untracked(|h| h.get(i).copied()) else { return };
        shown.set(Some(i));
        let Some(el) = ta.get_untracked() else { return };
        let (ua, ub, line) = slot.text.with_untracked(|t| (utf16_at(t, a), utf16_at(t, b), t[..a].matches('\n').count()));
        let _ = el.set_selection_range(ua, ub);
        draw();
        let top = line_top(line + 1).map_or(line as i32 * LINE_PX, |t| t as i32);
        let h = el.client_height();
        if top < el.scroll_top() || top > el.scroll_top() + h - 2 * LINE_PX {
            el.set_scroll_top(top - h / 3);
        }
        sync_scroll();
        update_caret();
    };
    let step = move |back: bool| {
        let n = hits.with_untracked(Vec::len);
        if n == 0 {
            return;
        }
        let i = match shown.get_untracked() {
            Some(i) if back => (i + n - 1) % n,
            Some(i) => (i + 1) % n,
            None => {
                // The first match after the caret.
                let at = ta.get_untracked().and_then(|el| el.selection_start().ok().flatten()).unwrap_or(0);
                let at = slot.text.with_untracked(|t| byte_at(t, at));
                hits.with_untracked(|h| h.iter().position(|&(a, _)| a >= at).unwrap_or(0))
            }
        };
        show_hit(i);
    };
    let focus_find = move || {
        request_animation_frame(move || {
            if let Some(i) = find_box.get_untracked() {
                let _ = i.focus();
                i.select();
            }
        });
    };
    // The find bar shown or hidden, here or by the Editor view's Find button.
    Effect::new(move |prev: Option<bool>| {
        let on = finding.get();
        if prev.is_none() && on {
            // Shown already (the Editor view keeps it from file to file).
            refind();
        } else if prev.is_some_and(|p| p != on) {
            shown.set(None);
            if on {
                refind();
                focus_find();
            } else {
                hits.set(vec![]);
            }
            draw();
        }
        on
    });
    let open_find = move |replace: bool| {
        // The selection, when it's on one line, is what's looked for.
        if let Some(el) = ta.get_untracked() {
            let (a, b) = (el.selection_start().ok().flatten().unwrap_or(0), el.selection_end().ok().flatten().unwrap_or(0));
            let picked = slot.text.with_untracked(|t| t.get(byte_at(t, a)..byte_at(t, b)).map(str::to_string)).unwrap_or_default();
            if !picked.is_empty() && !picked.contains('\n') {
                query.set(picked);
            }
        }
        if replace {
            replacing.set(true);
        }
        if finding.get_untracked() {
            shown.set(None);
            refind();
            draw();
            focus_find();
        } else {
            finding.set(true);
        }
    };
    let close_find = move || {
        finding.set(false);
        if let Some(el) = ta.get_untracked() {
            let _ = el.focus();
        }
    };
    // Replaces the shown match (or, with none shown, picks the next) and shows the next one.
    let replace_one = move || {
        let Some(el) = ta.get_untracked() else { return };
        let Some((a, b)) = shown.get_untracked().and_then(|i| hits.with_untracked(|h| h.get(i).copied())) else {
            step(false);
            return;
        };
        let w =
            slot.text.with_untracked(|t| find::replacement_at(t, &query.get_untracked(), opts.get_untracked(), &with.get_untracked(), keep.get_untracked(), a));
        let Some(w) = w else { return };
        let (ua, ub) = slot.text.with_untracked(|t| (utf16_at(t, a), utf16_at(t, b)));
        // Typed into the text (focused, so it goes there), so ⌘Z undoes it; its input event finds
        // the matches again.
        let _ = el.focus();
        let _ = el.set_selection_range(ua, ub);
        type_text(&el, &w);
        shown.set(None);
        let after = a + w.len();
        match hits.with_untracked(|h| h.iter().position(|&(s, _)| s >= after).or((!h.is_empty()).then_some(0))) {
            Some(i) => show_hit(i),
            None => draw(),
        }
        let _ = replace_box.get_untracked().map(|i| i.focus());
    };
    let replace_every = move || {
        let Some(el) = ta.get_untracked() else { return };
        let (q, w) = (query.get_untracked(), with.get_untracked());
        let Ok((new, n)) = slot.text.with_untracked(|t| find::replace_all(t, &q, opts.get_untracked(), &w, keep.get_untracked())) else { return };
        if n == 0 {
            return;
        }
        let _ = el.focus();
        el.select();
        type_text(&el, &new);
        let _ = el.set_selection_range(0, 0);
        slot.status.set((format!("Replaced {n} {}.", if n == 1 { "match" } else { "matches" }), "ok"));
    };
    let keys = move |ev: web_sys::KeyboardEvent| {
        let meta = ev.meta_key() || ev.ctrl_key();
        // ⌥⌘C, W and R turn the find options on or off, ⌥⌘P Preserve case (⌥ changes the key, so by
        // the code).
        if meta && ev.alt_key() && finding.get_untracked() {
            let flip: Option<fn(&mut find::FindOpts)> = match ev.code().as_str() {
                "KeyC" => Some(|o| o.case = !o.case),
                "KeyW" => Some(|o| o.word = !o.word),
                "KeyR" => Some(|o| o.regex = !o.regex),
                _ => None,
            };
            if let Some(f) = flip {
                ev.prevent_default();
                opts.update(f);
                shown.set(None);
                refind();
                draw();
                return;
            }
            if ev.code() == "KeyP" {
                ev.prevent_default();
                keep.update(|k| *k = !*k);
                return;
            }
        }
        match ev.key().as_str() {
            "f" | "ƒ" if meta => {
                ev.prevent_default();
                open_find(ev.alt_key());
            }
            "g" if meta && finding.get_untracked() => {
                ev.prevent_default();
                step(ev.shift_key());
            }
            _ => {}
        }
    };
    let box_keys = move |ev: web_sys::KeyboardEvent| {
        if ev.meta_key() || ev.ctrl_key() || ev.alt_key() || ev.is_composing() {
            return;
        }
        let Some(el) = ta.get_untracked() else { return };
        match ev.key().as_str() {
            "Tab" if !ev.shift_key() => {
                ev.prevent_default();
                type_text(&el, slot.text.with_untracked(|t| indent_unit(t)));
            }
            // A new line starts as indented as the one it's split from.
            "Enter" if !ev.shift_key() => {
                let at = el.selection_start().ok().flatten().unwrap_or(0);
                let indent = slot.text.with_untracked(|t| {
                    let b = byte_at(t, at);
                    let start = t[..b].rfind('\n').map_or(0, |i| i + 1);
                    t[start..b].chars().take_while(|c| *c == ' ' || *c == '\t').collect::<String>()
                });
                if !indent.is_empty() {
                    ev.prevent_default();
                    type_text(&el, &format!("\n{indent}"));
                }
            }
            "Escape" if finding.get_untracked() => close_find(),
            _ => {}
        }
    };
    let find_keys = move |ev: web_sys::KeyboardEvent| match ev.key().as_str() {
        "Enter" => {
            ev.prevent_default();
            step(ev.shift_key());
        }
        "Escape" => {
            ev.prevent_default();
            ev.stop_propagation();
            close_find();
        }
        _ => {}
    };
    ta.on_load(move |el| {
        // macOS would otherwise "correct" code as it's typed.
        let _ = el.set_attribute("autocorrect", "off");
        slot.text.with_untracked(|t| el.set_value(t));
        request_animation_frame(move || {
            draw();
            if let Some(el) = ta.get_untracked() {
                // Caret first: focusing scrolls to it, and set_value left it at the end.
                let _ = el.set_selection_range(0, 0);
                let _ = el.focus();
                el.set_scroll_top(0);
                el.set_scroll_left(0);
            }
            update_caret();
            colour_now();
        });
    });
    // Colours turned on or off in Settings.
    Effect::new(move |prev: Option<bool>| {
        let on = ctx.colour_code.get();
        if prev.is_some_and(|p| p != on) {
            if on {
                colour_now();
            } else {
                runs.set_value(None);
                stopped.set(false);
                draw();
            }
        }
        on
    });

    // Wrapping, turned on or off in Settings, moves the lines the layer's marks are on.
    Effect::new(move |prev: Option<bool>| {
        let w = wrap.get();
        if prev.is_some() {
            request_animation_frame(draw);
        }
        w
    });
    let path = move || slot.editor.get().map(|e| e.path).unwrap_or_default();
    let dirty = Memo::new(move |_| slot.dirty());
    let replace_keys = move |ev: web_sys::KeyboardEvent| match ev.key().as_str() {
        "Enter" => {
            ev.prevent_default();
            if ev.meta_key() || ev.ctrl_key() {
                replace_every()
            } else {
                replace_one()
            }
        }
        "Escape" => {
            ev.prevent_default();
            ev.stop_propagation();
            close_find();
        }
        _ => {}
    };
    let toggle_replace = move |_| {
        let on = !replacing.get_untracked();
        replacing.set(on);
        request_animation_frame(move || {
            let el = if on { replace_box.get_untracked() } else { find_box.get_untracked() };
            if let Some(i) = el {
                let _ = i.focus();
            }
        });
    };
    let none = move || hits.with(Vec::is_empty);
    // An option of how to find, turned on or off: the matches are found again.
    let opt = move |f: fn(&mut find::FindOpts)| {
        opts.update(f);
        shown.set(None);
        refind();
        draw();
    };
    let opt_btn = move |label: &'static str,
                        keys: &'static str,
                        on: fn(find::FindOpts) -> bool,
                        flip: fn(&mut find::FindOpts),
                        glyph: &'static str,
                        class: &'static str| {
        view! {
            <button class="filter-opt" class:on=move || on(opts.get()) aria-pressed=move || on(opts.get()).to_string()
                title=crate::os::keys(keys).to_string() aria-label=label
                on:mousedown=|ev: web_sys::MouseEvent| ev.prevent_default() on:click=move |_| opt(flip)>
                <span class=class>{glyph}</span>
            </button>
        }
    };
    // Shown on request (⌘F, or the Find button): the find box and its counts, and Replace with
    // under it (⌥⌘F, or the chevron).
    let find_bar = move || {
        finding.get().then(|| view! {
        <div class="find-bar" class:replacing=move || replacing.get() on:keydown=keys>
            <button class="btn icon-only chevron" class:open=move || replacing.get() aria-expanded=move || replacing.get().to_string()
                title=move || if replacing.get() { "Hide Replace with".to_string() } else { crate::os::keys("Replace (⌥⌘F)").to_string() } aria-label="Replace"
                on:mousedown=|ev: web_sys::MouseEvent| ev.prevent_default() on:click=toggle_replace>{ico(Icon::Chevron)}</button>
            <div class="filter-box find" class:bad=move || !query.with(String::is_empty) && none() title=move || find_err.get()>
                {ico(Icon::Search)}
                <input class="filter" node_ref=find_box type="text" placeholder="Find" aria-label="Find" spellcheck="false"
                    prop:value=move || query.get()
                    on:input=move |ev| { query.set(event_target_value(&ev)); shown.set(None); refind(); draw(); }
                    on:keydown=find_keys/>
                {opt_btn("Match case", "Match case (⌥⌘C)", |o| o.case, |o| o.case = !o.case, "Aa", "")}
                {opt_btn("Match whole word", "Match whole word (⌥⌘W)", |o| o.word, |o| o.word = !o.word, "ab", "word")}
                {opt_btn("Use regular expression", "Use regular expression (⌥⌘R)", |o| o.regex, |o| o.regex = !o.regex, ".*", "")}
            </div>
            <div class="find-tools">
                <span class="count">{move || {
                    let n = hits.with(Vec::len);
                    match (n, shown.get()) {
                        _ if query.with(String::is_empty) => String::new(),
                        (0, _) => "No matches".into(),
                        (n, Some(i)) => format!("{} of {n}{}", i + 1, if n >= FIND_MAX { "+" } else { "" }),
                        (n, None) => format!("{n}{} {}", if n >= FIND_MAX { "+" } else { "" }, if n == 1 { "match" } else { "matches" }),
                    }
                }}</span>
                <button class="btn icon-only" title="Previous match (⇧Enter)" aria-label="Previous match" disabled=none
                    on:click=move |_| step(true)>{ico(Icon::Prev)}</button>
                <button class="btn icon-only" title="Next match (Enter)" aria-label="Next match" disabled=none
                    on:click=move |_| step(false)>{ico(Icon::Next)}</button>
            </div>
            <button class="btn icon-only close-find" title="Close (Esc)" aria-label="Close find" on:click=move |_| close_find()>{ico(Icon::Close)}</button>
            {move || replacing.get().then(|| view! {
                <div class="filter-box with">
                    {ico(Icon::Edit)}
                    <input class="filter" node_ref=replace_box type="text" placeholder="Replace with" aria-label="Replace with" spellcheck="false"
                        prop:value=move || with.get()
                        on:input=move |ev| with.set(event_target_value(&ev))
                        on:keydown=replace_keys/>
                    <button class="filter-opt" class:on=move || keep.get() aria-pressed=move || keep.get().to_string()
                        title=crate::os::keys("Preserve case: Foo becomes Bar, FOO becomes BAR (⌥⌘P)") aria-label="Preserve case"
                        on:mousedown=|ev: web_sys::MouseEvent| ev.prevent_default() on:click=move |_| keep.update(|k| *k = !*k)><span>"AB"</span></button>
                </div>
                <div class="replace-tools">
                    <button class="btn icon-only" title="Replace (Enter): this match, then shows the next" aria-label="Replace" disabled=none
                        on:click=move |_| replace_one()>{ico(Icon::Replace)}</button>
                    <button class="btn icon-only" title=crate::os::keys("Replace all (⌘Enter)") aria-label="Replace all" disabled=none
                        on:click=move |_| replace_every()>{ico(Icon::ReplaceAll)}</button>
                </div>
            })}
        </div>
    })
    };
    let head = match slot.pane {
        // The Editor view: its tabs have the Find button.
        Some(_) => ().into_any(),
        None => view! {
            <div class="bar">
                <button class="btn back" title="Back to files" aria-label="Back to files" on:click=move |_| ctx.close_editor(false)><svg class="back-icon" viewBox="0 0 16 16" aria-hidden="true"><path d="M13 8H3M7.5 3.5 3 8l4.5 4.5"/></svg></button>
                <span class="title">{path}{move || dirty.get().then_some(" •")}</span>
                <span class="kind">{move || slot.editor.get().map(|e| match e.src {
                    Src::Mac => crate::os::computer().to_string(),
                    src => ctx.src_info(src).map_or_else(|| "Device".into(), |i| i.label),
                })}</span>
                <span class="spacer"></span>
                <button class="btn icon-only" class:active=move || finding.get() title=crate::os::keys("Find (⌘F)") aria-label="Find"
                    aria-pressed=move || finding.get().to_string()
                    on:click=move |_| if finding.get_untracked() { close_find() } else { open_find(false) }>{ico(Icon::Search)}</button>
            </div>
        }
        .into_any(),
    };
    view! {
        {head}
        {find_bar}
        {move || slot.ask.get().then(|| view! {
            <div class="notice">
                <span class="text">"You have unsaved changes."</span>
                <button class="btn primary" on:click=move |_| ctx.save_in(slot)>{ico(Icon::Save)}"Save"</button>
                <button class="btn danger" on:click=move |_| match slot.pane {
                    Some(p) => ctx.close_doc(p, p.doc_at.get_untracked(), true),
                    None => ctx.close_editor(true),
                }>{ico(Icon::Trash)}"Discard changes"</button>
                <button class="btn" on:click=move |_| slot.ask.set(false)>"Keep editing"</button>
            </div>
        })}
        <div class="code-edit" class:layered=drawn_on class:nowrap=move || !wrap.get() class:numbered=move || drawn_on && numbers.get()
            style:--ln-w=move || format!("{}ch", lines.get().to_string().len().max(2))
            on:keydown=keys>
            {drawn_on.then(|| view! { <pre class="code-layer" node_ref=layer aria-hidden="true"></pre> })}
            <textarea class="code-input" node_ref=ta spellcheck="false" autocomplete="off" autocapitalize="off"
                aria-label=move || format!("Text of {}", name.get_value())
                wrap=move || if wrap.get() { "soft" } else { "off" }
                on:input=move |_| edited()
                on:scroll=move |_| sync_scroll()
                on:keydown=box_keys
                on:keyup=move |_| update_caret()
                on:click=move |_| update_caret()
                on:select=move |_| update_caret()></textarea>
        </div>
        <div class="statusbar">
            <span class=move || format!("status {}", slot.status.get().1)>{move || {
                let (msg, _) = slot.status.get();
                if !msg.is_empty() { msg } else if dirty.get() { "Unsaved changes".into() } else { "Saved".into() }
            }}</span>
            <span class="spacer"></span>
            {move || stopped.get().then(|| view! { <span title="Colouring stopped partway, far into the file or where it got slow">"Colours stop partway"</span> })}
            {(!drawn_on).then(|| view! { <span title="Colours, line numbers and find marks are off for text over 512 KB">"Plain: big file"</span> })}
            {move || match picked.get() {
                (0, _) => None,
                (l, c) => Some(view! { <span>{format!("{l} {}, {c} {} selected", if l == 1 { "line" } else { "lines" }, if c == 1 { "character" } else { "characters" })}</span> }),
            }}
            <span title="Line:column">{move || { let (l, c) = caret.get(); format!("{l}:{c}") }}</span>
            <span title=if mixed { "Line endings. The file had both LF and CRLF: it's saved with one." } else { "Line endings" }>{if crlf { "CRLF" } else { "LF" }}</span>
            <span>{encoding}</span>
            <span>{language}</span>
            {slot.pane.map(|p| view! { <ViewSeg pane=p/> })}
        </div>
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
        assert_eq!(shift_runs(&runs, "ab cd", "abX cd"), [(4, 2), (0, 1), (0, 1), (2, 2)]);
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
        assert!(code_html("a\n", None, &[], None, ALL).ends_with("<span class=\"ln\" data-n=\"2\">\n</span>"));
        assert_eq!(code_html("", None, &[], None, ALL), "<span class=\"ln\" data-n=\"1\">\n</span>");
    }

    #[test]
    fn html_marks_matches_and_colours() {
        let t = "let x";
        // Runs that don't fit the text are left out.
        assert_eq!(code_html(t, Some(&[(4, 9)]), &[], None, ALL), "<span class=\"ln\" data-n=\"1\">let x\n</span>");
        let h = code_html(t, Some(&[(4, 3), (0, 2)]), &find::find(t, "ET", find::FindOpts::default(), FIND_MAX).unwrap(), Some(0), ALL);
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
        assert_eq!(code_html("a\nb", None, &[], None, 2..9), "<span class=\"far\">a\n</span><span class=\"ln\" data-n=\"2\">b\n</span>");
        assert_eq!(code_html("a\n", None, &[], None, 1..2), "<span class=\"ln\" data-n=\"1\">a\n</span><span class=\"far\">\n</span>");
        // Lines past the end: all of it, rich.
        assert_eq!(code_html("a", None, &[], None, 5..9), "<span class=\"ln\" data-n=\"1\">a\n</span>");
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
        assert_eq!(to_lf("a\r\nb\r\nc\n".into()), ("a\nb\nc\n".into(), true, true));
        assert_eq!(to_lf("a\r\nb\nc\n".into()), ("a\nb\nc\n".into(), false, true));
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
