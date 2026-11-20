//! The text editor: opening, saving and closing a text file.

use super::*;

impl Ctx {
    pub(super) fn open_text(self, p: Pane, path: String) {
        let src = p.src.get_untracked();
        self.busy.set(true);
        spawn_local(async move {
            match call::<TextFile>("read_text", json!({ "src": src.key(), "path": path })).await {
                Ok(f) => {
                    self.text.set(f.text.clone());
                    self.editor.set(Some(Editor { src, pane: p.id, path, version: f.version, saved_text: f.text }));
                    self.status.set((String::new(), ""));
                    self.confirm_discard.set(false);
                }
                Err(e) => p.error(e),
            }
            self.busy.set(false);
        });
    }

    pub(super) fn save(self) {
        let Some(ed) = self.editor.get_untracked() else { return };
        if self.busy.get_untracked() {
            return;
        }
        let text = self.text.get_untracked();
        self.busy.set(true);
        self.status.set(("Saving…".into(), ""));
        spawn_local(async move {
            let args = json!({ "src": ed.src.key(), "path": ed.path, "text": text, "base_version": ed.version, "create": false });
            match call::<WriteResult>("write_text", args).await {
                Ok(r) => {
                    let note = if r.backup.is_some() { format!(" The old version is backed up on {}.", crate::os::the_computer()) } else { String::new() };
                    self.status.set((format!("Saved to {}.{note}", self.place(ed.src)), "ok"));
                    self.log(format!("Saved {} on {}", base(&ed.path), self.place(ed.src)));
                    self.editor.set(Some(Editor { version: r.version, saved_text: text, ..ed }));
                    self.confirm_discard.set(false);
                }
                Err(e) => self.status.set((e, "error")),
            }
            self.busy.set(false);
        });
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

/// Text up to this size is drawn over the editing box, with colours, line numbers and find marks;
/// bigger text is edited plain.
const LAYER_MAX: usize = 512 * 1024;
/// Colours are asked for this long after typing stops.
const COLOUR_DELAY: std::time::Duration = std::time::Duration::from_millis(250);
/// Matches marked, at most.
const FIND_MAX: usize = 10_000;
/// The editor's line height in px (`.code-edit` in pane.css): for scrolling to a line in plain mode.
const LINE_PX: i32 = 20;

/// The class of each kind of run (`highlight::Kind` in the backend), as the preview's.
const TOKEN_CLASSES: [&str; 17] = [
    "", "tok-comment", "tok-string", "tok-number", "tok-keyword", "tok-type", "tok-function", "tok-tag", "tok-attr", "tok-heading",
    "tok-link", "tok-url", "tok-bold", "tok-italic", "tok-quote", "tok-list", "tok-code",
];

/// Where `q` is in `t`, as byte ranges: ignoring case when `q` is ASCII.
fn find_all(t: &str, q: &str) -> Vec<(usize, usize)> {
    if q.is_empty() {
        return vec![];
    }
    let at = |t: &str, q: &str| t.match_indices(q).take(FIND_MAX).map(|(i, _)| (i, i + q.len())).collect();
    // Lowercasing ASCII keeps every byte where it was.
    if q.is_ascii() { at(&t.to_ascii_lowercase(), &q.to_ascii_lowercase()) } else { at(t, q) }
}

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
fn shift_runs(runs: &[(u8, u32)], old: &str, new: &str) -> Vec<(u8, u32)> {
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

/// The text as HTML for the layer under the editing box: a `<span class="ln">` per line (its
/// number in `data-n`, shown by CSS), a `<span>` per coloured run and a `<mark>` per match. Every
/// line ends with a line break, so an empty last line has its height. The text is escaped.
fn code_html(t: &str, runs: Option<&[(u8, u32)]>, hits: &[(usize, usize)], shown: Option<usize>) -> String {
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
    let mut out = String::with_capacity(t.len() * 2 + 64);
    let (mut ri, mut hi, mut line, mut pos) = (0, 0, 1, 0);
    out.push_str("<span class=\"ln\" data-n=\"1\">");
    while pos < t.len() {
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
            out.push_str("</span><span class=\"ln\" data-n=\"");
            out.push_str(&line.to_string());
            out.push_str("\">");
        }
    }
    // The last line has no line break of its own (or is the empty one after the last).
    out.push_str("\n</span>");
    out
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
    if t.lines().take(2000).any(|l| l.starts_with('\t')) { "\t" } else { "    " }
}

/// Types `s` into the box where the selection is, as typing would (so ⌘Z undoes it).
fn type_text(el: &web_sys::HtmlTextAreaElement, s: &str) {
    let typed = document()
        .dyn_into::<web_sys::HtmlDocument>()
        .ok()
        .and_then(|d| d.exec_command_with_show_ui_and_value("insertText", false, s).ok())
        .unwrap_or(false);
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

/// The text editor, full width: code colours, line numbers, wrap, the current line, and find (⌘F).
/// The text is typed in a `<textarea>` whose letters are clear; a layer under it shows them, in
/// colour, scrolled with it.
#[component]
pub(super) fn TextEditor() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let ta = NodeRef::<leptos::html::Textarea>::new();
    let layer = NodeRef::<leptos::html::Pre>::new();
    let find_box = NodeRef::<leptos::html::Input>::new();
    let name = ctx.editor.with_untracked(|e| e.as_ref().map(|e| base(&e.path).to_string()).unwrap_or_default());
    let language = code_language(&name).unwrap_or("Text");
    let name = StoredValue::new(name);
    // Small enough to draw under the box.
    let drawn_on = ctx.text.with_untracked(|t| t.len() <= LAYER_MAX);
    // The text last drawn, and the colours for it.
    let drawn = StoredValue::new(String::new());
    let runs = StoredValue::new(None::<Vec<(u8, u32)>>);
    // Colours stopped partway (far in, or slow): the rest is plain.
    let stopped = RwSignal::new(false);
    // Counts edits, so colours asked for before one are dropped.
    let edits = StoredValue::new(0u64);
    let colour_timer = StoredValue::new_local(None::<TimeoutHandle>);
    let caret = RwSignal::new((1usize, 1usize));
    let finding = RwSignal::new(false);
    let query = RwSignal::new(String::new());
    let hits = RwSignal::new(Vec::<(usize, usize)>::new());
    let shown = RwSignal::new(None::<usize>);
    let (wrap, numbers) = (ctx.wrap_text, ctx.line_numbers);
    let lines = Memo::new(move |_| ctx.text.with(|t| t.matches('\n').count() + 1));

    // The current line gets a highlight in the layer.
    let mark_line = move |line: usize| {
        let Some(l) = layer.get_untracked() else { return };
        if let Ok(Some(old)) = l.query_selector(".ln.cur") {
            let _ = old.class_list().remove_1("cur");
        }
        if let Ok(Some(el)) = l.query_selector(&format!(":scope > .ln:nth-child({line})")) {
            let _ = el.class_list().add_1("cur");
        }
    };
    let sync_scroll = move || {
        if let (Some(t), Some(l)) = (ta.get_untracked(), layer.get_untracked()) {
            l.set_scroll_top(t.scroll_top());
            l.set_scroll_left(t.scroll_left());
        }
    };
    let draw = move || {
        if !drawn_on {
            return;
        }
        let Some(l) = layer.get_untracked() else { return };
        let html = ctx.text.with_untracked(|t| runs.with_value(|r| hits.with_untracked(|h| code_html(t, r.as_deref(), h, shown.get_untracked()))));
        l.set_inner_html(&html);
        mark_line(caret.get_untracked().0);
        sync_scroll();
    };
    let update_caret = move || {
        let Some(el) = ta.get_untracked() else { return };
        let u = el.selection_start().ok().flatten().unwrap_or(0);
        let (line, col) = ctx.text.with_untracked(|t| {
            let b = byte_at(t, u);
            let before = &t[..b];
            let line_start = before.rfind('\n').map_or(0, |i| i + 1);
            (before.matches('\n').count() + 1, before[line_start..].chars().count() + 1)
        });
        if caret.get_untracked().0 != line {
            mark_line(line);
        }
        caret.set((line, col));
    };
    let colour_now = move || {
        if !drawn_on || !ctx.colour_code.get_untracked() {
            return;
        }
        let at = edits.get_value();
        let text = ctx.text.get_untracked();
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
            hits.set(ctx.text.with_untracked(|t| find_all(t, &q)));
            shown.set(shown.get_untracked().filter(|&i| i < hits.with_untracked(Vec::len)));
        }
    };
    let edited = move || {
        let Some(el) = ta.get_untracked() else { return };
        let v = el.value();
        edits.update_value(|n| *n += 1);
        if drawn_on {
            let shifted = drawn.with_value(|old| runs.with_value(|r| r.as_ref().map(|r| shift_runs(r, old, &v))));
            runs.set_value(shifted);
            drawn.set_value(v.clone());
        }
        ctx.text.set(v);
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
        let (ua, ub, line) = ctx.text.with_untracked(|t| (utf16_at(t, a), utf16_at(t, b), t[..a].matches('\n').count()));
        let _ = el.set_selection_range(ua, ub);
        draw();
        let top = layer
            .get_untracked()
            .and_then(|l| l.query_selector(&format!(":scope > .ln:nth-child({})", line + 1)).ok().flatten())
            .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok())
            .map_or(line as i32 * LINE_PX, |e| e.offset_top());
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
                let at = ctx.text.with_untracked(|t| byte_at(t, at));
                hits.with_untracked(|h| h.iter().position(|&(a, _)| a >= at).unwrap_or(0))
            }
        };
        show_hit(i);
    };
    let open_find = move || {
        // The selection, when it's on one line, is what's looked for.
        if let Some(el) = ta.get_untracked() {
            let (a, b) = (el.selection_start().ok().flatten().unwrap_or(0), el.selection_end().ok().flatten().unwrap_or(0));
            let picked = ctx.text.with_untracked(|t| t.get(byte_at(t, a)..byte_at(t, b)).map(str::to_string)).unwrap_or_default();
            if !picked.is_empty() && !picked.contains('\n') {
                query.set(picked);
            }
        }
        finding.set(true);
        shown.set(None);
        refind();
        draw();
        request_animation_frame(move || {
            if let Some(i) = find_box.get_untracked() {
                let _ = i.focus();
                i.select();
            }
        });
    };
    let close_find = move || {
        finding.set(false);
        hits.set(vec![]);
        shown.set(None);
        draw();
        if let Some(el) = ta.get_untracked() {
            let _ = el.focus();
        }
    };
    let keys = move |ev: web_sys::KeyboardEvent| {
        let meta = ev.meta_key() || ev.ctrl_key();
        match ev.key().as_str() {
            "f" if meta => {
                ev.prevent_default();
                open_find();
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
                type_text(&el, ctx.text.with_untracked(|t| indent_unit(t)));
            }
            // A new line starts as indented as the one it's split from.
            "Enter" if !ev.shift_key() => {
                let at = el.selection_start().ok().flatten().unwrap_or(0);
                let indent = ctx.text.with_untracked(|t| {
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
        el.set_value(&ctx.text.get_untracked());
        if drawn_on {
            drawn.set_value(ctx.text.get_untracked());
        }
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

    let path = move || ctx.editor.get().map(|e| e.path).unwrap_or_default();
    let dirty = Memo::new(move |_| ctx.dirty());
    view! {
        <div class="bar">
            <button class="btn back" title="Back to files" aria-label="Back to files" on:click=move |_| ctx.close_editor(false)><svg class="back-icon" viewBox="0 0 16 16" aria-hidden="true"><path d="M13 8H3M7.5 3.5 3 8l4.5 4.5"/></svg></button>
            <span class="title">{path}{move || dirty.get().then_some(" •")}</span>
            <span class="kind">{move || ctx.editor.get().map(|e| match e.src {
                Src::Mac => crate::os::computer().to_string(),
                src => ctx.src_info(src).map_or_else(|| "Device".into(), |i| i.label),
            })}</span>
            <span class="spacer"></span>
            <button class="btn icon-only" class:active=move || finding.get() title=crate::os::keys("Find (⌘F)") aria-label="Find"
                on:click=move |_| if finding.get_untracked() { close_find() } else { open_find() }>{ico(Icon::Search)}</button>
            <button class="btn icon-only" class:active=move || wrap.get() aria-pressed=move || wrap.get().to_string()
                aria-label="Wrap long lines" title=move || if wrap.get() { "Wrap long lines: on" } else { "Wrap long lines: off" }
                on:click=move |_| { ctx.set_wrap_text(!wrap.get_untracked()); request_animation_frame(move || { draw(); }); }>{ico(Icon::Wrap)}</button>
            {drawn_on.then(|| view! {
                <button class="btn icon-only" class:active=move || numbers.get() aria-pressed=move || numbers.get().to_string()
                    aria-label="Line numbers" title=move || if numbers.get() { "Line numbers: on" } else { "Line numbers: off" }
                    on:click=move |_| ctx.set_line_numbers(!numbers.get_untracked())>{ico(Icon::LineNumbers)}</button>
            })}
            <button class="btn primary" title=crate::os::keys("⌘S") disabled=move || !dirty.get() || ctx.busy.get() on:click=move |_| ctx.save()>{ico(Icon::Save)}"Save"</button>
        </div>
        {move || ctx.confirm_discard.get().then(|| view! {
            <div class="notice">
                <span class="text">"You have unsaved changes."</span>
                <button class="btn primary" on:click=move |_| ctx.save()>{ico(Icon::Save)}"Save"</button>
                <button class="btn danger" on:click=move |_| ctx.close_editor(true)>{ico(Icon::Trash)}"Discard changes"</button>
                <button class="btn" on:click=move |_| ctx.confirm_discard.set(false)>"Keep editing"</button>
            </div>
        })}
        {move || finding.get().then(|| view! {
            <div class="find-bar" on:keydown=keys>
                <div class="filter-box" class:bad=move || !query.with(String::is_empty) && hits.with(Vec::is_empty)>
                    {ico(Icon::Search)}
                    <input class="filter" node_ref=find_box type="text" placeholder="Find" aria-label="Find" spellcheck="false"
                        prop:value=move || query.get()
                        on:input=move |ev| { query.set(event_target_value(&ev)); shown.set(None); refind(); draw(); }
                        on:keydown=find_keys/>
                </div>
                <span class="count">{move || {
                    let n = hits.with(Vec::len);
                    match (n, shown.get()) {
                        _ if query.with(String::is_empty) => String::new(),
                        (0, _) => "No matches".into(),
                        (n, Some(i)) => format!("{} of {n}{}", i + 1, if n >= FIND_MAX { "+" } else { "" }),
                        (n, None) => format!("{n}{} {}", if n >= FIND_MAX { "+" } else { "" }, if n == 1 { "match" } else { "matches" }),
                    }
                }}</span>
                <button class="btn icon-only" title="Previous match (⇧Enter)" aria-label="Previous match" disabled=move || hits.with(Vec::is_empty)
                    on:click=move |_| step(true)>{ico(Icon::Prev)}</button>
                <button class="btn icon-only" title="Next match (Enter)" aria-label="Next match" disabled=move || hits.with(Vec::is_empty)
                    on:click=move |_| step(false)>{ico(Icon::Next)}</button>
                <button class="btn icon-only" title="Close (Esc)" aria-label="Close find" on:click=move |_| close_find()>{ico(Icon::Close)}</button>
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
            <span class=move || format!("status {}", ctx.status.get().1)>{move || {
                let (msg, _) = ctx.status.get();
                if !msg.is_empty() { msg } else if dirty.get() { "Unsaved changes".into() } else { "Saved".into() }
            }}</span>
            <span class="spacer"></span>
            {move || stopped.get().then(|| view! { <span title="Colouring stopped partway, far into the file or where it got slow">"Colours stop partway"</span> })}
            {(!drawn_on).then(|| view! { <span title="Colours, line numbers and find marks are off for text over 512 KB">"Plain: big file"</span> })}
            <span>{move || { let (l, c) = caret.get(); format!("Ln {l}, Col {c}") }}</span>
            <span>{language}</span>
            <span>{move || {
                let n = lines.get();
                let chars = ctx.text.with(|t| t.chars().count());
                format!("{n} {} · {chars} {}", if n == 1 { "line" } else { "lines" }, if chars == 1 { "character" } else { "characters" })
            }}</span>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let h = code_html("a<b\n\nc", None, &[], None);
        assert_eq!(h, "<span class=\"ln\" data-n=\"1\">a&lt;b\n</span><span class=\"ln\" data-n=\"2\">\n</span><span class=\"ln\" data-n=\"3\">c\n</span>");
        // A last empty line still gets a line.
        assert!(code_html("a\n", None, &[], None).ends_with("<span class=\"ln\" data-n=\"2\">\n</span>"));
        assert_eq!(code_html("", None, &[], None), "<span class=\"ln\" data-n=\"1\">\n</span>");
    }

    #[test]
    fn html_marks_matches_and_colours() {
        let t = "let x";
        // Runs that don't fit the text are left out.
        assert_eq!(code_html(t, Some(&[(4, 9)]), &[], None), "<span class=\"ln\" data-n=\"1\">let x\n</span>");
        let h = code_html(t, Some(&[(4, 3), (0, 2)]), &find_all(t, "ET"), Some(0));
        assert_eq!(h, "<span class=\"ln\" data-n=\"1\"><span class=\"tok-keyword\">l</span><mark class=\"hit on\"><span class=\"tok-keyword\">et</span></mark> x\n</span>");
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
