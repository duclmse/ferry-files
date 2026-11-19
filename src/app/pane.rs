//! A pane: its path, notices, status bar, and the click, drag and drop handling its views share.

use super::*;

/// The path, one button per folder. When it doesn't fit, the folders after "/" fold into "…",
/// from the left, as many as need to; "…" lists them.
#[component]
pub(super) fn Crumbs(pane: Pane) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let p = pane;
    // A click on the empty part of the path (or ⌘L) turns it into a box to type a path in.
    let edit = move |ev: web_sys::MouseEvent| {
        if ev.target() == ev.current_target() {
            p.editing_path.set(true);
        }
    };
    let el = NodeRef::<html::Div>::new();
    // How many folders (after "/") are folded into "…". Every folder stays in the page; folded
    // ones are only hidden, so changing this toggles classes instead of rebuilding the path.
    let fold = RwSignal::new(0usize);
    // Shows everything for a moment, measures, and folds as many folders as it takes to fit. It
    // all happens before the browser paints, so nothing flickers.
    let fit = move || {
        let Some(el) = el.get_untracked() else { return };
        let classes = el.class_list();
        let _ = classes.add_1("measuring");
        let (need, room) = (el.scroll_width(), el.client_width());
        let width = |e: Option<web_sys::Element>| e.and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok()).map_or(0, |e| e.offset_width());
        // "…" is shown while measuring; "+ 1" is the gap beside each item.
        let more = width(el.query_selector(".part.more").ok().flatten()) + 1;
        let parts: Vec<i32> = el.query_selector_all(".part.dir").map_or(vec![], |ps| (0..ps.length()).map(|i| width(ps.item(i).and_then(|n| n.dyn_into().ok())) + 1).collect());
        let _ = classes.remove_1("measuring");
        let mut k = 0;
        if need - more > room {
            let mut over = need - room;
            // The last folder always shows; it's cut short if it alone doesn't fit.
            while over > 0 && k + 1 < parts.len() {
                over -= parts[k];
                k += 1;
            }
        }
        if fold.get_untracked() != k {
            fold.set(k);
        }
    };
    // After the path changes and is drawn (an animation frame comes before the next paint).
    Effect::new(move |_| {
        p.cwd.track();
        if !p.editing_path.get() {
            request_animation_frame(fit);
        }
    });
    // Right away when the path's room changes: resize observers run between layout and paint.
    let watcher = StoredValue::new_local(None::<(web_sys::ResizeObserver, wasm_bindgen::closure::Closure<dyn Fn()>)>);
    let width = StoredValue::new(0);
    Effect::new(move |_| {
        let Some(node) = el.get() else { return };
        let cb = wasm_bindgen::closure::Closure::<dyn Fn()>::new(move || {
            let w = el.get_untracked().map_or(0, |e| e.client_width());
            if w != width.get_value() {
                width.set_value(w);
                fit();
            }
        });
        if let Ok(ro) = web_sys::ResizeObserver::new(cb.as_ref().unchecked_ref()) {
            ro.observe(&node);
            watcher.set_value(Some((ro, cb)));
        }
    });
    on_cleanup(move || {
        if let Some((ro, _)) = watcher.try_update_value(Option::take).flatten() {
            ro.disconnect();
        }
    });
    view! {
        <div class="crumbs" node_ref=el title=format!("Click here or press {} to type a path", crate::os::keys("⌘L")) on:click=edit>
            {move || if p.editing_path.get() {
                view! { <PathEdit pane=p/> }.into_any()
            } else {
                let cwd = p.cwd.get();
                let mut paths = Vec::new();
                let mut acc = String::new();
                for part in cwd.split('/').filter(|s| !s.is_empty()) {
                    acc = format!("{acc}/{part}");
                    paths.push((part.to_string(), acc.clone()));
                }
                let last = paths.len().saturating_sub(1);
                let all = StoredValue::new(paths.iter().map(|(_, path)| path.clone()).collect::<Vec<_>>());
                let folded = move || all.with_value(|a| a[..fold.get().min(a.len())].to_vec());
                view! {
                    <button on:click=move |_| ctx.navigate(p, "/".into())>"/"</button>
                    <span class="part more" class:folded=move || fold.get() == 0>
                        <button class="more" title=move || folded().last().cloned().unwrap_or_default()
                            on:click=move |ev| ctx.show_folders_menu(p, &ev, folded())>"…"</button>
                    </span>
                    {paths.into_iter().enumerate().map(|(i, (name, path))| view! {
                        <span class="part dir" class:last=i == last class:folded=move || i < fold.get()>
                            {(i > 0).then(|| view! { <span class="sep">"/"</span> })}
                            <button title=path.clone() on:click=move |_| ctx.navigate(p, path.clone())>{name}</button>
                        </span>
                    }).collect_view()}
                }.into_any()
            }}
        </div>
    }
}

