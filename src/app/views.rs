//! The ways a pane shows a folder: list, thumbnails and columns.

use super::*;

#[component]
pub(super) fn ListView(pane: Pane) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let p = pane;
    let (over, drop) = drop_events(ctx, p, None);
    let here = format!("{}:", p.id);
    // Dragging a column header to another place: the column, and where it would go (a position
    // in `list_cols`, 0 being right after the name).
    let moving = RwSignal::new(None::<(usize, usize)>);
    // Set once a drag has moved, so the click that ends it doesn't also sort.
    let dragged = StoredValue::new(false);
    let head = NodeRef::<html::Tr>::new();
    let listeners = StoredValue::new_local(None::<(WindowListenerHandle, WindowListenerHandle)>);
    let stop = move || {
        if let Some((a, b)) = listeners.try_update_value(Option::take).flatten() {
            a.remove();
            b.remove();
        }
    };
    on_cleanup(stop);
    let start_move = move |ev: web_sys::MouseEvent, col: usize| {
        if ev.button() != 0 {
            return;
        }
        let x0 = ev.client_x();
        dragged.set_value(false);
        let moved = window_event_listener(ev::mousemove, move |e| {
            if !dragged.get_value() && (e.client_x() - x0).abs() < 5 {
                return;
            }
            dragged.set_value(true);
            // Before the first header (after the name) whose middle is right of the pointer.
            let Some(tr) = head.get_untracked() else { return };
            let ths = tr.query_selector_all("th[data-col]").ok();
            let mut at = 0;
            if let Some(ths) = ths {
                for n in 0..ths.length() {
                    let Some(th) = ths.item(n).and_then(|t| t.dyn_into::<web_sys::Element>().ok()) else { continue };
                    let r = th.get_bounding_client_rect();
                    if (e.client_x() as f64) > r.left() + r.width() / 2.0 {
                        at = n as usize + 1;
                    }
                }
            }
            moving.set(Some((col, at)));
        });
        let up = window_event_listener(ev::mouseup, move |_| {
            if let Some((col, at)) = moving.get_untracked() {
                let mut v = p.list_cols.get_untracked();
                if let Some(from) = v.iter().position(|&c| c == col) {
                    v.remove(from);
                    v.insert(if at > from { at - 1 } else { at }.min(v.len()), col);
                    p.set_list_cols(v);
                }
            }
            moving.set(None);
            stop();
        });
        stop();
        listeners.set_value(Some((moved, up)));
    };
    // The drop mark: on the left of the header at that position, or the right of the last.
    let mark = move |pos: usize, last: bool| {
        moving.with(|m| match *m {
            Some((_, at)) if at == pos => "drop-before",
            Some((_, at)) if last && at == pos + 1 => "drop-after",
            _ => "",
        })
    };
    view! {
        <div class="list" class:drop=move || ctx.drop_at.get().as_deref() == Some(here.as_str())
            on:click=move |_| p.select_only(None) on:contextmenu=move |ev| ctx.show_menu(p, &ev, None)
            on:dragover=over on:drop=drop>
            <table class:moving=move || moving.with(Option::is_some)>
                // The name takes what's left; the others have the widths dragged on their left edge.
                <colgroup>
                    <col/>
                    {move || p.list_cols.get().into_iter().map(|c| view! {
                        <col style=move || format!("width: {}px", match p.list_w[c].get() { w if w > 0.0 => w, _ => LIST_W[c] })/>
                    }).collect_view()}
                </colgroup>
                <thead on:contextmenu=move |ev| ctx.show_columns_menu(p, &ev)><tr node_ref=head>
                    <th>{sort_button(p, SortKey::Name, dragged)}</th>
                    {move || {
                        let cols = p.list_cols.get();
                        let n = cols.len();
                        cols.into_iter().enumerate().map(|(pos, c)| {
                            let k = LIST_COLS[c];
                            let num = matches!(k, SortKey::Date | SortKey::Size);
                            view! {
                                <th class:num=num data-col=c.to_string() class=move || mark(pos, pos + 1 == n)
                                    class:lifted=move || moving.with(|m| m.is_some_and(|(m, _)| m == c))
                                    title="Drag to move this column; right-click to choose columns"
                                    on:mousedown=move |ev| start_move(ev, c)>
                                    <Grip size=p.list_w[c] key=format!("yomi-files.list-w.{}.{}", p.id, c)
                                        min=50.0 max=400.0 target=Target::ParentLeft class="grip col-grip"/>
                                    {sort_button(p, k, dragged)}
                                </th>
                            }
                        }).collect_view()
                    }}
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
                            let cells = [kind_label(&e), when(e.mtime), if e.dir { String::new() } else { human_size(e.size) }];
                            view! {
                                <tr class:selected=move || p.is_picked(&path) class:drop=move || ctx.drop_at.get().as_deref() == Some(key.as_str())
                                    draggable=move || (!editing()).to_string() data-dir=dir
                                    on:dragstart=drag_start(ctx, p, e.path.clone()) on:dragend=drag_end(ctx)
                                    on:dragover=move |ev| if e.dir { over(ev) } on:drop=move |ev| if e.dir { drop(ev) }
                                    on:click=click on:dblclick=dbl on:contextmenu=menu>
                                    <td class="name" title=e.path.clone()><span class="icon">{icon(k)}</span><Name pane=p entry=e.clone()/>
                                        {where_.map(|w| view! { <span class="where">{w}</span> })}</td>
                                    {move || p.list_cols.get().into_iter().map(|c| {
                                        view! { <td class=if c == 0 { "kindcol" } else { "num" }>{cells[c].clone()}</td> }
                                    }).collect_view()}
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

/// A column header's button: sorts by it, or turns the order around when it's already sorted by
/// it. Not when it was just dragged.
fn sort_button(p: Pane, k: SortKey, dragged: StoredValue<bool>) -> impl IntoView {
    view! {
        <button class="sort" title=format!("Sort by {}", k.label().to_lowercase())
            on:click=move |_| if !dragged.get_value() { p.set_sort(k) }>
            {k.label()}
            <span class="arrow">{move || match p.sort.get() {
                Sort(s, asc) if s == k => if asc { "▲" } else { "▼" },
                _ => "",
            }}</span>
        </button>
    }
}

/// The list's columns after the name, by index, and their default widths.
pub(super) const LIST_COLS: [SortKey; 3] = [SortKey::Kind, SortKey::Date, SortKey::Size];
const LIST_W: [f64; 3] = [110.0, 150.0, 90.0];

/// The list's columns as last arranged for pane `id`: all of them, in order, at first.
pub(super) fn stored_cols(id: usize) -> Vec<usize> {
    let mut v: Vec<usize> = Vec::new();
    match stored(&format!("yomi-files.list-cols.{id}")) {
        Some(s) => {
            for c in s.split(',').filter_map(|c| c.trim().parse::<usize>().ok()) {
                if c < LIST_COLS.len() && !v.contains(&c) {
                    v.push(c);
                }
            }
        }
        None => v = (0..LIST_COLS.len()).collect(),
    }
    v
}

impl Pane {
    pub(super) fn set_list_cols(self, v: Vec<usize>) {
        store(&format!("yomi-files.list-cols.{}", self.id), &v.iter().map(usize::to_string).collect::<Vec<_>>().join(","));
        self.list_cols.set(v);
    }
}

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
