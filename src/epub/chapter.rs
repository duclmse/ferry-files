//! A chapter's view: visual (an editable iframe holding the chapter as HTML) or source, with
//! the formatting buttons.

use super::*;

impl Ep {
    pub(super) fn frame_doc(self) -> Option<web_sys::HtmlDocument> {
        self.frame
            .get_untracked()?
            .content_document()?
            .dyn_into()
            .ok()
    }

    /// The edited <body> content as XHTML.
    pub(super) fn visual_body(self) -> Result<String, String> {
        let doc = self
            .frame_doc()
            .ok_or("The chapter view isn't ready yet.")?;
        let body = doc.body().ok_or("The chapter has no body.")?;
        let ser = web_sys::XmlSerializer::new().map_err(|e| format!("{e:?}"))?;
        let kids = body.child_nodes();
        let mut out = String::new();
        for i in 0..kids.length() {
            let Some(n) = kids.item(i) else { continue };
            let s = ser.serialize_to_string(&n).map_err(|e| format!("{e:?}"))?;
            if n.node_type() == web_sys::Node::ELEMENT_NODE {
                // The serializer marks each top-level element with the XHTML namespace; <html> already declares it.
                out.push_str(&s.replacen(r#" xmlns="http://www.w3.org/1999/xhtml""#, "", 1));
            } else {
                out.push_str(&s);
            }
        }
        Ok(out)
    }

    /// Zooms the chapter. In Visual the page is built again: its edits are sent first, and it's
    /// read back with them.
    pub(super) fn set_zoom(self, z: u32) {
        let z = z.clamp(ZOOM_MIN, ZOOM_MAX);
        let keep = move || {
            self.zoom.set(z);
            crate::app::store(ZOOM_KEY, &z.to_string());
        };
        if self.mode.get_untracked() != Mode::Visual {
            keep();
            return;
        }
        self.act(move || async move {
            keep();
            if let View::Chapter(p) = self.view.get_untracked() {
                self.load_chapter(&p).await?;
            }
            Ok(())
        });
    }

    /// Gives the visual page the focus back, with the cursor or selection it had.
    pub(super) fn refocus(self) {
        let Some(frame) = self.frame.get_untracked() else {
            return;
        };
        let Some(doc) = self.frame_doc() else { return };
        if let Some(w) = frame.content_window() {
            let _ = w.focus();
        }
        if let (Some(r), Ok(Some(sel))) = (self.range.get_value(), doc.get_selection()) {
            let _ = sel.remove_all_ranges();
            let _ = sel.add_range(&r);
        }
    }

    pub(super) fn frame_loaded(self) {
        self.range.set_value(None);
        if let Some(id) = self.anchor.try_update_value(Option::take).flatten() {
            self.scroll_to(&id);
        }
        let Some(doc) = self.frame_doc() else { return };
        let kept = doc.clone();
        let moved = Closure::<dyn FnMut()>::new(move || {
            let r = kept
                .get_selection()
                .ok()
                .flatten()
                .filter(|s| s.range_count() > 0)
                .and_then(|s| s.get_range_at(0).ok());
            if r.is_some() {
                self.range.set_value(r);
            }
        });
        let _ =
            doc.add_event_listener_with_callback("selectionchange", moved.as_ref().unchecked_ref());
        moved.forget();
        doc.set_design_mode("on");
        let _ = doc.exec_command_with_show_ui_and_value("defaultParagraphSeparator", false, "p");
        let input =
            Closure::<dyn FnMut(web_sys::Event)>::new(move |_| self.chapter_dirty.set(true));
        let _ = doc.add_event_listener_with_callback("input", input.as_ref().unchecked_ref());
        input.forget();
        // Key presses inside the iframe don't reach the app window, so ⌘S is caught here too.
        let keys =
            Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |e: web_sys::KeyboardEvent| {
                if !(e.meta_key() || e.ctrl_key()) {
                    return;
                }
                match e.key().as_str() {
                    "s" => self.save(),
                    "f" | "ƒ" => self.open_find(e.alt_key()),
                    "r" => self.open_find(true),
                    _ => return,
                }
                e.prevent_default();
            });
        let _ = doc.add_event_listener_with_callback("keydown", keys.as_ref().unchecked_ref());
        keys.forget();
    }

    /// Where to cut the open chapter to move part of it, from the cursor or selection, as the
    /// arguments of `epub_move`. To the previous chapter goes what's before the cursor's block (or
    /// up to the end of the selection's last); to the next, the cursor's block (or the
    /// selection's first) and what follows.
    pub(super) fn cut(self, next: bool) -> Result<serde_json::Value, String> {
        const NO_CURSOR: &str = "Click in the chapter where it should be cut first.";
        match self.mode.get_untracked() {
            Mode::Source => {
                let ta = self.source_box.get_untracked().ok_or(NO_CURSOR)?;
                let text = ta.value();
                let (start, end) = (
                    ta.selection_start().ok().flatten().unwrap_or(0),
                    ta.selection_end().ok().flatten().unwrap_or(0),
                );
                // The box counts UTF-16 units; the backend counts bytes, with a line break as one.
                let bytes = |units: u32| {
                    String::from_utf16_lossy(
                        &text.encode_utf16().take(units as usize).collect::<Vec<_>>(),
                    )
                    .len()
                };
                let (offset, through) = if !next && start != end {
                    (bytes(end).saturating_sub(1), true)
                } else {
                    (bytes(start), false)
                };
                Ok(json!({ "offset": offset, "through": through }))
            }
            Mode::Visual => {
                self.refocus();
                let doc = self.frame_doc().ok_or(NO_CURSOR)?;
                let body = doc.body().ok_or("The chapter has no body.")?;
                let body_node: &web_sys::Node = body.as_ref();
                let sel = doc
                    .get_selection()
                    .ok()
                    .flatten()
                    .filter(|s| s.range_count() > 0)
                    .ok_or(NO_CURSOR)?;
                let range = sel.get_range_at(0).map_err(|_| NO_CURSOR)?;
                let kids: Vec<web_sys::Node> = {
                    let list = body.child_nodes();
                    (0..list.length()).filter_map(|i| list.item(i)).collect()
                };
                let is_element = |n: &web_sys::Node| n.node_type() == web_sys::Node::ELEMENT_NODE;
                let elements = kids.iter().filter(|n| is_element(n)).count();
                // The top-level element a point is in, as an index among them; a point between
                // them counts as in the next one.
                let block_of = |node: web_sys::Node, offset: u32| -> usize {
                    let top = if node.is_same_node(Some(body_node)) {
                        kids.get(offset as usize).cloned()
                    } else {
                        let mut n = node;
                        loop {
                            match n.parent_node() {
                                Some(p) if p.is_same_node(Some(body_node)) => break Some(n),
                                Some(p) => n = p,
                                None => break None,
                            }
                        }
                    };
                    let Some(top) = top else { return elements };
                    let at = kids
                        .iter()
                        .position(|k| k.is_same_node(Some(&top)))
                        .unwrap_or(kids.len());
                    kids[..at].iter().filter(|n| is_element(n)).count()
                };
                let block = if !next && !range.collapsed() {
                    let k = block_of(
                        range.end_container().map_err(|_| NO_CURSOR)?,
                        range.end_offset().unwrap_or(0),
                    );
                    (k + 1).min(elements)
                } else {
                    block_of(
                        range.start_container().map_err(|_| NO_CURSOR)?,
                        range.start_offset().unwrap_or(0),
                    )
                };
                let names: Vec<String> = kids
                    .iter()
                    .filter_map(|n| n.dyn_ref::<web_sys::Element>().map(|e| e.local_name()))
                    .collect();
                Ok(json!({ "block": block, "names": names }))
            }
        }
    }

    /// Moves part of the open chapter into the chapter before it or after it (see `cut`).
    pub(super) fn move_part(self, next: bool) {
        let View::Chapter(path) = self.view.get_untracked() else {
            return;
        };
        let cut = match self.cut(next) {
            Ok(c) => c,
            Err(e) => return self.say(e, "error"),
        };
        self.act(move || async move {
            let mut args = cut;
            args["id"] = json!(self.id());
            args["path"] = json!(path);
            args["next"] = json!(next);
            let m = call::<Moved>("epub_move", args).await?;
            self.book.set(Some(m.book));
            self.load_chapter(&path).await?;
            let links = match m.links {
                0 => String::new(),
                n => format!(" {} changed to match.", plural(n, "link", "links")),
            };
            let place = if next { "start" } else { "end" };
            self.say(
                format!(
                    "Moved {} to the {place} of “{}”.{links} Save to write the book to {}.",
                    plural(m.blocks, "block", "blocks"),
                    m.to,
                    self.place()
                ),
                "ok",
            );
            Ok(())
        });
    }

    pub(super) fn exec(self, cmd: &str, arg: &str) {
        self.refocus();
        if let Some(doc) = self.frame_doc() {
            let _ = doc.exec_command_with_show_ui_and_value(cmd, false, arg);
            self.chapter_dirty.set(true);
        }
    }
}

type Format = (&'static str, &'static str, &'static str, &'static str);

const INLINE: &[Format] = &[("bold", "", "B", "Bold"), ("italic", "", "I", "Italic")];

/// Headings, paragraph and quote: buttons, or one menu when the bar is narrow.
const BLOCKS: &[Format] = &[
    ("formatBlock", "h1", "H1", "Heading 1"),
    ("formatBlock", "h2", "H2", "Heading 2"),
    ("formatBlock", "h3", "H3", "Heading 3"),
    ("formatBlock", "p", "¶", "Paragraph"),
    ("formatBlock", "blockquote", "“ ”", "Quote"),
];

const MORE: &[Format] = &[
    ("insertUnorderedList", "", "• List", "Bulleted list"),
    ("removeFormat", "", "Clear", "Clear formatting"),
    ("undo", "", "↶", "Undo"),
    ("redo", "", "↷", "Redo"),
];

fn fmt_buttons(ep: Ep, which: &'static [Format]) -> impl IntoView {
    which
        .iter()
        .map(|&(cmd, arg, text, title)| view! { <button title=title aria-label=title on:click=move |_| ep.exec(cmd, arg)>{text}</button> })
        .collect_view()
}

/// The block styles as one button and its menu, shown instead of their buttons when the bar is
/// narrow (styles/epub-editor.css).
#[component]
pub(super) fn BlockMenu() -> impl IntoView {
    let ep = expect_context::<Ep>();
    let open = RwSignal::new(false);
    let menu = NodeRef::<html::Span>::new();
    let away = window_event_listener(leptos::ev::mousedown, move |e| {
        let inside = menu
            .get_untracked()
            .zip(e.target().and_then(|t| t.dyn_into::<web_sys::Node>().ok()))
            .is_some_and(|(m, t)| m.contains(Some(&t)));
        if !inside {
            open.set(false);
        }
    });
    on_cleanup(move || away.remove());
    view! {
        <span class="fmt-block-menu" node_ref=menu>
            <button class:active=open title="Heading, paragraph or quote" aria-label="Heading, paragraph or quote" aria-haspopup="menu"
                aria-expanded=move || open.get().to_string() on:click=move |_| open.update(|o| *o = !*o)>"H ▾"</button>
            {move || open.get().then(|| view! {
                <span class="fmt-popup" role="menu">
                    {BLOCKS.iter().map(|&(cmd, arg, text, title)| view! {
                        <button role="menuitem" title=title on:click=move |_| {
                            open.set(false);
                            ep.exec(cmd, arg);
                        }>
                            <span class="mark">{text}</span>
                            {title}
                        </button>
                    }).collect_view()}
                </span>
            })}
        </span>
    }
}

#[component]
pub(super) fn ChapterView() -> impl IntoView {
    let ep = expect_context::<Ep>();
    let visual_ok = move || {
        ep.doc
            .with(|d| d.as_ref().is_some_and(|d| d.html.is_some()))
    };
    // Where this chapter is in reading order, for moving part of it to the one before or after.
    let path = Memo::new(move |_| match ep.view.get() {
        View::Chapter(p) => p,
        View::Info => String::new(),
    });
    let here = Memo::new(move |_| {
        path.with(|path| {
            ep.book.with(|b| {
                b.as_ref()
                    .and_then(|b| b.chapters.iter().position(|c| c.path == *path))
            })
        })
    });
    // What the page is: none yet, the visual page, a chapter that can't show visually, or its
    // XHTML (drawn anew for each chapter). The visual page stays from chapter to chapter: only
    // what it shows changes.
    let kind = Memo::new(move |_| {
        let mode = ep.mode.get();
        let shown = ep.doc.with(|d| match d {
            None => 0u8,
            Some(d) if d.html.is_some() => 1,
            Some(_) => 2,
        });
        let source = if mode == Mode::Source && shown > 0 {
            ep.doc.with(|_| path.get())
        } else {
            String::new()
        };
        (mode, shown, source)
    });
    let count = move || ep.book.with(|b| b.as_ref().map_or(0, |b| b.chapters.len()));
    // The tools after Visual and Source fold into a menu button only when the bar can't hold
    // them: `fit` lays them out in full (`measuring`) to see whether they fit, before the paint.
    let more = RwSignal::new(false);
    let compact = RwSignal::new(false);
    let bar = NodeRef::<html::Div>::new();
    let fit = move || {
        let Some(el) = bar.try_get_untracked().flatten() else {
            return;
        };
        let classes = el.class_list();
        let _ = classes.add_1("measuring");
        let fits = el.scroll_width() <= el.client_width() + 1;
        let _ = classes.remove_1("measuring");
        if compact.get_untracked() == fits {
            compact.set(!fits);
        }
        if fits && more.get_untracked() {
            more.set(false);
        }
    };
    // When what the bar holds changes (Visual or Source, a chapter that can't show visually).
    Effect::new(move |_| {
        ep.mode.track();
        ep.doc.track();
        request_animation_frame(fit);
    });
    let watcher = StoredValue::new_local(None::<(web_sys::ResizeObserver, Closure<dyn Fn()>)>);
    Effect::new(move |_| {
        let Some(node) = bar.get() else { return };
        let cb = Closure::<dyn Fn()>::new(fit);
        if let Ok(ro) = web_sys::ResizeObserver::new(cb.as_ref().unchecked_ref()) {
            ro.observe(&node);
            if let Some((old, _)) = watcher.try_update_value(|w| w.replace((ro, cb))).flatten() {
                old.disconnect();
            }
        }
    });
    on_cleanup(move || {
        if let Some((ro, _)) = watcher.try_update_value(Option::take).flatten() {
            ro.disconnect();
        }
    });
    // When the preview moves to another file, the frame goes with it: its edits are kept to send later.
    on_cleanup(move || {
        if ep.mode.get_untracked() == Mode::Visual && ep.chapter_dirty.get_untracked() {
            if let Ok(b) = ep.visual_body() {
                ep.kept.set_value(Some(b));
            }
        }
    });
    view! {
        // mousedown is kept from the bar, so clicking it leaves the focus and the cursor in the page.
        <div class="viewer-bar tools" class:compact=compact node_ref=bar on:mousedown=|e| e.prevent_default()>
            <div class="seg">
                <button class:active=move || ep.mode.get() == Mode::Visual title="Visual" aria-label="Visual"
                    on:click=move |_| ep.set_mode(Mode::Visual)>{ico(Icon::Eye)}</button>
                <button class:active=move || ep.mode.get() == Mode::Source title="Source" aria-label="Source"
                    on:click=move |_| ep.set_mode(Mode::Source)>{ico(Icon::Code)}</button>
            </div>
            <button class="btn tools-more" class:active=more title="Editing tools" aria-label="Editing tools"
                aria-expanded=move || more.get().to_string() on:click=move |_| more.update(|m| *m = !*m)>{ico(Icon::Menu)}</button>
            <div class="tools-rest" class:open=more>
            <span class="btn-group" role="group">
                <button class="btn" title="Move to the end of the previous chapter: what's before the cursor's paragraph, or up to the end of the selection"
                    aria-label="Move to the previous chapter" disabled=move || here.get().is_none_or(|k| k == 0)
                    on:mousedown=|e| e.prevent_default() on:click=move |_| ep.move_part(false)>{ico(Icon::ToPrevPart)}</button>
                <button class="btn" title="Move to the start of the next chapter: the cursor's paragraph (or the selection's first) and what follows"
                    aria-label="Move to the next chapter" disabled=move || here.get().is_none_or(|k| k + 1 >= count())
                    on:mousedown=|e| e.prevent_default() on:click=move |_| ep.move_part(true)>{ico(Icon::ToNextPart)}</button>
            </span>
            {move || (ep.mode.get() == Mode::Source).then(|| view! {
                <button class="btn" class:active=move || ep.colours.get() title="Colours" aria-label="Colours"
                    on:click=move |_| {
                        let on = !ep.colours.get_untracked();
                        ep.colours.set(on);
                        crate::app::store(COLOURS_KEY, if on { "1" } else { "0" });
                    }>{ico(Icon::Highlight)}</button>
            })}
            {move || (ep.mode.get() == Mode::Visual && visual_ok()).then(|| view! {
                <div class="fmt">
                    {fmt_buttons(ep, INLINE)}
                    <span class="fmt-blocks">{fmt_buttons(ep, BLOCKS)}</span>
                    <BlockMenu/>
                    {fmt_buttons(ep, MORE)}
                </div>
            })}
            </div>
        </div>
        {move || match kind.get() {
            (_, 0, _) => view! { <div class="empty"><span class="spin"></span></div> }.into_any(),
            (Mode::Visual, 1, _) => {
                let page = move || ep.doc.with(|d| d.as_ref().and_then(|d| d.html.as_deref()).map(|h| crate::app::editor_page(h, ep.zoom.get_untracked())).unwrap_or_default());
                view! {
                    // No allow-scripts: a book's scripts must never run next to the app's commands.
                    <iframe class="page" sandbox="allow-same-origin" node_ref=ep.frame srcdoc=page
                        on:load=move |_| ep.frame_loaded()></iframe>
                }.into_any()
            }
            (Mode::Visual, _, _) => view! {
                <div class="notice error">
                    <span class="text">"This chapter isn't well-formed XHTML, so it can't be shown visually. "
                        {move || ep.doc.with(|d| d.as_ref().and_then(|d| d.problem.clone()).unwrap_or_default())}</span>
                    <button class="btn" on:click=move |_| ep.set_mode(Mode::Source)>"Fix it in Source"</button>
                </div>
            }.into_any(),
            (Mode::Source, _, path) => view! { <SourceEdit path/> }.into_any(),
        }}
    }
}

/// The chapter's XHTML: a clear-lettered box over a layer that draws it in colour (as the text
/// editor does), or the plain box when colours are off or the chapter is big.
#[component]
pub(super) fn SourceEdit(path: String) -> impl IntoView {
    let ep = expect_context::<Ep>();
    let layer = NodeRef::<html::Pre>::new();
    let ta = ep.source_box;
    // The runs and the text they were for, and which colouring is the latest.
    let runs = StoredValue::new(None::<Vec<(u8, u32)>>);
    let ran_on = StoredValue::new(String::new());
    let asked = StoredValue::new(0u32);
    let timer = StoredValue::new(None::<leptos::leptos_dom::helpers::TimeoutHandle>);
    // The text as the box holds it, with line breaks as LF.
    let text = move || ep.source.with_untracked(|s| s.replace("\r\n", "\n"));
    let layered = move || ep.colours.get() && ep.source.with(|s| s.len() <= crate::app::LAYER_MAX);
    let draw = move || {
        let Some(l) = layer.get_untracked() else {
            return;
        };
        let t = text();
        let r = ran_on.with_value(|old| {
            runs.with_value(|r| {
                r.as_ref().map(|r| {
                    if *old == t {
                        r.clone()
                    } else {
                        crate::app::shift_runs(r, old, &t)
                    }
                })
            })
        });
        l.set_inner_html(&crate::app::code_html(
            &t,
            r.as_deref(),
            &[],
            None,
            1..usize::MAX,
        ));
        if let Some(b) = ta.get_untracked() {
            l.set_scroll_top(b.scroll_top());
            l.set_scroll_left(b.scroll_left());
        }
    };
    let colour = move || {
        let t = text();
        let n = asked.get_value() + 1;
        asked.set_value(n);
        let name = path.clone();
        spawn_local(async move {
            let Ok(c) =
                call::<TextColours>("colour_text", json!({ "name": name, "text": t })).await
            else {
                return;
            };
            if asked.try_get_value() != Some(n) {
                return;
            }
            runs.set_value(c.runs);
            ran_on.set_value(t);
            draw();
        });
    };
    Effect::new(move |_| {
        ep.source.track();
        if !layered() {
            return;
        }
        draw();
        if let Some(h) = timer.try_update_value(Option::take).flatten() {
            h.clear();
        }
        let colour = colour.clone();
        timer.set_value(
            set_timeout_with_handle(move || colour(), std::time::Duration::from_millis(250)).ok(),
        );
    });
    on_cleanup(move || {
        if let Some(h) = timer.try_update_value(Option::take).flatten() {
            h.clear();
        }
    });
    let scrolled = move |_| {
        if let (Some(b), Some(l)) = (ta.get_untracked(), layer.get_untracked()) {
            l.set_scroll_top(b.scroll_top());
            l.set_scroll_left(b.scroll_left());
        }
    };
    view! {
        <div class="code-edit source" class:layered=layered style=move || format!("--zoom: {}", ep.zoom.get() as f64 / 100.0)>
            <pre class="code-layer" node_ref=layer aria-hidden="true"></pre>
            <textarea class="code-input" node_ref=ta spellcheck="false" autocomplete="off" autocapitalize="off"
                aria-label="Chapter source" prop:value=move || ep.source.get()
                on:input=move |ev| { ep.source.set(event_target_value(&ev)); ep.chapter_dirty.set(true); }
                on:scroll=scrolled></textarea>
        </div>
    }
}
