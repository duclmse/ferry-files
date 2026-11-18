//! The ways a pane shows a folder: list, thumbnails and columns.

use super::*;

#[component]
pub(super) fn ListView(pane: Pane) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let p = pane;
    let (over, drop) = drop_events(ctx, p, None);
    let here = format!("{}:", p.id);
    view! {
        <div class="list" class:drop=move || ctx.drop_at.get().as_deref() == Some(here.as_str())
            on:click=move |_| p.select_only(None) on:contextmenu=move |ev| ctx.show_menu(p, &ev, None)
            on:dragover=over on:drop=drop>
            <table>
                // The name takes what's left; the others have the widths dragged on their left edge.
                <colgroup>
                    <col/>
                    {LIST_W.iter().enumerate().map(|(n, &default)| view! {
                        <col style=move || format!("width: {}px", match p.list_w[n].get() { w if w > 0.0 => w, _ => default })/>
                    }).collect_view()}
                </colgroup>
                <thead><tr>
                    {SortKey::ALL.into_iter().enumerate().map(|(i, k)| {
                        let num = matches!(k, SortKey::Date | SortKey::Size);
                        view! {
                            <th class:num=num>
                                {(i > 0).then(|| view! {
                                    <Grip size=p.list_w[i - 1] key=format!("yomi-files.list-w.{}.{}", p.id, i - 1)
                                        min=50.0 max=400.0 target=Target::ParentLeft class="grip col-grip"/>
                                })}
                                <button class="sort" title=format!("Sort by {}", k.label().to_lowercase())
                                on:click=move |_| p.set_sort(k)>
                                {k.label()}
                                <span class="arrow">{move || match p.sort.get() {
                                    Sort(s, asc) if s == k => if asc { "▲" } else { "▼" },
                                    _ => "",
                                }}</span>
                            </button></th>
                        }
                    }).collect_view()}
                </tr></thead>
                <tbody>
                    <For each=move || p.entries.get() key=|e| (e.path.clone(), e.size, e.mtime) let(e)>
                        {
                            let k = kind(&e);
                            let path = e.path.clone();
                            let editing = renaming(p, &e);
                            let (click, dbl, menu) = row_events(ctx, p, &e);
                            let where_ = p.found.get_untracked().is_some().then(|| parent(&e.path));
                            let (dir, key) = (e.dir.then(|| e.path.clone()), format!("{}:{}", p.id, e.path));
                            let (over, drop) = drop_events(ctx, p, dir.clone());
                            view! {
                                <tr class:selected=move || p.is_picked(&path) class:drop=move || ctx.drop_at.get().as_deref() == Some(key.as_str())
                                    draggable=move || (!editing()).to_string() data-dir=dir
                                    on:dragstart=drag_start(ctx, p, e.path.clone()) on:dragend=drag_end(ctx)
                                    on:dragover=move |ev| if e.dir { over(ev) } on:drop=move |ev| if e.dir { drop(ev) }
                                    on:click=click on:dblclick=dbl on:contextmenu=menu>
                                    <td class="name" title=e.path.clone()><span class="icon">{icon(k)}</span><Name pane=p entry=e.clone()/>
                                        {where_.map(|w| view! { <span class="where">{w}</span> })}</td>
                                    <td class="kindcol">{kind_label(&e)}</td>
                                    <td class="num">{when(e.mtime)}</td>
                                    <td class="num">{if e.dir { String::new() } else { human_size(e.size) }}</td>
                                </tr>
                            }
                        }
                    </For>
                </tbody>
            </table>
            {empty_note(p)}
        </div>
    }
}

/// Default widths of the list's kind, date and size columns.
const LIST_W: [f64; 3] = [110.0, 150.0, 90.0];

/// Whether `e` is being renamed (tracked).
fn renaming(p: Pane, e: &Entry) -> impl Fn() -> bool + Copy + 'static {
    let path = StoredValue::new(e.path.clone());
    move || p.renaming.with(|r| path.with_value(|x| r.as_ref() == Some(x)))
}

