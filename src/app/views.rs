//! The ways a pane shows a folder: list, thumbnails and columns.

use super::*;
use crate::api::{Listing, Packed};

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
    // Only the rows in view (and a little either side) are rendered.
    let scroller = NodeRef::<html::Div>::new();
    // As a tree: the rows with open folders' items under them, and their depth.
    let tree = Memo::new(move |_| p.tree_on().then(|| p.tree_rows()));
    let len = Signal::derive(move || tree.with(|t| t.as_ref().map(Vec::len)).unwrap_or_else(|| p.entries.with(Vec::len)));
    let win = rows::Window::new(scroller, len, rows::Rows { h: 29.0, per: 1, top: 30.0 }, rows::list_rows, Signal::derive(move || Some(p.id)));
    let gap = move |h: f64| (h > 0.0).then(|| view! {
        <tr class="gap" aria-hidden="true"><td colspan=move || (1 + p.list_cols.with(Vec::len)).to_string() style=format!("height: {h}px")></td></tr>
    });
    view! {
        <div class="list" node_ref=scroller data-rows="" class:tree=move || tree.with(Option::is_some) class:drop=move || ctx.drop_at.get().as_deref() == Some(here.as_str())
            on:click=move |_| p.select_only(None) on:contextmenu=move |ev| ctx.show_menu(p, &ev, None)
            on:scroll=move |_| { win.update(); more_near_end(ctx, p, scroller) } on:dragover=over on:drop=drop>
            // At least the other columns and NAME_MIN for the name: narrower, the fixed layout would
            // give the name nothing. The list scrolls sideways instead.
            <table class:moving=move || moving.with(Option::is_some)
                style=move || format!("min-width: {}px", NAME_MIN + p.list_cols.get().into_iter().map(|c| col_w(p, c)).sum::<f64>())>
                // The name takes what's left; the others have the widths dragged on their left edge.
                <colgroup>
                    <col/>
                    {move || p.list_cols.get().into_iter().map(|c| view! {
                        <col style=move || format!("width: {}px", col_w(p, c))/>
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
                                    <Grip size=p.list_w[c] key=format!("ferry-files.list-w.{}.{}", p.id, c)
                                        min=50.0 max=400.0 target=Target::ParentLeft class="grip col-grip"/>
                                    {sort_button(p, k, dragged)}
                                </th>
                            }
                        }).collect_view()
                    }}
                </tr></thead>
                <tbody>
                    {move || gap(win.gaps().0)}
                    <For each=move || tree.with(|t| match t {
                            Some(rows) => win.slice(rows),
                            None => p.entries.with(|es| win.slice(es)).into_iter().map(|e| (e, 0)).collect(),
                        }) key=|(e, _)| (e.path.clone(), e.size, e.mtime) let((e, depth))>
                        {
                            let k = kind(&e);
                            // In the tree: a folder's arrow (or room for one), indented by depth.
                            let twisty = {
                                let path = StoredValue::new(e.path.clone());
                                let is_dir = e.dir;
                                move || tree.with(Option::is_some).then(|| if is_dir {
                                    let open = Signal::derive(move || path.with_value(|x| p.is_open(x)));
                                    view! {
                                        <button class="twisty" tabindex="-1" title=move || if open.get() { "Close" } else { "Open here" }
                                            aria-label=move || if open.get() { "Close" } else { "Open here" }
                                            on:click=move |ev| { ev.stop_propagation(); ctx.toggle_kids(p, path.get_value()) }
                                            on:dblclick=|ev| ev.stop_propagation()>
                                            {chev(open)}
                                        </button>
                                    }.into_any()
                                } else {
                                    view! { <span class="twisty"></span> }.into_any()
                                })
                            };
                            let path = e.path.clone();
                            let editing = renaming(p, &e);
                            let (click, dbl, menu) = row_events(ctx, p, &e);
                            let where_ = p.found.get_untracked().is_some().then(|| parent(&e.path));
                            let (dir, key) = (e.dir.then(|| e.path.clone()), format!("{}:{}", p.id, e.path));
                            let (over, drop) = drop_events(ctx, p, dir.clone());
                            let cells = [kind_label(&e), when(e.mtime), if e.dir { String::new() } else { human_size(e.size) }];
                            view! {
                                <tr data-path=e.path.clone() data-cmp={let e = e.clone(); move || cmp_mark(p, &e)} class:selected=move || p.is_picked(&path) class:drop=move || ctx.drop_at.get().as_deref() == Some(key.as_str())
                                    draggable=move || (!editing()).to_string() data-dir=dir
                                    on:dragstart=drag_start(ctx, p, e.path.clone()) on:dragend=drag_end(ctx)
                                    on:dragover=move |ev| if e.dir { over(ev) } on:drop=move |ev| if e.dir { drop(ev) }
                                    on:click=click on:dblclick=dbl on:contextmenu=menu>
                                    <td class="name" title=e.path.clone() style=(depth > 0).then(|| format!("padding-left: {}px", 12 + depth * 16))>{twisty}<span class="icon">{icon(k)}</span><Name pane=p entry=e.clone() split=true/>
                                        {where_.map(|w| view! { <span class="where">{w}</span> })}</td>
                                    {move || p.list_cols.get().into_iter().map(|c| {
                                        view! { <td class=if c == 0 { "kindcol" } else { "num" }>{cells[c].clone()}</td> }
                                    }).collect_view()}
                                </tr>
                            }
                        }
                    </For>
                    {move || gap(win.gaps().1)}
                </tbody>
            </table>
            {more_row(ctx, p)}
            {empty_note(p)}
        </div>
    }
}