/// The path as text: Enter goes there, Escape (or clicking away) leaves it as it was.
#[component]
fn PathEdit(pane: Pane) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let p = pane;
    let input = NodeRef::<html::Input>::new();
    input.on_load(move |el| {
        request_animation_frame(move || {
            let _ = el.focus();
            el.select();
        });
    });
    let go = move || {
        let typed = input.get_untracked().map(|el| el.value()).unwrap_or_default();
        let typed = typed.trim();
        p.editing_path.set(false);
        let path = match typed.trim_end_matches('/') {
            "" => "/".to_string(),
            t if t.starts_with('/') => t.to_string(),
            // Relative to the folder shown.
            t => format!("{}/{t}", p.cwd.get_untracked().trim_end_matches('/')),
        };
        if path != p.cwd.get_untracked() {
            ctx.navigate(p, path);
        }
    };
    view! {
        <input class="path" node_ref=input value=p.cwd.get_untracked() spellcheck="false" autocomplete="off"
            on:keydown=move |ev| match ev.key().as_str() {
                "Enter" => { ev.prevent_default(); go() }
                "Escape" => { ev.prevent_default(); ev.stop_propagation(); p.editing_path.set(false) }
                _ => {}
            }
            on:blur=move |_| p.editing_path.set(false)/>
    }
}

