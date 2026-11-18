//! A pane: its path, notices, status bar, and the click, drag and drop handling its views share.

use super::*;

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
    view! {
        <div class="crumbs" title="Click here or press ⌘L to type a path" on:click=edit>
            {move || if p.editing_path.get() {
                view! { <PathEdit pane=p/> }.into_any()
            } else {
                let cwd = p.cwd.get();
                let mut acc = String::new();
                let mut out = vec![view! { <button on:click=move |_| ctx.navigate(p, "/".into())>"/"</button> }.into_any()];
                for part in cwd.split('/').filter(|s| !s.is_empty()) {
                    acc = format!("{acc}/{part}");
                    let target = acc.clone();
                    out.push(view! {
                        <button on:click=move |_| ctx.navigate(p, target.clone())>{part.to_string()}</button>
                    }.into_any());
                    out.push(view! { <span class="sep">"/"</span> }.into_any());
                }
                out.pop_if(|_| cwd != "/");
                out.into_any()
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
                    [e] => format!("Delete {} from {place}? A backup is kept on the Mac.", e.name),
                    _ => format!("Delete {} items from {place}? Files are backed up on the Mac; folders are deleted only when empty.", items.len()),
                };
                view! {
                    <div class="notice"><span class="text">{label}</span>
                        <button class="btn danger solid" on:click=move |_| ctx.delete(p, items.clone())>"Delete"</button>
                        <button class="btn" on:click=dismiss>"Cancel"</button></div>
                }.into_any()
            }
            Notice::Sync { report: r, from, to } => {
                let list = |what: &str, v: &[String]| -> String {
                    let mut s = v.iter().take(4).cloned().collect::<Vec<_>>().join(", ");
                    if v.len() > 4 {
                        s.push_str(&format!(", and {} more", v.len() - 4));
                    }
                    format!("{} {what}: {s}.", v.len())
                };
                let mut lines = vec![if r.files + r.folders == 0 {
                    format!("Nothing to copy: everything in {} is already in {}.", base(&from.1), to.1)
                } else {
                    format!("Copy {} {} ({}){} from {} into {} on {}?",
                        r.files, if r.files == 1 { "file" } else { "files" }, human_size(r.bytes),
                        if r.folders > 0 { format!(" and {} {}", r.folders, if r.folders == 1 { "folder" } else { "folders" }) } else { String::new() },
                        base(&from.1), to.1, ctx.place(to.0))
                }];
                if r.same > 0 {
                    lines.push(format!("{} already there.", r.same));
                }
                if !r.differ.is_empty() {
                    lines.push(format!("{} Left as they are.", list("differ in size", &r.differ)));
                }
                if !r.extra.is_empty() {
                    lines.push(format!("{} Left as they are.", list("only in the other pane", &r.extra)));
                }
                let go = r.files + r.folders > 0;
                let (f, t) = (StoredValue::new(from), StoredValue::new(to));
                view! {
                    <div class="notice"><span class="text">{lines.join("\n")}</span>
                        {go.then(|| view! { <button class="btn primary" on:click=move |_| ctx.sync(p, f.get_value(), t.get_value())>"Copy"</button> })}
                        <button class="btn" on:click=dismiss>{if go { "Cancel" } else { "Dismiss" }}</button></div>
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
                        <button class="btn primary" on:click=move |_| submit()>"Create"</button>
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
            <div class="bar">
                <button class="btn" title="Up (⌘↑)" disabled=move || p.cwd.get() == "/" on:click=move |_| ctx.go_up(p)>"↑"</button>
                <div class="seg" role="group" aria-label="Show files on">
                    <button title="Files on the Mac" aria-label="Mac" class:active=move || p.src.get() == Src::Mac
                        on:click=move |_| ctx.switch_src(p, Src::Mac)>"💻"</button>
                    // One button per connected device.
                    {move || {
                        let phones = ctx.phones.get();
                        if phones.is_empty() {
                            return view! { <button title="No device connected" aria-label="Device" disabled=true>"📱"</button> }.into_any();
                        }
                        phones.into_iter().map(|i| {
                            let src = Src::of(&i.id);
                            view! {
                                <button title=format!("Files on {} ({})", i.label, i.transport) aria-label=i.label.clone()
                                    class:active=move || p.src.get() == src on:click=move |_| ctx.switch_src(p, src)>"📱"</button>
                            }
                        }).collect_view().into_any()
                    }}
                </div>
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
                        title="Show names containing this (⌘F or /). Enter searches subfolders too."
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
                {(p.id == 0).then(|| view! {
                    <button class="btn" class:active=move || ctx.split.get()
                        title=move || if ctx.split.get() { "Hide the second pane (⌘\\)" } else { "Show a second pane (⌘\\)" }
                        aria-label="Second pane" on:click=move |_| ctx.set_split(!ctx.split.get_untracked())>"◫"</button>
                })}
                {(p.id == 1).then(|| view! {
                    <button class="btn" title="Hide this pane (⌘\\)" aria-label="Hide this pane"
                        on:click=move |_| ctx.set_split(false)>"✕"</button>
                })}
            </div>
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
                    <Grip size=p.view_w key=format!("yomi-files.view-w.{}", p.id) min=200.0 room=360.0/>
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
    if ev.alt_key() {
        false
    } else if ev.meta_key() {
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
            <span class="search">{format!("{n} found for “{q}”{more}")}</span>
            <button class="btn small" on:click=move |_| ctx.reload(p)>"Back to folder"</button>
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
                    store(&format!("yomi-files.preview.{}", p.id), if on { "1" } else { "0" });
                }>
                <svg viewBox="0 0 16 16" aria-hidden="true"><rect x="1.5" y="2.5" width="13" height="11" rx="1.5"/><path d="M9.5 2.5v11"/></svg>
            </button>
        </div>
    }
}