/// A column header's button: sorts by it, or turns the order around when it's already sorted by
/// it. Not when it was just dragged.
fn sort_button(p: Pane, k: SortKey, dragged: StoredValue<bool>) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    view! {
        <button class="sort" title=format!("Sort by {}", k.label().to_lowercase())
            on:click=move |_| if !dragged.get_value() {
                p.set_sort(k);
                // Only part of the folder is here: the server sorts it all and sends the first page again.
                if p.more.get_untracked() {
                    ctx.reload(p);
                }
            }>
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
/// Their order before any is moved: size, kind, modified.
pub(super) const LIST_ORDER: [usize; 3] = [2, 0, 1];
/// The narrowest the name column gets; a narrower list scrolls sideways.
const NAME_MIN: f64 = 160.0;

/// Column `c`'s width: as dragged, or its default.
fn col_w(p: Pane, c: usize) -> f64 {
    match p.list_w[c].get() {
        w if w > 0.0 => w,
        _ => LIST_W[c],
    }
}

/// The list's columns as last arranged for pane `id`: all of them, in `LIST_ORDER`, at first.
pub(super) fn stored_cols(id: usize) -> Vec<usize> {
    let mut v: Vec<usize> = Vec::new();
    match stored(&format!("ferry-files.list-cols.{id}")) {
        Some(s) => {
            for c in s.split(',').filter_map(|c| c.trim().parse::<usize>().ok()) {
                if c < LIST_COLS.len() && !v.contains(&c) {
                    v.push(c);
                }
            }
        }
        None => v = LIST_ORDER.to_vec(),
    }
    v
}

impl Pane {
    pub(super) fn set_list_cols(self, v: Vec<usize>) {
        store(&format!("ferry-files.list-cols.{}", self.id), &v.iter().map(usize::to_string).collect::<Vec<_>>().join(","));
        self.list_cols.set(v);
    }
}

/// Whether `e` is being renamed (tracked).
fn renaming(p: Pane, e: &Entry) -> impl Fn() -> bool + Copy + 'static {
    let path = StoredValue::new(e.path.clone());
    move || p.renaming.with(|r| path.with_value(|x| r.as_ref() == Some(x)))
}

/// How long a tile stays in view before its thumbnail is asked for.
const THUMB_DELAY: std::time::Duration = std::time::Duration::from_millis(150);