#[component]
pub(super) fn Notices(pane: Pane) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let p = pane;
    let name = RwSignal::new(String::new());
    let dismiss = move |_| p.notice.set(Notice::None);
    view! {
        {move || match p.notice.get() {
            Notice::None => ().into_any(),
            Notice::Error(m) => view! {
                <div class="notice error"><span class="text">{m}</span>
                    <button class="btn" on:click=dismiss>"Dismiss"</button></div>
            }.into_any(),
            Notice::Ok(m) => view! {
                <div class="notice ok"><span class="text">{m}</span>
                    <button class="btn" on:click=dismiss>"Dismiss"</button></div>
            }.into_any(),
            Notice::ConfirmDelete(items) => {
                let place = ctx.place(p.src.get_untracked());
                let label = match items.as_slice() {
                    [e] if e.dir => format!("Delete the folder {} from {place}? Only an empty folder can be deleted.", e.name),
                    [e] => format!("Delete {} from {place}? A backup is kept on {}.", e.name, crate::os::the_computer()),
                    _ => format!("Delete {} items from {place}? Files are backed up on {}; folders are deleted only when empty.", items.len(), crate::os::the_computer()),
                };
                view! {
                    <div class="notice"><span class="text">{label}</span>
                        <button class="btn danger solid" on:click=move |_| ctx.delete(p, items.clone())>{ico(Icon::Trash)}"Delete"</button>
                        <button class="btn" on:click=dismiss>"Cancel"</button></div>
                }.into_any()
            }
            Notice::Sync { report: r, from, to, mode } => {
                let list = |what: &str, v: &[String]| -> String {
                    let mut s = v.iter().take(4).cloned().collect::<Vec<_>>().join(", ");
                    if v.len() > 4 {
                        s.push_str(&format!(", and {} more", v.len() - 4));
                    }
                    format!("{} {what}: {s}.", v.len())
                };
                let files = |n: u32| format!("{n} {}", if n == 1 { "file" } else { "files" });
                let mode = RwSignal::new(mode);
                let typed = RwSignal::new(String::new());
                let r = StoredValue::new(r);
                let (f, t) = (StoredValue::new(from), StoredValue::new(to));
                let removing = move || mode.with(|m| m == "mirror") && r.with_value(|r| !r.extra.is_empty());
                // Many removals need the word typed, so a mirror isn't run by a stray click.
                let must_type = move || removing() && r.with_value(|r| r.extra.len() > MIRROR_TYPE_OVER);
                let lines = move || {
                    let (from, to) = (f.get_value(), t.get_value());
                    let m = mode.get();
                    r.with_value(|r| {
                        let copy = r.files + r.folders > 0;
                        let replace = m != "copy" && r.changed > 0;
                        let mut lines = vec![];
                        if copy {
                            lines.push(format!("Copy {} ({}){} from {} into {} on {}.",
                                files(r.files), human_size(r.bytes),
                                if r.folders > 0 { format!(" and {} {}", r.folders, if r.folders == 1 { "folder" } else { "folders" }) } else { String::new() },
                                base(&from.1), to.1, ctx.place(to.0)));
                        }
                        if replace {
                            lines.push(format!("Replace {} that differ ({}). The ones there now are backed up.", files(r.changed), human_size(r.changed_bytes)));
                        }
                        if !(copy || replace || (m == "mirror" && !r.extra.is_empty())) {
                            lines.push(format!("Nothing to do: {} already has what's in {}.", to.1, base(&from.1)));
                        }
                        if r.same > 0 {
                            lines.push(format!("{} already there.", r.same));
                        }
                        let kept = r.differ.len() - if m == "copy" { 0 } else { r.changed as usize };
                        if m == "copy" && kept > 0 {
                            lines.push(format!("{} Left as they are.", list("differ in size", &r.differ)));
                        } else if kept > 0 {
                            lines.push(format!("{kept} {} a file on one side and a folder on the other. Left as {}.", if kept == 1 { "is" } else { "are" }, if kept == 1 { "it is" } else { "they are" }));
                        }
                        if !r.extra.is_empty() {
                            if m == "mirror" {
                                lines.push(format!("Remove {} only in {} (folders with everything in them). Files are backed up first:", r.extra.len(), to.1));
                            } else {
                                lines.push(format!("{} Left as they are.", list("only in the other pane", &r.extra)));
                            }
                        }
                        lines.join("\n")
                    })
                };
                let go = move || {
                    let m = mode.get();
                    r.with_value(|r| r.files + r.folders > 0 || (m != "copy" && r.changed > 0) || (m == "mirror" && !r.extra.is_empty()))
                };
                let ready = move || go() && (!must_type() || typed.with(|x| x.trim().eq_ignore_ascii_case("remove")));
                let run = move || {
                    let remove = if removing() { r.with_value(|r| r.extra.clone()) } else { vec![] };
                    ctx.sync(p, f.get_value(), t.get_value(), mode.get_untracked(), remove);
                };
                let label = move || match mode.get().as_str() {
                    "update" => "Update",
                    "mirror" => "Mirror",
                    _ => "Copy",
                };
                view! {
                    <div class="notice sync"><span class="text">{lines}</span>
                        <select aria-label="How to sync" title="Copy: only what's missing. Update: also replace files that differ. Mirror: also remove what's only in the target."
                            on:change=move |e| { mode.set(event_target_value(&e)); typed.set(String::new()); }>
                            {SYNC_MODES.iter().map(|(v, l)| view! { <option value=*v selected=move || mode.with(|m| m == v)>{*l}</option> }).collect_view()}
                        </select>
                        {move || must_type().then(|| view! {
                            <input class="confirm" placeholder="Type remove" aria-label="Type remove to confirm"
                                prop:value=move || typed.get() on:input=move |e| typed.set(event_target_value(&e))
                                on:keydown=move |e| if e.key() == "Enter" && ready() { run() }/>
                        })}
                        <button class="btn primary" class:danger=removing class:solid=removing disabled=move || !ready()
                            on:click=move |_| run()>{ico(Icon::Copy)}{label}</button>
                        <button class="btn" title="Keep this sync in the sidebar, to run again"
                            on:click=move |_| { ctx.save_pair(f.get_value(), t.get_value(), mode.get_untracked()); p.notice.set(Notice::Ok("Saved under Sync pairs in the sidebar.".into())); }>"Save"</button>
                        <button class="btn" on:click=dismiss>"Cancel"</button>
                        {move || removing().then(|| view! {
                            <ul class="sync-remove">{r.with_value(|r| r.extra.iter().map(|x| view! { <li>{x.clone()}</li> }).collect_view())}</ul>
                        })}
                    </div>
                }.into_any()
            }
            Notice::NewItem { folder } => {
                name.set(String::new());
                let submit = move || ctx.create(p, name.get_untracked(), folder);
                view! {
                    <div class="notice">
                        <span>{if folder { "New folder:" } else { "New text file:" }}</span>
                        <input autofocus placeholder=if folder { "Folder name" } else { "notes.txt" }
                            prop:value=move || name.get()
                            on:input=move |ev| name.set(event_target_value(&ev))
                            on:keydown=move |ev| match ev.key().as_str() {
                                "Enter" => submit(),
                                "Escape" => p.notice.set(Notice::None),
                                _ => {}
                            }/>
                        <button class="btn primary" on:click=move |_| submit()>{ico(Icon::Plus)}"Create"</button>
                        <button class="btn" on:click=dismiss>"Cancel"</button>
                    </div>
                }.into_any()
            }
        }}
    }
}

