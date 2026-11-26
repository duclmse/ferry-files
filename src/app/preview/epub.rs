//! An EPUB in the preview: its cover, or its chapters one at a time.

use super::*;

pub(in crate::app) const EPUB_READ_KEY: &str = "ferry-files.epub-read";
/// The text size of a book read in the preview, in percent of 15 px.
const EPUB_ZOOM_KEY: &str = "ferry-files.epub-zoom";

/// An EPUB: its cover, or its chapters one at a time. Only the book's contents list and the
/// chapter shown are read (and its pictures, as they show), not the whole book. `line` is the
/// name and facts; the book's buttons go at the end of that line.
#[component]
pub(super) fn EpubView(pane: Pane, src: Src, entry: Entry, line: AnyView) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let ep = ctx.ep;
    // Editing reads the whole book into the app; reading takes a chapter at a time.
    let editing = {
        let path = entry.path.clone();
        Signal::derive(move || ep.host.get() == Some(pane.id) && ep.holds(&src.key(), &path))
    };
    let opening = RwSignal::new(false);
    let edit_error = RwSignal::new(None::<String>);
    let book_path = StoredValue::new(entry.path.clone());
    let edit = move |_| {
        edit_error.set(None);
        if editing.get_untracked() {
            ep.close(false);
            return;
        }
        opening.set(true);
        let path = book_path.get_value();
        spawn_local(async move {
            let r = ep.open(&src.key(), ctx.place(src), path, pane.id).await;
            let _ = opening.try_set(false);
            if let Err(e) = r {
                let _ = edit_error.try_set(Some(e));
            }
        });
    };
    let reading = ctx.epub_read;
    let contents = RwSignal::new(None::<Result<EpubContents, String>>);
    let at = RwSignal::new(0usize);
    let page = RwSignal::new(None::<Result<String, String>>);
    let asked = StoredValue::new(0u32);
    let (path, size) = (StoredValue::new(entry.path.clone()), entry.size);
    Effect::new(move |_| {
        if !reading.get() || contents.with_untracked(Option::is_some) {
            return;
        }
        let args = json!({ "src": src.key(), "path": path.get_value(), "size": size });
        spawn_local(async move {
            let r = call::<EpubContents>("epub_contents", args).await;
            let _ = contents.try_set(Some(r));
        });
    });
    Effect::new(move |_| {
        if !reading.get() {
            return;
        }
        let Some(ch) = contents.with(|c| {
            c.as_ref()
                .and_then(|c| c.as_ref().ok())
                .and_then(|c| c.chapters.get(at.get()).cloned())
        }) else {
            return;
        };
        let n = asked.get_value() + 1;
        asked.set_value(n);
        page.set(None);
        let args =
            json!({ "src": src.key(), "path": path.get_value(), "size": size, "chapter": ch.path });
        spawn_local(async move {
            let r = call::<String>("epub_peek", args).await;
            if asked.try_get_value() == Some(n) {
                let _ = page.try_set(Some(r));
            }
        });
    });
    let count = move || {
        contents.with(|c| {
            c.as_ref()
                .and_then(|c| c.as_ref().ok())
                .map_or(0, |c| c.chapters.len())
        })
    };
    let go = move |d: i32| {
        let to = at.get_untracked() as i32 + d;
        if to >= 0 && (to as usize) < count() {
            at.set(to as usize);
        }
    };
    let toggle = move |_| {
        if editing.get_untracked() {
            ep.close(false);
            reading.set(true);
            store(EPUB_READ_KEY, "1");
            return;
        }
        let on = !reading.get_untracked();
        reading.set(on);
        store(EPUB_READ_KEY, if on { "1" } else { "0" });
    };
    let zoom = RwSignal::new(
        stored(EPUB_ZOOM_KEY)
            .and_then(|z| z.parse::<u32>().ok())
            .unwrap_or(100)
            .clamp(60, 250),
    );
    let set_zoom = move |z: u32| {
        let z = z.clamp(60, 250);
        zoom.set(z);
        store(EPUB_ZOOM_KEY, &z.to_string());
    };
    let outline = ep.outline;
    let frame = NodeRef::<html::Iframe>::new();
    // The page in a quiet style, after the <base> the backend adds so the book's own styles load.
    // A new zoom builds the page again.
    let doc = move || {
        let html = page.get()?.ok()?;
        let style =
            super::reading::page_style(&ctx.theme.get(), 15, zoom.get(), "16px 20px 60px", "1.55");
        let next = contents.with(|c| {
            c.as_ref()
                .and_then(|c| c.as_ref().ok())
                .and_then(|c| c.chapters.get(at.get() + 1))
                .map(|ch| ch.label.clone())
        });
        Some(super::reading::page(
            &html,
            &style,
            next.as_deref().map(|l| (l, at.get() + 1)),
        ))
    };
    // Scrolling on past a chapter's end turns to the next; back past its start, to the previous
    // one's end.
    let land_end = StoredValue::new(false);
    // Find while reading: the book's chapters are searched (their text read for it), and the
    // matches marked in the chapter shown.
    // Its box and results are the Search tab of the pane beside the page (the other tab is the
    // chapters); finding is that tab showing.
    let search_tab = RwSignal::new(false);
    let finding = Memo::new(move |_| ep.outline.get() && search_tab.get());
    let fq = RwSignal::new(Query::default());
    let found = RwSignal::new(None::<Result<Found, String>>);
    let searching = RwSignal::new(false);
    let find_focus = RwSignal::new(0u32);
    let open_find = move || {
        search_tab.set(true);
        if !ep.outline.get_untracked() {
            ep.toggle_outline();
        }
        find_focus.update(|n| *n += 1);
    };
    // Esc in the Search tab: back to the chapters.
    let close_find = move || search_tab.set(false);
    let show_chapters = move || {
        if ep.outline.get_untracked() && !search_tab.get_untracked() {
            ep.toggle_outline();
        } else {
            search_tab.set(false);
            if !ep.outline.get_untracked() {
                ep.toggle_outline();
            }
        }
    };
    // The page's marks go when the Search tab does.
    Effect::new(move |was: Option<bool>| {
        let now = finding.get();
        if was == Some(true) && !now {
            if let Some(f) = frame.get_untracked() {
                super::reading::unmark(&f);
            }
        }
        now
    });
    // Searches now, or (`now` false, while typing) once typing has stopped for a moment. A
    // search still running when another starts is let go: only the last one's results show.
    let asked_search = StoredValue::new(0u32);
    let timer = StoredValue::new(None::<TimeoutHandle>);
    let run_search = move || {
        let query = fq.get_untracked();
        let n = asked_search.get_value() + 1;
        asked_search.set_value(n);
        if query.find.trim().is_empty() {
            searching.set(false);
            found.set(None);
            if let Some(f) = frame.get_untracked() {
                super::reading::unmark(&f);
            }
            return;
        }
        searching.set(true);
        let args =
            json!({ "src": src.key(), "path": path.get_value(), "size": size, "query": query });
        spawn_local(async move {
            let r = call::<Found>("epub_peek_find", args).await;
            if asked_search.try_get_value() != Some(n) {
                return;
            }
            let _ = searching.try_set(false);
            let _ = found.try_set(Some(r));
            if let Some(f) = frame.try_get_untracked().flatten() {
                super::reading::mark(&f, &query);
            }
        });
    };
    let search = move |now: bool| {
        if let Some(h) = timer.get_value() {
            h.clear();
        }
        if now {
            run_search();
        } else {
            timer.set_value(
                set_timeout_with_handle(run_search, std::time::Duration::from_millis(300)).ok(),
            );
        }
    };
    // A match picked in the results: its chapter, with that match the current one. In another
    // chapter, it's marked once that one has loaded.
    let current = RwSignal::new(None::<(String, usize)>);
    let pending_hit = StoredValue::new(None::<usize>);
    let chapter_at = move |p: &str| {
        contents.with_untracked(|c| {
            c.as_ref()
                .and_then(|c| c.as_ref().ok())
                .and_then(|c| c.chapters.iter().position(|ch| ch.path == p))
        })
    };
    let pick = move |p: String, hit: usize| {
        let Some(k) = chapter_at(&p) else { return };
        current.set(Some((p, hit)));
        if k == at.get_untracked() {
            if let Some(f) = frame.get_untracked() {
                super::reading::mark(&f, &fq.get_untracked());
                super::reading::focus_mark(&f, hit);
            }
        } else {
            pending_hit.set_value(Some(hit));
            land_end.set_value(false);
            at.set(k);
        }
    };
    // A link in the page: to another chapter (or a place in it), or a place in this one. Links
    // out of the book aren't followed.
    let anchor = StoredValue::new(None::<String>);
    let link = move |href: String| {
        let Ok(u) = web_sys::Url::new(&href) else {
            return;
        };
        let decode = |s: String| {
            js_sys::decode_uri_component(&s)
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or(s)
        };
        let frag =
            Some(decode(u.hash().trim_start_matches('#').to_string())).filter(|f| !f.is_empty());
        let target = decode(u.pathname());
        let k = contents.with_untracked(|c| {
            c.as_ref().and_then(|c| c.as_ref().ok()).and_then(|c| {
                c.chapters
                    .iter()
                    .position(|ch| target.ends_with(&format!("/{}", ch.path)))
            })
        });
        match k {
            Some(k) if k == at.get_untracked() => {
                if let (Some(f), Some(id)) = (frame.get_untracked(), frag) {
                    super::reading::scroll_to(&f, &id);
                }
            }
            Some(k) => {
                anchor.set_value(frag);
                land_end.set_value(false);
                at.set(k);
            }
            None => {}
        }
    };
    // ⌘F from the app's window, with the focus in this preview (app.rs).
    let asked_find = StoredValue::new(ctx.read_find.get_untracked().1);
    Effect::new(move |_| {
        let (id, n) = ctx.read_find.get();
        if n != asked_find.get_value() {
            asked_find.set_value(n);
            if id == pane.id && reading.get_untracked() && !editing.get_untracked() {
                open_find();
            }
        }
    });
    let turn = move |d: i32| {
        let to = at.get_untracked() as i32 + d;
        if to >= 0 && (to as usize) < count() {
            land_end.set_value(d < 0);
            at.set(to as usize);
        }
    };
    let loaded = move |e: web_sys::Event| {
        let end = land_end.get_value();
        land_end.set_value(false);
        let frame = event_target::<web_sys::HtmlIFrameElement>(&e);
        match super::reading::followed(&frame) {
            Some(k) if k < count() => at.set(k),
            Some(_) => {}
            None => {
                super::reading::follow(&frame, end, turn, open_find, link);
                if finding.get_untracked() {
                    super::reading::mark(&frame, &fq.get_untracked());
                }
                if let Some(hit) = pending_hit.try_update_value(Option::take).flatten() {
                    super::reading::focus_mark(&frame, hit);
                } else if let Some(id) = anchor.try_update_value(Option::take).flatten() {
                    super::reading::scroll_to(&frame, &id);
                }
            }
        }
    };
    let chapter_list = move || {
        contents.with(|c| match c {
            Some(Ok(c)) => c
                .chapters
                .iter()
                .enumerate()
                .map(|(k, ch)| view! { <option value=k.to_string()>{ch.label.clone()}</option> })
                .collect_view()
                .into_any(),
            _ => ().into_any(),
        })
    };
    let outline_list = move || {
        contents.with(|c| match c {
            Some(Ok(c)) if c.chapters.is_empty() => view! { <span class="hint">"This book lists no chapters."</span> }.into_any(),
            Some(Ok(c)) => c
                .chapters
                .iter()
                .enumerate()
                .map(|(k, ch)| {
                    let label = ch.label.clone();
                    view! {
                        <button class="pdf-head" class:here=move || at.get() == k title=label.clone() on:click=move |_| at.set(k)>
                            <span class="name">{label.clone()}</span>
                        </button>
                    }
                })
                .collect_view()
                .into_any(),
            Some(Err(e)) => view! { <span class="hint">{e.clone()}</span> }.into_any(),
            None => view! { <span class="hint">"Loading…"</span> }.into_any(),
        })
    };
    // Narrow, the bar drops the zoom percentage, then the chapter list, then the zoom.
    let bar = NodeRef::<html::Div>::new();
    fold_bar(bar, 3);
    let chapter = move || {
        match (contents.get(), page.get()) {
        (Some(Err(e)), _) | (_, Some(Err(e))) => view! { <p class="hint">{e}</p> }.into_any(),
        (Some(Ok(c)), _) if c.chapters.is_empty() => view! { <p class="hint">"This book lists no chapters."</p> }.into_any(),
        (Some(Ok(_)), Some(Ok(_))) => view! {
            // No allow-scripts: a book's scripts must never run next to the app's commands.
            <iframe class="epub-page" sandbox="allow-same-origin" node_ref=frame srcdoc=doc title="Chapter" on:load=loaded></iframe>
        }
        .into_any(),
        _ => view! { <p class="hint">"Opening…"</p> }.into_any(),
    }
    };
    view! {
        <div class="meta">
            <div class="info">{line}</div>
            <div class="meta-tools">
                {move || opening.get().then(|| view! { <span class="spin"></span> })}
                <span class="btn-group" role="group">
                    <button class="btn" class:active=move || reading.get() && !editing.get() title="Read the book here" aria-label="Read the book here" on:click=toggle>{ico(Icon::Book)}</button>
                    <button class="btn" class:active=editing title="Edit the book" aria-label="Edit the book" disabled=opening on:click=edit>{ico(Icon::Edit)}</button>
                </span>
            </div>
        </div>
        <div class="view" class:epub-view=move || editing.get() || reading.get()>
            {move || edit_error.get().map(|e| view! {
                <div class="notice error">
                    <span class="text">{e}</span>
                    <button class="btn" title="Dismiss" aria-label="Dismiss" on:click=move |_| edit_error.set(None)>{ico(Icon::Close)}</button>
                </div>
            })}
            {move || if editing.get() {
                view! {
                    <div class="viewer-bar epub-bar book-editor" node_ref=bar><crate::epub::EditBar/></div>
                    <crate::epub::EditBody/>
                }
                .into_any()
            } else if !reading.get() {
                view! { <BigCover src entry=entry.clone()/> }.into_any()
            } else {
                view! {
                    <div class="viewer-bar epub-bar book-reader" node_ref=bar>
                        // The pane beside the page: its Chapters tab, and its Search tab.
                        <span class="btn-group" role="group">
                            <button class="btn" class:active=move || outline.get() && !search_tab.get() title="Chapters" aria-label="Chapters"
                                on:click=move |_| show_chapters()>{ico(Icon::Contents)}</button>
                            <button class="btn" class:active=finding title=crate::os::keys("Find in the book (⌘F)") aria-label="Find in the book"
                                aria-pressed=move || finding.get().to_string()
                                on:click=move |_| if finding.get_untracked() { ep.toggle_outline() } else { open_find() }>{ico(Icon::Search)}</button>
                        </span>
                        <span class="btn-group" role="group">
                            <button class="btn" title="Smaller text" aria-label="Smaller text" on:click=move |_| set_zoom(zoom.get_untracked().saturating_sub(10))>{ico(Icon::ZoomOut)}</button>
                            <button class="btn zoom" title="Back to 100%" on:click=move |_| set_zoom(100)>{move || format!("{}%", zoom.get())}</button>
                            <button class="btn" title="Bigger text" aria-label="Bigger text" on:click=move |_| set_zoom(zoom.get_untracked() + 10)>{ico(Icon::ZoomIn)}</button>
                        </span>
                        <select class="epub-chapters grow" aria-label="Chapter" prop:value=move || at.get().to_string()
                            on:keydown=|ev| ev.stop_propagation()
                            on:change=move |ev| at.set(event_target_value(&ev).parse().unwrap_or(0))>{chapter_list}</select>
                        <span class="epub-of">{move || match count() { 0 => String::new(), n => format!("{} of {n}", at.get() + 1) }}</span>
                        <span class="btn-group" role="group">
                            <button class="btn" title="Previous chapter" aria-label="Previous chapter" disabled=move || {at.get() == 0} on:click=move |_| go(-1)>{ico(Icon::Prev)}</button>
                            <button class="btn" title="Next chapter" aria-label="Next chapter" disabled=move || {at.get() + 1 >= count()} on:click=move |_| go(1)>{ico(Icon::Next)}</button>
                        </span>
                    </div>
                    <div class="epub-body book-reader">
                        {move || outline.get().then(|| view! {
                            <nav class="pdf-outline epub-outline">
                                <div class="outline-tabs" role="tablist">
                                    <button role="tab" class:on=move || !search_tab.get() aria-selected=move || (!search_tab.get()).to_string()
                                        title="Chapters" on:click=move |_| search_tab.set(false)>{ico(Icon::Contents)}"Chapters"</button>
                                    <button role="tab" class:on=search_tab aria-selected=move || search_tab.get().to_string()
                                        title=crate::os::keys("Search the book (⌘F)") on:click=move |_| open_find()>{ico(Icon::Search)}"Search"</button>
                                </div>
                                {move || if search_tab.get() {
                                    super::reading::find_bar(super::reading::BOOK_WORDS, fq, found, searching, find_focus, search, pick, current, close_find).into_any()
                                } else {
                                    outline_list().into_any()
                                }}
                            </nav>
                        })}
                        {chapter}
                    </div>
                }
                .into_any()
            }}
        </div>
    }
}

/// The cover of an EPUB packed in a ZIP, from `url`; the book is unpacked for it.
#[component]
pub(super) fn ZippedCover(url: String) -> impl IntoView {
    let state = RwSignal::new(None::<bool>);
    view! {
        <div class="pic" class:whole=move || state.get() == Some(true)>
            {move || match state.get() {
                None => view! { <span class="spin"></span> }.into_any(),
                Some(false) => view! { <span class="hint">"This book has no cover."</span> }.into_any(),
                Some(true) => ().into_any(),
            }}
            <img class="full" src=url alt="Book cover" draggable="false" class:pending=move || state.get() != Some(true)
                on:load=move |_| state.set(Some(true)) on:error=move |_| state.set(Some(false))/>
        </div>
    }
}