/// The file's icon, replaced by its thumbnail (an image, or an EPUB's cover) once that has loaded.
/// Files with no thumbnail, or whose thumbnail fails, keep the icon.
///
/// The picture is only there while the tile is in view: scrolled away, it's let go (and the icon
/// shows again), so a folder of thousands of photos doesn't keep every one it has shown in memory.
/// It's asked for once the tile has stayed in view a moment: scrolling fast past thousands of
/// photos would otherwise queue a request for each.
#[component]
pub(super) fn Thumbnail(src: Src, entry: Entry) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let state = RwSignal::new(0u8); // 0 loading, 1 loaded, 2 failed
    let shown = RwSignal::new(false);
    let glyph = icon(kind(&entry));
    let url = thumb_url(src, &entry);
    // Phone thumbnails can be turned off in the settings.
    let allowed = move || src == Src::Mac || ctx.phone_thumbs.get();
    let img = NodeRef::<html::Img>::new();
    let watching = StoredValue::new_local(None::<(web_sys::Element, u32)>);
    let soon = StoredValue::new_local(None::<TimeoutHandle>);
    let wait = move || {
        if let Some(h) = soon.try_update_value(Option::take).flatten() {
            h.clear();
        }
    };
    let stop = move || {
        wait();
        if let Some((el, id)) = watching.try_update_value(Option::take).flatten() {
            near::unwatch(&el, id);
        }
    };
    // Again when the picture comes back after phone thumbnails were turned off and on.
    img.on_load(move |el| {
        stop();
        let el: web_sys::Element = el.into();
        let id = near::watch(&el, move |on| {
            wait();
            if on {
                if let Ok(h) = set_timeout_with_handle(move || { let _ = shown.try_set(true); }, THUMB_DELAY) {
                    soon.set_value(Some(h));
                }
                return;
            }
            shown.set(false);
            if state.get_untracked() == 1 {
                state.set(0);
            }
        });
        watching.set_value(Some((el, id)));
    });
    on_cleanup(stop);
    view! {
        {move || (state.get() != 1 || !allowed()).then(|| view! { <span class="glyph">{glyph}</span> })}
        {move || url.clone().filter(|_| allowed()).map(|url| view! {
            // Kept in the layout (just invisible) while loading, so it's seen coming into view.
            <img node_ref=img src=move || shown.get().then(|| url.clone()) decoding="async" draggable="false" alt=""
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
    // Only the rows of tiles in view (and a little either side) are rendered.
    let scroller = NodeRef::<html::Div>::new();
    let len = Signal::derive(move || p.entries.with(Vec::len));
    let win = rows::Window::new(scroller, len, rows::Rows { h: 160.0, per: 4, top: 12.0 }, rows::tile_rows, Signal::derive(move || Some(p.id)));
    view! {
        <div class="list" node_ref=scroller data-rows="" class:drop=move || ctx.drop_at.get().as_deref() == Some(here.as_str())
            on:click=move |_| p.select_only(None) on:contextmenu=move |ev| ctx.show_menu(p, &ev, None)
            on:scroll=move |_| { win.update(); more_near_end(ctx, p, scroller) } on:dragover=over on:drop=drop>
            <div class="gap" style=move || format!("height: {}px", win.gaps().0)></div>
            <div class="thumbs">
                <For each=move || p.entries.with(|es| win.slice(es)) key=|e| (e.path.clone(), e.size, e.mtime) let(e)>
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
                                data-path=e.path.clone() data-cmp={let e = e.clone(); move || cmp_mark(p, &e)} class:selected=move || p.is_picked(&path) class:drop=move || ctx.drop_at.get().as_deref() == Some(key.as_str())
                                draggable=move || (!editing()).to_string() data-dir=dir
                                on:dragstart=drag_start(ctx, p, e.path.clone()) on:dragend=drag_end(ctx)
                                on:dragover=move |ev| if is_dir { over(ev) } on:drop=move |ev| if is_dir { drop(ev) }
                                on:click=click on:dblclick=dbl on:contextmenu=menu>
                                <div class="pic"><Thumbnail src=p.src.get_untracked() entry=e.clone()/></div>
                                <div class="label"><Name pane=p entry=e.clone() fit=true/></div>
                                <div class="sub">{sub}</div>
                            </div>
                        }
                    }
                </For>
            </div>
            <div class="gap" style=move || format!("height: {}px", win.gaps().1)></div>
            {more_row(ctx, p)}
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
                if let Ok((es, _)) = ctx.list_first(src, &dir, p.sort.get_untracked(), json!({ "current": false })).await {
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
    // A ZIP file picked in the last column: what's in it, as more columns.
    // Also any file the menu's Open as ZIP in columns (or the preview's Show as ZIP) was used on.
    let zip = Memo::new(move |_| {
        p.selected_entry().filter(|e| {
            super::preview::has_ext(e, super::preview::ZIP_EXTS)
                || p.peek_as.with(|f| matches!(f, Some((path, PeekAs::Zip)) if *path == e.path))
        })
    });
    let zip_open = RwSignal::new(Vec::<String>::new());
    // Show the newest column: scroll to the right end when the folder changes.
    Effect::new(move |_| {
        chain.track();
        zip.track();
        zip_open.track();
        request_animation_frame(move || {
            if let Some(el) = strip.try_get_untracked().flatten() {
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
            {move || zip.get().map(|e| view! { <ZipColumns pane=p src=p.src.get_untracked() entry=e open=zip_open/> })}
        </div>
    }
}

/// An item in a folder of a ZIP file: its full name in the ZIP (folders end in /), its name, and
/// its unpacked size, and whether it's stored without compression.
#[derive(Clone)]
struct ZipItem {
    path: String,
    name: String,
    dir: bool,
    size: u64,
    stored: bool,
}

/// What's in folder `prefix` ("" or "a/b/") of a ZIP: folders first, then by name. Folders are
/// also made up from the files' names, since a ZIP needn't list them.
fn zip_items(items: &[Packed], prefix: &str) -> Vec<ZipItem> {
    let mut out: Vec<ZipItem> = Vec::new();
    for i in items {
        let Some(rest) = i.name.strip_prefix(prefix).filter(|r| !r.is_empty()) else { continue };
        let item = match rest.find('/') {
            Some(at) => ZipItem { path: format!("{prefix}{}/", &rest[..at]), name: rest[..at].to_string(), dir: true, size: 0, stored: false },
            None => ZipItem { path: i.name.clone(), name: rest.to_string(), dir: false, size: i.size, stored: i.stored },
        };
        if !out.iter().any(|o| o.path == item.path) {
            out.push(item);
        }
    }
    out.sort_by_key(|i| (!i.dir, i.name.to_lowercase()));
    out
}

/// A ZIP's folders as columns after the column view's own: the top, then each folder opened.
/// `open` is the folders opened, deepest last; clicking an item picks it (and opens it, when it's
/// a folder, or shows it in the preview, when it's a file).
#[component]
fn ZipColumns(pane: Pane, src: Src, entry: Entry, open: RwSignal<Vec<String>>) -> impl IntoView {
    let got = RwSignal::new(None::<Result<Listing, String>>);
    let picked = RwSignal::new(None::<String>);
    open.set(vec![]);
    pane.zip_pick.set(None);
    let zip_path = StoredValue::new(entry.path.clone());
    let args = json!({ "src": src.key(), "path": entry.path, "size": entry.size });
    spawn_local(async move {
        let r = call::<Listing>("zip_list", args).await;
        let _ = got.try_set(Some(r));
    });
    move || match got.get() {
        None => view! { <div class="col zip-col"><div class="empty"><span class="spin"></span></div></div> }.into_any(),
        Some(Err(e)) => view! { <div class="col zip-col"><div class="empty">{e}</div></div> }.into_any(),
        Some(Ok(l)) => {
            let l = StoredValue::new(l);
            let short = l.with_value(|l| l.total > l.items.len() as u64);
            let levels = move || std::iter::once(String::new()).chain(open.get()).enumerate().collect::<Vec<_>>();
            view! {
                <For each=levels key=|(n, prefix)| (*n, prefix.clone()) let((n, prefix))>
                    {
                        let width = RwSignal::new(0.0);
                        let items = l.with_value(|l| zip_items(&l.items, &prefix));
                        view! {
                            <div class="col zip-col" style=move || match width.get() { w if w > 0.0 => format!("flex-basis: {w}px"), _ => String::new() }>
                                {items.is_empty().then(|| view! { <div class="empty">"Empty"</div> })}
                                {items.into_iter().map(|i| {
                                    let (a, b, c) = (i.path.clone(), i.path.clone(), i.path.clone());
                                    let tip = if i.dir { i.path.clone() } else { format!("{} · {}", i.path, human_size(i.size)) };
                                    view! {
                                        <div class="row" title=tip
                                            class:open=move || i.dir && open.with(|o| o.get(n) == Some(&a))
                                            class:selected=move || picked.with(|p| p.as_deref() == Some(b.as_str()))
                                            on:click=move |ev| {
                                                ev.stop_propagation();
                                                picked.set(Some(c.clone()));
                                                // A file shows in the preview; a folder leaves the ZIP there.
                                                pane.zip_pick.set((!i.dir).then(|| (src, zip_path.get_value(), c.clone(), i.size, i.stored)));
                                                open.update(|o| {
                                                    o.truncate(n);
                                                    if i.dir {
                                                        o.push(c.clone());
                                                    }
                                                });
                                            }>
                                            <span class="icon">{icon(if i.dir { Kind::Folder } else { Kind::Other })}</span>
                                            <span class="name">{split_name(&i.name, i.dir)}</span>
                                            {(!i.dir).then(|| view! { <span class="size">{human_size(i.size)}</span> })}
                                            {i.dir.then(|| view! { <span class="chev">"›"</span> })}
                                        </div>
                                    }
                                }).collect_view()}
                                {(n == 0 && short).then(|| view! { <div class="empty">"Only the first 5000 items are shown."</div> })}
                            </div>
                            <Grip size=width key=String::new() min=140.0 max=600.0 class="grip col-grip"/>
                        }
                    }
                </For>
            }.into_any()
        }
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
    let all = Memo::new(move |_| if current.get() { Some(p.entries.get()) } else { p.cols.with(|c| c.get(&d).cloned()) });
    // Only the rows in view (and a little either side) are rendered. Keys and arrows move in the
    // current column, so that's the one `rows::reveal` scrolls.
    let scroller = NodeRef::<html::Div>::new();
    let len = Signal::derive(move || all.with(|a| a.as_ref().map_or(0, Vec::len)));
    let pane = Signal::derive(move || current.get().then_some(p.id));
    let win = rows::Window::new(scroller, len, rows::Rows { h: 26.0, per: 1, top: 4.0 }, rows::column_rows, pane);
    let dir_v = StoredValue::new(dir.clone());
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
        <div class="col" node_ref=scroller data-rows=move || current.get().then_some("")
            on:scroll=move |_| { win.update(); if current.get_untracked() { more_near_end(ctx, p, scroller) } }
            class:current=current on:contextmenu=bg_menu data-dir=data_dir
            style=move || match width.get() { w if w > 0.0 => format!("flex-basis: {w}px"), _ => String::new() }
            class:drop=move || ctx.drop_at.get().as_deref() == Some(key.as_str())
            on:dragover=over on:drop=drop>
            {move || match all.with(|a| a.as_ref().map(Vec::is_empty)) {
                None => Some(view! { <div class="empty"><span class="spin"></span></div> }.into_any()),
                Some(true) => Some(view! { <div class="empty">"Empty"</div> }.into_any()),
                Some(false) => None,
            }}
            <div class="gap" style=move || format!("height: {}px", win.gaps().0)></div>
            <For each=move || all.with(|a| a.as_deref().map(|es| win.slice(es)).unwrap_or_default()) key=|e| (e.path.clone(), e.size, e.mtime) let(e)>
                {
                    let (p1, p2) = (e.path.clone(), e.path.clone());
                    let (e1, e2) = (e.clone(), e.clone());
                    let dir = dir_v.get_value();
                    let click = move |ev: web_sys::MouseEvent| {
                        if current.get_untracked() && (crate::os::cmd(ev.meta_key(), ev.ctrl_key()) || ev.shift_key()) {
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
                            data-path=e.path.clone() data-cmp={let e = e.clone(); move || cmp_mark(p, &e)} class:selected=move || current.get() && p.is_picked(&p2)
                            class:drop=move || ctx.drop_at.get().as_deref() == Some(key.as_str())
                            draggable=move || (!editing()).to_string() data-dir=fdir
                            on:dragstart=drag_start(ctx, p, e.path.clone()) on:dragend=drag_end(ctx)
                            on:dragover=move |ev| if is_dir { over(ev) } on:drop=move |ev| if is_dir { drop(ev) }
                            on:click=click
                            on:dblclick=move |_| if !e2.dir { ctx.open(p, e2.clone()) }
                            on:contextmenu=move |ev| ctx.show_menu(p, &ev, Some(e.clone()))>
                            <span class="icon">{icon(k)}</span>
                            <span class="name"><Name pane=p entry=e.clone() split=true/></span>
                            {e.dir.then(|| view! { <span class="chev">"›"</span> })}
                        </div>
                    }
                }
            </For>
            <div class="gap" style=move || format!("height: {}px", win.gaps().1)></div>
            {move || current.get().then(|| more_row(ctx, p))}
        </div>
    }
}

/// How near the bottom (in pixels) scrolling loads the folder's next page.
const MORE_NEAR: i32 = 600;

/// Loads pane `p`'s next page when `scroller` is scrolled near its bottom.
fn more_near_end(ctx: Ctx, p: Pane, scroller: NodeRef<html::Div>) {
    if !p.more.get_untracked() {
        return;
    }
    if let Some(el) = scroller.get_untracked() {
        if el.scroll_top() + el.client_height() + MORE_NEAR >= el.scroll_height() {
            ctx.load_more(p);
        }
    }
}

/// The row at the bottom of a folder listed a page at a time, while it has more: loads the next
/// page when clicked (and when scrolled to).
fn more_row(ctx: Ctx, p: Pane) -> impl IntoView {
    move || p.more.get().then(|| view! {
        <div class="more-row">
            {move || if p.loading_more.get() {
                view! { <span><span class="spin"></span>" Loading more…"</span> }.into_any()
            } else {
                view! {
                    <span class="count">{move || format!("{} of {}", count(p.got.get_value() as u64), count(p.total.get()))}</span>
                    <button on:click=move |ev| { ev.stop_propagation(); ctx.load_more(p) }>
                        {format!("Load {} more", count(super::nav::PAGE as u64))}</button>
                }.into_any()
            }}
        </div>
    })
}