/// The name of `e`, or a field to rename it when it's being renamed. Enter or clicking elsewhere
/// renames; Escape leaves it as it was.
#[component]
pub(super) fn Name(pane: Pane, entry: Entry) -> impl IntoView {
    let p = pane;
    let path = entry.path.clone();
    let e = StoredValue::new(entry);
    let editing = Memo::new(move |_| p.renaming.with(|r| r.as_deref() == Some(path.as_str())));
    move || if editing.get() {
        view! { <NameEdit pane=p entry=e.get_value()/> }.into_any()
    } else {
        e.with_value(|e| e.name.clone()).into_any()
    }
}

#[component]
fn NameEdit(pane: Pane, entry: Entry) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let p = pane;
    let input = NodeRef::<html::Input>::new();
    let done = StoredValue::new(false);
    // Select the name without its extension, as Finder does.
    let stem = match (entry.dir, entry.name.rsplit_once('.')) {
        (false, Some((s, _))) if !s.is_empty() => s.encode_utf16().count(),
        _ => entry.name.encode_utf16().count(),
    } as u32;
    input.on_load(move |el| {
        request_animation_frame(move || {
            let _ = el.focus();
            let _ = el.set_selection_range(0, stem);
        });
    });
    let name = entry.name.clone();
    let e = StoredValue::new(entry);
    let finish = move |keep: bool| {
        if done.get_value() {
            return;
        }
        done.set_value(true);
        let new = input.get_untracked().map(|el| el.value()).unwrap_or_default();
        if keep {
            ctx.rename(p, e.get_value(), new);
        } else {
            p.renaming.set(None);
        }
    };
    view! {
        <input class="rename" node_ref=input value=name spellcheck="false" autocomplete="off"
            on:click=|ev| ev.stop_propagation()
            on:dblclick=|ev| ev.stop_propagation()
            on:mousedown=|ev| ev.stop_propagation()
            on:contextmenu=|ev| ev.stop_propagation()
            on:keydown=move |ev| match ev.key().as_str() {
                "Enter" => { ev.prevent_default(); finish(true) }
                "Escape" => { ev.prevent_default(); ev.stop_propagation(); finish(false) }
                _ => {}
            }
            on:blur=move |_| finish(true)/>
    }
}