/// The file's icon, replaced by its thumbnail (an image, or an EPUB's cover) once that has loaded.
/// Files with no thumbnail, or whose thumbnail fails, keep the icon.
#[component]
pub(super) fn Thumbnail(src: Src, entry: Entry) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let state = RwSignal::new(0u8); // 0 loading, 1 loaded, 2 failed
    let glyph = icon(kind(&entry));
    let url = thumb_url(src, &entry);
    // Phone thumbnails can be turned off in the settings.
    let allowed = move || src == Src::Mac || ctx.phone_thumbs.get();
    view! {
        {move || (state.get() != 1 || !allowed()).then(|| view! { <span class="glyph">{glyph}</span> })}
        {move || url.clone().filter(|_| allowed()).map(|url| view! {
            // Kept in the layout (just invisible) while loading, so lazy loading still starts it.
            <img src=url loading="lazy" decoding="async" draggable="false" alt=""
                class:pending=move || state.get() != 1
                on:load=move |_| state.set(1) on:error=move |_| state.set(2)/>
        })}
    }
}

#[component]
pub(super) fn ThumbView(pane: Pane) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let p = pane;
    let (over, drop) = drop_events(ctx, p, None);
    let here = format!("{}:", p.id);
    view! {
        <div class="list" class:drop=move || ctx.drop_at.get().as_deref() == Some(here.as_str())
            on:click=move |_| p.select_only(None) on:contextmenu=move |ev| ctx.show_menu(p, &ev, None)
            on:dragover=over on:drop=drop>
            <div class="thumbs">
                <For each=move || p.entries.get() key=|e| (e.path.clone(), e.size, e.mtime) let(e)>
                    {
                        let path = e.path.clone();
                        let editing = renaming(p, &e);
                        let (click, dbl, menu) = row_events(ctx, p, &e);
                        let sub = if e.dir { "Folder".to_string() } else { human_size(e.size) };
                        let (dir, key) = (e.dir.then(|| e.path.clone()), format!("{}:{}", p.id, e.path));
                        let (over, drop) = drop_events(ctx, p, dir.clone());
                        let is_dir = e.dir;
                        view! {
                            <div class="tile" title=e.name.clone()
                                class:selected=move || p.is_picked(&path) class:drop=move || ctx.drop_at.get().as_deref() == Some(key.as_str())
                                draggable=move || (!editing()).to_string() data-dir=dir
                                on:dragstart=drag_start(ctx, p, e.path.clone()) on:dragend=drag_end(ctx)
                                on:dragover=move |ev| if is_dir { over(ev) } on:drop=move |ev| if is_dir { drop(ev) }
                                on:click=click on:dblclick=dbl on:contextmenu=menu>
                                <div class="pic"><Thumbnail src=p.src.get_untracked() entry=e.clone()/></div>
                                <div class="label"><Name pane=p entry=e.clone()/></div>
                                <div class="sub">{sub}</div>
                            </div>
                        }
                    }
                </For>
            </div>
            {empty_note(p)}
        </div>
    }
}

