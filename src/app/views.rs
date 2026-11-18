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
                <thead><tr>
                    {SortKey::ALL.map(|k| {
                        let num = matches!(k, SortKey::Date | SortKey::Size);
                        view! {
                            <th class:num=num><button class="sort" title=format!("Sort by {}", k.label().to_lowercase())
                                on:click=move |_| p.set_sort(k)>
                                {k.label()}
                                <span class="arrow">{move || match p.sort.get() {
                                    Sort(s, asc) if s == k => if asc { "▲" } else { "▼" },
                                    _ => "",
                                }}</span>
                            </button></th>
                        }
                    })}
                </tr></thead>
                <tbody>
                    <For each=move || p.entries.get() key=|e| (e.path.clone(), e.size, e.mtime) let(e)>
                        {
                            let k = kind(&e);
                            let path = e.path.clone();
                            let (click, dbl, menu) = row_events(ctx, p, &e);
                            let where_ = p.found.get_untracked().is_some().then(|| parent(&e.path));
                            let (dir, key) = (e.dir.then(|| e.path.clone()), format!("{}:{}", p.id, e.path));
                            let (over, drop) = drop_events(ctx, p, dir.clone());
                            view! {
                                <tr class:selected=move || p.is_picked(&path) class:drop=move || ctx.drop_at.get().as_deref() == Some(key.as_str())
                                    draggable="true" data-dir=dir
                                    on:dragstart=drag_start(ctx, p, e.path.clone()) on:dragend=drag_end(ctx)
                                    on:dragover=move |ev| if e.dir { over(ev) } on:drop=move |ev| if e.dir { drop(ev) }
                                    on:click=click on:dblclick=dbl on:contextmenu=menu>
                                    <td class="name" title=e.path.clone()><span class="icon">{icon(k)}</span>{e.name.clone()}
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
                        let (click, dbl, menu) = row_events(ctx, p, &e);
                        let sub = if e.dir { "Folder".to_string() } else { human_size(e.size) };
                        let (dir, key) = (e.dir.then(|| e.path.clone()), format!("{}:{}", p.id, e.path));
                        let (over, drop) = drop_events(ctx, p, dir.clone());
                        let is_dir = e.dir;
                        view! {
                            <div class="tile" title=e.name.clone()
                                class:selected=move || p.is_picked(&path) class:drop=move || ctx.drop_at.get().as_deref() == Some(key.as_str())
                                draggable="true" data-dir=dir
                                on:dragstart=drag_start(ctx, p, e.path.clone()) on:dragend=drag_end(ctx)
                                on:dragover=move |ev| if is_dir { over(ev) } on:drop=move |ev| if is_dir { drop(ev) }
                                on:click=click on:dblclick=dbl on:contextmenu=menu>
                                <div class="pic"><Thumbnail src=p.src.get_untracked() entry=e.clone()/></div>
                                <div class="label">{e.name.clone()}</div>
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
                <Column pane=p dir chain/>
            </For>
            {move || p.selected_entry().map(|e| {
                let k = kind(&e);
                let what = match k {
                    Kind::Folder => "Folder", Kind::Epub => "EPUB book", Kind::Text => "Text file", Kind::Other => "File",
                };
                view! {
                    <div class="preview">
                        {
                            let src = p.src.get_untracked();
                            match media(&e) {
                                Some(Media::Image) => view! {
                                    <div class="pic"><img class="full" src=stream_url(src, &e) alt=""/></div>
                                }.into_any(),
                                Some(Media::Video) => view! {
                                    <video class="play" controls preload="metadata" src=stream_url(src, &e)></video>
                                }.into_any(),
                                Some(Media::Audio) => view! {
                                    <div class="pic"><Thumbnail src=src entry=e.clone()/></div>
                                    <audio class="play" controls preload="metadata" src=stream_url(src, &e)></audio>
                                }.into_any(),
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
                        <span class="hint">{if e.dir { "Double-click or → to open" } else { "Double-click to open · right-click for more" }}</span>
                    </div>
                }
            })}
        </div>
    }
}

#[component]
pub(super) fn Column(pane: Pane, dir: String, chain: Memo<Vec<String>>) -> impl IntoView {
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
                    view! {
                        <div class="row"
                            class:open=move || open_child.get().as_deref() == Some(p1.as_str())
                            class:selected=move || current.get() && p.is_picked(&p2)
                            class:drop=move || ctx.drop_at.get().as_deref() == Some(key.as_str())
                            draggable="true" data-dir=fdir
                            on:dragstart=drag_start(ctx, p, e.path.clone()) on:dragend=drag_end(ctx)
                            on:dragover=move |ev| if is_dir { over(ev) } on:drop=move |ev| if is_dir { drop(ev) }
                            on:click=click
                            on:dblclick=move |_| if !e2.dir { ctx.open(p, e2.clone()) }
                            on:contextmenu=move |ev| ctx.show_menu(p, &ev, Some(e.clone()))>
                            <span class="icon">{icon(k)}</span>
                            <span class="name">{e.name.clone()}</span>
                            {e.dir.then(|| view! { <span class="chev">"›"</span> })}
                        </div>
                    }
                }).collect_view().into_any(),
            }}
        </div>
    }
}