/// One pane: its toolbar (phone or Mac, the folder path, the view), its files, and its status bar.
/// New folder, new file, upload and copy are in the context menu.
#[component]
pub(super) fn PaneView(pane: Pane) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let p = pane;
    let active = move || ctx.active.get() == p.id;
    let filter_ref = NodeRef::<html::Input>::new();
    view! {
        <section class="pane" class:active=active data-pane=p.id.to_string() on:mousedown=move |_| ctx.active.set(p.id)
            // The left pane keeps the width it was dragged to; the right one takes the rest.
            style=move || match ctx.pane_w.get() {
                w if w > 0.0 && p.id == 0 && ctx.split.get() => format!("flex: 0 1 {w}px"),
                _ => String::new(),
            }>
            {move || (p.tabs.with(Vec::len) > 1).then(|| view! { <Tabs pane=p/> })}
            <div class="bar">
                {(p.id == 0).then(|| view! {
                    <button class="btn icon-only" class:active=move || ctx.side.get()
                        title=move || format!("{} the {} ({})", if ctx.side.get() { "Hide" } else { "Show" }, crate::os::sidebar(), crate::os::keys("⌥⌘S"))
                        aria-label="Sidebar" on:click=move |_| ctx.set_side(!ctx.side.get_untracked())>{ico(Icon::Sidebar)}</button>
                })}
                <button class="btn icon-only" title=format!("Up ({})", crate::os::keys("⌘↑")) aria-label="Up" disabled=move || p.cwd.get() == "/" on:click=move |_| ctx.go_up(p)>{ico(Icon::Up)}</button>
                <Crumbs pane=p/>
                {move || p.loading.get().then(|| view! { <span class="spin"></span> })}
                // Narrow panes show only the icon; clicking it opens the field.
                <div class="filter-box" class:has=move || !p.filter.with(String::is_empty)
                    on:click=move |_| if let Some(el) = filter_ref.get_untracked() { let _ = el.focus(); }>
                    <svg class="filter-icon" viewBox="0 0 16 16" aria-hidden="true">
                        <circle cx="7" cy="7" r="4.5"/><path d="M10.5 10.5 14 14"/>
                    </svg>
                    <datalist id=format!("searches-{}", p.id)>
                        {move || ctx.searches.get().into_iter().map(|q| view! { <option value=q></option> }).collect_view()}
                    </datalist>
                    <input class="filter" type="search" placeholder="Filter" node_ref=filter_ref list=format!("searches-{}", p.id)
                        title=format!("Show names containing this ({} or /). Enter searches subfolders too.", crate::os::keys("⌘F"))
                        prop:value=move || p.filter.get()
                        on:input=move |ev| { p.filter.set(event_target_value(&ev)); p.arrange(); }
                        on:keydown=move |ev| match ev.key().as_str() {
                            "Enter" => ctx.find(p),
                            "Escape" => {
                                p.filter.set(String::new());
                                p.arrange();
                                if let Some(t) = ev.target() { let _ = t.unchecked_into::<web_sys::HtmlElement>().blur(); }
                            }
                            _ => {}
                        }/>
                    {move || (!p.filter.with(String::is_empty)).then(|| view! {
                        <button class="filter-clear" title="Clear" aria-label="Clear filter"
                            on:mousedown=|ev| ev.prevent_default()
                            on:click=move |ev| {
                                ev.stop_propagation();
                                p.filter.set(String::new());
                                p.arrange();
                                if let Some(el) = filter_ref.get_untracked() { let _ = el.focus(); }
                            }>"✕"</button>
                    })}
                </div>
                <button class="btn icon-only" class:active=move || p.filters_open.get() || p.filtering()
                    title="Search filters: kind, size, date" aria-label="Search filters" aria-expanded=move || p.filters_open.get().to_string()
                    on:click=move |_| p.filters_open.update(|o| *o = !*o)>{ico(Icon::Funnel)}</button>
                <button class="btn icon-only" title=format!("New tab ({})", crate::os::keys("⌘T")) aria-label="New tab"
                    on:click=move |_| ctx.new_tab(p, p.src.get_untracked(), p.cwd.get_untracked())>{ico(Icon::NewTab)}</button>
                {(p.id == 0).then(|| view! {
                    <button class="btn icon-only" class:active=move || ctx.split.get()
                        title=move || format!("{} ({})", if ctx.split.get() { "Hide the second pane" } else { "Show a second pane" }, crate::os::keys("⌘\\"))
                        aria-label="Second pane" on:click=move |_| ctx.set_split(!ctx.split.get_untracked())>{ico(Icon::Split)}</button>
                })}
                {(p.id == 1).then(|| view! {
                    <button class="btn icon-only" title=format!("Hide this pane ({})", crate::os::keys("⌘\\")) aria-label="Hide this pane"
                        on:click=move |_| ctx.set_split(false)>{ico(Icon::Close)}</button>
                })}
            </div>
            {move || p.filters_open.get().then(|| {
                let pick = |label: &'static str, sig: RwSignal<usize>, names: Vec<&'static str>| view! {
                    <select aria-label=label prop:value=move || sig.get().to_string()
                        on:change=move |e| sig.set(event_target_value(&e).parse().unwrap_or(0))>
                        {names.into_iter().enumerate().map(|(i, n)| view! { <option value=i.to_string()>{n}</option> }).collect_view()}
                    </select>
                };
                view! {
                    <div class="search-filters">
                        {pick("Kind", p.search_kind, SEARCH_KINDS.iter().map(|x| x.0).collect())}
                        {pick("Size", p.search_size, SEARCH_SIZES.iter().map(|x| x.0).collect())}
                        {pick("Modified", p.search_time, SEARCH_TIMES.iter().map(|x| x.0).collect())}
                        <button class="btn small primary" on:click=move |_| ctx.find(p)>"Search"</button>
                        <button class="btn small" title="Back to any kind, size and time" on:click=move |_| {
                            p.search_kind.set(0);
                            p.search_size.set(0);
                            p.search_time.set(0);
                        }>"Clear"</button>
                        <span class="hint">"Searches this folder and its subfolders, with the text in Filter if any. Use * and ? for patterns, like IMG_*.jpg."</span>
                    </div>
                }
            })}
            <CompareBar pane=p/>
            <div class="body">
                <div class="files" class:beside=move || p.preview.get() class:sized=move || { p.view_w.get() > 0.0 }
                    style=move || match p.view_w.get() {
                        w if w > 0.0 && p.preview.get() => format!("flex-basis: {w}px"),
                        _ => String::new(),
                    }>
                    {move || match p.view.get() {
                        ViewMode::List => view! { <ListView pane=p/> }.into_any(),
                        ViewMode::Thumbs => view! { <ThumbView pane=p/> }.into_any(),
                        ViewMode::Columns => view! { <ColumnView pane=p/> }.into_any(),
                    }}
                </div>
                {move || p.preview.get().then(|| view! {
                    <Grip size=p.view_w key=format!("ferry-files.view-w.{}", p.id) min=200.0 room=360.0/>
                    <Preview pane=p/>
                })}
            </div>
            // Messages and questions about this pane, above its status bar.
            <Notices pane=p/>
            <ListStatus pane=p/>
        </section>
    }
}