/// Finder-style columns: one per folder from "/" down to the current one, then a preview of the
/// selected item. Clicking a folder opens it; clicking a file in an earlier column goes back there.
#[component]
pub(super) fn ColumnView(pane: Pane) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let p = pane;
    let chain = Memo::new(move |_| chain_from(&p.root.get(), &p.cwd.get()));
    // Fetch the folders to the left that aren't known yet (e.g. when starting deep in this view).
    Effect::new(move |_| {
        let cwd = p.cwd.get();
        let src = p.src.get_untracked();
        for dir in chain.get() {
            if dir == cwd || p.cols.with_untracked(|c| c.contains_key(&dir)) {
                continue;
            }
            spawn_local(async move {
                let args = json!({ "src": src.key(), "path": dir, "current": false });
                if let Ok(es) = call::<Vec<Entry>>("list", args).await {
                    if p.src.get_untracked() == src {
                        let mut es = ctx.visible(es);
                        p.sort.get_untracked().apply(&mut es);
                        p.cols.update(|c| {
                            c.insert(dir, es);
                        });
                    }
                }
            });
        }
    });
    let strip = NodeRef::<html::Div>::new();
    // Show the newest column: scroll to the right end when the folder changes.
    Effect::new(move |_| {
        chain.track();
        request_animation_frame(move || {
            if let Some(el) = strip.get_untracked() {
                el.set_scroll_left(el.scroll_width());
            }
        });
    });
    view! {
        <div class="columns" node_ref=strip>
            // Keyed by side too, so switching between phone and Mac rebuilds the columns.
            <For each=move || { let s = p.src.get(); chain.get().into_iter().map(move |d| (s, d)).collect::<Vec<_>>() }
                key=|(s, d)| (s.key(), d.clone()) let((_s, dir))>
                {
                    // Each column has its own width; dragging one leaves the others.
                    let width = RwSignal::new(p.col_ws.with_value(|m| m.get(&dir).copied().unwrap_or(0.0)));
                    let d = dir.clone();
                    Effect::new(move |_| {
                        let w = width.get();
                        p.col_ws.update_value(|m| { m.insert(d.clone(), w); });
                    });
                    view! {
                        <Column pane=p dir chain width/>
                        <Grip size=width key=String::new() min=140.0 max=600.0 class="grip col-grip"/>
                    }
                }
            </For>
        </div>
    }
}

/// What's selected, to the right of the files: a bigger picture, the video or audio itself, and
/// details. Shown in every view; the button at the end of the status bar hides it.
#[component]
pub(super) fn Preview(pane: Pane) -> impl IntoView {
    let p = pane;
    // Only a different item rebuilds it, so a reload doesn't restart a playing video.
    let sel = Memo::new(move |_| p.selected_entry());
    view! {
        <div class="preview">
            {move || match sel.get() {
                None => view! { <span class="hint">{move || match p.marked.with(Vec::len) {
                    0 | 1 => "Pick an item to see it here.".to_string(),
                    n => format!("{n} items picked."),
                }}</span> }.into_any(),
                Some(e) => {
                    let k = kind(&e);
                    let what = match k {
                        Kind::Folder => "Folder", Kind::Epub => "EPUB book", Kind::Text => "Text file", Kind::Other => "File",
                    };
                    view! {
                        {
                            let src = p.src.get_untracked();
                            match media(&e) {
                                Some(Media::Image) => view! {
                                    <div class="pic"><img class="full" src=stream_url(src, &e) alt=""/></div>
                                }.into_any(),
                                Some(Media::Video) => view! { <Player url=stream_url(src, &e) video=true/> }.into_any(),
                                Some(Media::Audio) => view! {
                                    <div class="pic"><Thumbnail src=src entry=e.clone()/></div>
                                    <Player url=stream_url(src, &e) video=false/>
                                }.into_any(),
                                None if is_text(&e) => view! { <TextPeek src=src entry=e.clone()/> }.into_any(),
                            None => view! {
                                    <div class="pic"><Thumbnail src=src entry=e.clone()/></div>
                                }.into_any(),
                            }
                        }
                        <b class="name">{e.name.clone()}</b>
                        <span class="sub">{what}{(!e.dir).then(|| format!(" · {}", human_size(e.size)))}</span>
                        <dl>
                            <dt>"Modified"</dt><dd>{when(e.mtime)}</dd>
                            <dt>"Where"</dt><dd class="path">{parent(&e.path)}</dd>
                        </dl>
                        <span class="hint">{if e.dir { "Double-click to open" } else { "Double-click to open · right-click for more" }}</span>
                    }.into_any()
                }
            }}
        </div>
    }
}