/// Selects on click (⌘ and Shift pick several), opens on double-click, and shows the context menu
/// on right-click.
pub(super) fn row_events(
    ctx: Ctx,
    p: Pane,
    e: &Entry,
) -> (impl Fn(web_sys::MouseEvent) + 'static, impl Fn(web_sys::MouseEvent) + 'static, impl Fn(web_sys::MouseEvent) + 'static) {
    let (path, e1, e2) = (e.path.clone(), e.clone(), e.clone());
    (
        move |ev: web_sys::MouseEvent| {
            ev.stop_propagation();
            p.click(&ev, &path);
        },
        move |_| ctx.open(p, e1.clone()),
        move |ev: web_sys::MouseEvent| ctx.show_menu(p, &ev, Some(e2.clone())),
    )
}

/// Starts dragging an item: the picked items if it's one of them, otherwise just it.
pub(super) fn drag_start(ctx: Ctx, p: Pane, path: String) -> impl Fn(web_sys::DragEvent) + 'static {
    move |ev: web_sys::DragEvent| {
        if !p.picked().iter().any(|e| e.path == path) {
            p.select_only(Some(path.clone()));
        }
        let paths: Vec<String> = p.picked().into_iter().map(|e| e.path).collect();
        if let Some(dt) = ev.data_transfer() {
            let _ = dt.set_data("text/plain", &paths.join("\n"));
            dt.set_effect_allowed("copyMove");
        }
        ctx.drag.set_value(Some((p.src.get_untracked(), paths)));
    }
}

pub(super) fn drag_end(ctx: Ctx) -> impl Fn(web_sys::DragEvent) + 'static {
    move |_| {
        ctx.drag.set_value(None);
        ctx.drop_at.set(None);
    }
}