/// Text files the preview shows the start of: any size, as only the start is read.
fn is_text(e: &Entry) -> bool {
    !e.dir && TEXT_EXTS.contains(&ext(&e.name).as_str())
}

/// A video or audio player streaming from the device. When the preview moves on, it stops and
/// lets go of the file, so the web view drops what it buffered.
#[component]
fn Player(url: String, video: bool) -> impl IntoView {
    let el = StoredValue::new_local(None::<web_sys::HtmlMediaElement>);
    let keep = move |e: &web_sys::HtmlMediaElement| el.set_value(Some(e.clone()));
    on_cleanup(move || {
        if let Some(m) = el.try_update_value(Option::take).flatten() {
            let _ = m.pause();
            let _ = m.remove_attribute("src");
            m.load();
        }
    });
    let video_ref = NodeRef::<html::Video>::new();
    let audio_ref = NodeRef::<html::Audio>::new();
    video_ref.on_load(move |v| keep(&v));
    audio_ref.on_load(move |a| keep(&a));
    if video {
        view! { <video class="play" controls preload="metadata" src=url node_ref=video_ref></video> }.into_any()
    } else {
        view! { <audio class="play" controls preload="metadata" src=url node_ref=audio_ref></audio> }.into_any()
    }
}

/// How much of a text file the preview loads, at most, as it's scrolled.
const PEEK_CAP: u64 = 8 * 1024 * 1024;

/// A text file, a piece at a time: the next piece loads when the end is scrolled near.
#[component]
fn TextPeek(src: Src, entry: Entry) -> impl IntoView {
    let pieces = RwSignal::new(Vec::<String>::new());
    // Where the next piece starts; None once the whole file is in.
    let next = RwSignal::new(None::<u64>);
    let error = RwSignal::new(None::<String>);
    let loading = RwSignal::new(false);
    let path = StoredValue::new(entry.path);
    let fetch = move |start: u64| {
        if loading.get_untracked() {
            return;
        }
        loading.set(true);
        spawn_local(async move {
            let path = path.try_get_value().unwrap_or_default();
            let r = call::<Peek>("peek_text", json!({ "src": src.key(), "path": path, "start": start })).await;
            // The preview may show something else by now: then these are gone and nothing is set.
            match r {
                Ok(p) => {
                    if pieces.try_update(|v| v.push(p.text)).is_some() {
                        next.set(p.next);
                    }
                }
                Err(e) => {
                    let _ = error.try_set(Some(e));
                }
            }
            let _ = loading.try_set(false);
        });
    };
    fetch(0);
    let capped = move || next.get().is_some_and(|n| n >= PEEK_CAP);
    let scrolled = move |ev: web_sys::Event| {
        let Some(pre) = ev.target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) else { return };
        let left = pre.scroll_height() - pre.scroll_top() - pre.client_height();
        match next.get_untracked() {
            Some(n) if n < PEEK_CAP && left < 600 && error.get_untracked().is_none() => fetch(n),
            _ => {}
        }
    };
    let empty = move || pieces.with(|v| v.len() == 1 && v[0].trim().is_empty()) && next.get().is_none();
    view! {
        <div class="peek">
            {move || if pieces.with(Vec::is_empty) {
                match error.get() {
                    Some(e) => view! { <span class="hint">{e}</span> }.into_any(),
                    None => view! { <span class="spin"></span> }.into_any(),
                }
            } else if empty() {
                view! { <span class="hint">"Empty"</span> }.into_any()
            } else {
                view! {
                    <pre on:scroll=scrolled>
                        <For each=move || 0..pieces.with(Vec::len) key=|i| *i
                            children=move |i| pieces.with_untracked(|v| v[i].clone())/>
                    </pre>
                    {move || if let Some(e) = error.get() {
                        view! { <span class="more">{e}</span> }.into_any()
                    } else if capped() {
                        view! { <span class="more">"Only the first 8 MB is shown. Double-click to open it all."</span> }.into_any()
                    } else if loading.get() {
                        view! { <span class="more">"Loading more…"</span> }.into_any()
                    } else {
                        ().into_any()
                    }}
                }.into_any()
            }}
        </div>
    }
}