/// Lets items dragged from a pane drop into folder `dir` (None: the pane's folder). Dropping moves
/// within the same device and copies between devices, like Finder; ⌥ always copies, ⌘ always moves.
pub(super) fn drop_events(
    ctx: Ctx,
    p: Pane,
    dir: Option<String>,
) -> (impl Fn(web_sys::DragEvent) + 'static, impl Fn(web_sys::DragEvent) + 'static) {
    let key = format!("{}:{}", p.id, dir.clone().unwrap_or_default());
    let d = dir.clone();
    let over = move |ev: web_sys::DragEvent| {
        let Some((src, paths)) = ctx.drag.get_value() else { return };
        let target = dir_or_cwd(p, &d);
        // Not into one of the dragged folders, nor into the folder they're already in.
        if src == p.src.get_untracked() && paths.iter().any(|x| *x == target || parent(x) == target) {
            if ctx.drop_at.get_untracked().is_some() {
                ctx.drop_at.set(None);
            }
            return;
        }
        ev.prevent_default();
        ev.stop_propagation();
        if let Some(dt) = ev.data_transfer() {
            dt.set_drop_effect(if moves(&ev, src, p.src.get_untracked()) { "move" } else { "copy" });
        }
        if ctx.drop_at.get_untracked().as_deref() != Some(key.as_str()) {
            ctx.drop_at.set(Some(key.clone()));
        }
    };
    let drop = move |ev: web_sys::DragEvent| {
        ev.prevent_default();
        ev.stop_propagation();
        ctx.drop_at.set(None);
        let Some((src, paths)) = ctx.drag.write_value().take() else { return };
        let to_src = p.src.get_untracked();
        ctx.active.set(p.id);
        ctx.transfer(p, src, paths, to_src, dir_or_cwd(p, &dir), moves(&ev, src, to_src));
    };
    (over, drop)
}

pub(super) fn dir_or_cwd(p: Pane, dir: &Option<String>) -> String {
    dir.clone().unwrap_or_else(|| p.cwd.get_untracked())
}

pub(super) fn moves(ev: &web_sys::DragEvent, from: Src, to: Src) -> bool {
    // On the Mac ⌥ copies and ⌘ moves; elsewhere Ctrl copies and Shift moves, as in File Explorer.
    let (copy, mv) = if crate::os::is_mac() { (ev.alt_key(), ev.meta_key()) } else { (ev.ctrl_key(), ev.shift_key()) };
    if copy {
        false
    } else if mv {
        true
    } else {
        from == to
    }
}

pub(super) fn empty_note(p: Pane) -> impl IntoView {
    move || (p.entries.with(|e| e.is_empty()) && !p.loading.get()).then(|| view! {
        <div class="empty">"This folder is empty."</div>
    })
}