#[component]
pub(super) fn Column(pane: Pane, dir: String, chain: Memo<Vec<String>>, width: RwSignal<f64>) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let p = pane;
    let d = dir.clone();
    let current = Memo::new(move |_| p.cwd.get() == d);
    let d = dir.clone();
    // The folder open to the right of this column, if any.
    let open_child = Memo::new(move |_| chain.with(|c| c.iter().position(|x| *x == d).and_then(|i| c.get(i + 1).cloned())));
    let d = dir.clone();
    let entries = move || if current.get() { Some(p.entries.get()) } else { p.cols.with(|c| c.get(&d).cloned()) };
    let d = dir.clone();
    let bg_menu = move |ev: web_sys::MouseEvent| {
        if !current.get_untracked() {
            ctx.navigate(p, d.clone());
        }
        ctx.show_menu(p, &ev, None);
    };
    let (over, drop) = drop_events(ctx, p, Some(dir.clone()));
    let key = format!("{}:{}", p.id, dir);
    let data_dir = dir.clone();
    view! {
        <div class="col" class:current=current on:contextmenu=bg_menu data-dir=data_dir
            style=move || match width.get() { w if w > 0.0 => format!("flex-basis: {w}px"), _ => String::new() }
            class:drop=move || ctx.drop_at.get().as_deref() == Some(key.as_str())
            on:dragover=over on:drop=drop>
            {move || match entries() {
                None => view! { <div class="empty"><span class="spin"></span></div> }.into_any(),
                Some(es) if es.is_empty() => view! { <div class="empty">"Empty"</div> }.into_any(),
                Some(es) => es.into_iter().map(|e| {
                    let (p1, p2) = (e.path.clone(), e.path.clone());
                    let (e1, e2) = (e.clone(), e.clone());
                    let dir = dir.clone();
                    let click = move |ev: web_sys::MouseEvent| {
                        if current.get_untracked() && (ev.meta_key() || ev.shift_key()) {
                            p.click(&ev, &e1.path);
                        } else if e1.dir {
                            ctx.navigate(p, e1.path.clone());
                        } else if current.get_untracked() {
                            p.select_only(Some(e1.path.clone()));
                        } else {
                            ctx.navigate_select(p, p.src.get_untracked(), dir.clone(), Some(e1.path.clone()));
                        }
                    };
                    let k = kind(&e);
                    let (fdir, key) = (e.dir.then(|| e.path.clone()), format!("{}:{}", p.id, e.path));
                    let (over, drop) = drop_events(ctx, p, fdir.clone());
                    let is_dir = e.dir;
                    let editing = renaming(p, &e);
                    view! {
                        <div class="row"
                            class:open=move || open_child.get().as_deref() == Some(p1.as_str())
                            class:selected=move || current.get() && p.is_picked(&p2)
                            class:drop=move || ctx.drop_at.get().as_deref() == Some(key.as_str())
                            draggable=move || (!editing()).to_string() data-dir=fdir
                            on:dragstart=drag_start(ctx, p, e.path.clone()) on:dragend=drag_end(ctx)
                            on:dragover=move |ev| if is_dir { over(ev) } on:drop=move |ev| if is_dir { drop(ev) }
                            on:click=click
                            on:dblclick=move |_| if !e2.dir { ctx.open(p, e2.clone()) }
                            on:contextmenu=move |ev| ctx.show_menu(p, &ev, Some(e.clone()))>
                            <span class="icon">{icon(k)}</span>
                            <span class="name"><Name pane=p entry=e.clone()/></span>
                            {e.dir.then(|| view! { <span class="chev">"›"</span> })}
                        </div>
                    }
                }).collect_view().into_any(),
            }}
        </div>
    }
}