/// A pane's status bar: what's in the folder, what's selected, and where the files are.
#[component]
pub(super) fn ListStatus(pane: Pane) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let p = pane;
    let summary = move || {
        if p.loading.get() {
            return "Loading…".to_string();
        }
        p.entries.with(|es| {
            let folders = es.iter().filter(|e| e.dir).count();
            let files = es.len() - folders;
            let bytes: u64 = es.iter().filter(|e| !e.dir).map(|e| e.size).sum();
            let n = |c: usize, one: &str, many: &str| format!("{c} {}", if c == 1 { one } else { many });
            match (folders, files) {
                (0, 0) => "Empty folder".to_string(),
                (_, 0) => n(folders, "folder", "folders"),
                (0, _) => format!("{} · {}", n(files, "file", "files"), human_size(bytes)),
                _ => format!("{}, {} · {}", n(folders, "folder", "folders"), n(files, "file", "files"), human_size(bytes)),
            }
        })
    };
    let selection = move || {
        let n = p.marked.with(|m| m.len());
        if n > 1 {
            let bytes: u64 = p.marked.with(|m| p.entries.with(|es| es.iter().filter(|e| m.contains(&e.path)).map(|e| e.size).sum()));
            return Some(format!("{n} picked · {}", human_size(bytes)));
        }
        p.selected_entry().map(|e| {
            let size = if e.dir { String::new() } else { format!(" · {}", human_size(e.size)) };
            let modified = if e.mtime > 0 { format!(" · modified {}", when(e.mtime)) } else { String::new() };
            format!("{}{size}{modified}", e.name)
        })
    };
    let place = move || match p.src.get() {
        Src::Mac => ctx.mac.get().map(|i| i.label).unwrap_or_default(),
        src => ctx.src_info(src).map(|i| format!("{} · {}", i.label, i.transport)).unwrap_or_default(),
    };
    let search = move || p.found.get().map(|(q, complete)| {
        let n = p.all.with(|a| a.len());
        let more = if complete { "" } else { " (stopped early: open a smaller folder to see all)" };
        view! {
            <span class="search">{format!("{n} found for {q}{more}")}</span>
            <button class="btn small back" title="Back to folder" aria-label="Back to folder" on:click=move |_| ctx.reload(p)><svg class="back-icon" viewBox="0 0 16 16" aria-hidden="true"><path d="M13 8H3M7.5 3.5 3 8l4.5 4.5"/></svg></button>
        }
    });
    view! {
        <div class="statusbar">
            {search}
            <span>{summary}</span>
            <span class="sel">{selection}</span>
            <span class="spacer"></span>
            <span class="free">{move || ctx.free_text(p)}</span>
            <span>{place}</span>
            <div class="seg" role="group" aria-label="View">
                {ViewMode::ALL.map(|m| view! {
                    <button title=if m == ViewMode::List { "View as list (right-click to choose its columns)".to_string() } else { format!("View as {}", m.label()) }
                        aria-label=m.label() class:active=move || p.view.get() == m on:click=move |_| ctx.set_view(p, m)
                        on:contextmenu=move |ev| if m == ViewMode::List { ctx.show_columns_menu(p, &ev) }>{m.glyph()}</button>
                })}
            </div>
            <button class="btn small preview-toggle" class:active=move || p.preview.get()
                title=move || if p.preview.get() { "Hide the preview" } else { "Show the preview" } aria-label="Preview"
                on:click=move |_| {
                    let on = !p.preview.get_untracked();
                    p.preview.set(on);
                    store(&format!("ferry-files.preview.{}", p.id), if on { "1" } else { "0" });
                }>
                <svg viewBox="0 0 16 16" aria-hidden="true"><rect x="1.5" y="2.5" width="13" height="11" rx="1.5"/><path d="M9.5 2.5v11"/></svg>
            </button>
        </div>
    }
}

/// The pane's tabs, shown when there's more than one: each folder's name, × to close, + for a new one.
#[component]
fn Tabs(pane: Pane) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let p = pane;
    let n = move || p.tabs.with(Vec::len);
    view! {
        <div class="tabs" role="tablist">
            {move || (0..n()).map(|i| {
                let shown = move || p.tab.get() == i;
                // The shown tab follows the pane; the others are where they were left.
                let place = move || {
                    if shown() {
                        (p.src.get(), p.cwd.get())
                    } else {
                        p.tabs.with(|ts| ts.get(i).map(|t| (t.src, t.cwd.clone())).unwrap_or((Src::Mac, String::new())))
                    }
                };
                let name = move || {
                    let (src, cwd) = place();
                    match cwd.as_str() {
                        "" => "…".to_string(),
                        "/" => ctx.src_info(src).map_or_else(|| "/".into(), |i| i.label),
                        _ => base(&cwd),
                    }
                };
                let title = move || {
                    let (src, cwd) = place();
                    let label = ctx.src_info(src).map_or_else(String::new, |i| i.label);
                    if src == Src::Mac { crate::os::native_path(&cwd) } else { format!("{label}: {cwd}") }
                };
                view! {
                    <div class="tab" class:shown=shown role="tab" aria-selected=move || shown().to_string() title=title
                        on:click=move |_| ctx.show_tab(p, i)
                        // A middle click closes it.
                        on:auxclick=move |ev| if ev.button() == 1 { ev.prevent_default(); ctx.close_tab(p, i) }>
                        <span class="name">{name}</span>
                        <button class="close" title=format!("Close tab ({})", crate::os::keys("⌘W")) aria-label="Close tab"
                            on:click=move |ev| { ev.stop_propagation(); ctx.close_tab(p, i) }>"×"</button>
                    </div>
                }
            }).collect_view()}
            <button class="add" title=format!("New tab ({})", crate::os::keys("⌘T")) aria-label="New tab"
                on:click=move |_| ctx.new_tab(p, p.src.get_untracked(), p.cwd.get_untracked())>"+"</button>
        </div>
    }
}
