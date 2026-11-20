//! The Editor view: the pane's folder as a tree beside a text editor whose files are in tabs.

use super::sidebar::chev;
use super::*;

/// The tree's narrowest width, in px.
const TREE_MIN: f64 = 140.0;

/// Folders first, then by name.
fn tree_order(mut es: Vec<Entry>) -> Vec<Entry> {
    es.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    es
}

#[component]
pub(super) fn CodeView(pane: Pane) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let p = pane;
    // The editor is made again for each file.
    let shown = Memo::new(move |_| p.doc.with(|d| d.as_ref().map(|d| (d.src, d.path.clone()))));
    let root = move || {
        let place = p.cwd.get();
        match place.as_str() {
            "/" => ctx.src_info(p.src.get()).map_or_else(|| "/".into(), |i| i.label),
            _ => base(&place),
        }
    };
    view! {
        <div class="code-view">
            <div class="code-tree" role="tree" aria-label="Files"
                style=move || match p.tree_w.get() { w if w > 0.0 => format!("width: {w}px"), _ => String::new() }>
                <div class="code-tree-head" title=move || p.cwd.get()>{root}</div>
                <div class="code-tree-scroll">
                    {move || tree_order(ctx.visible(p.all.get())).into_iter().map(|e| view! { <CodeNode pane=p entry=e depth=0/> }).collect_view()}
                </div>
            </div>
            <Grip size=p.tree_w key=format!("ferry-files.tree-w.{}", p.id) min=TREE_MIN target=Target::Prev room=300.0/>
            <div class="code-main">
                <DocTabs pane=p/>
                {move || match shown.get() {
                    Some(_) => view! { <TextEditor file=p.slot()/> }.into_any(),
                    None => view! {
                        <div class="code-empty">"Open a text file from the tree."</div>
                        <div class="statusbar">
                            <span class="spacer"></span>
                            <ViewSeg pane=p/>
                        </div>
                    }
                    .into_any(),
                }}
            </div>
        </div>
    }
}

/// A row of the tree: a folder (click to show what's in it, listed each time it's opened) or a
/// file (click to open a text file; double-click opens other files).
#[component]
fn CodeNode(pane: Pane, entry: Entry, depth: usize) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let p = pane;
    let src = p.src.get_untracked();
    let path = StoredValue::new(entry.path.clone());
    let style = format!("--depth: {depth}");
    let title = entry.path.clone();
    let name = entry.name.clone();
    if entry.dir {
        let open = Memo::new(move |_| path.with_value(|x| p.tree_open.with(|o| o.contains(x))));
        // What's in it: None until listed. A listing that fails shows nothing under it.
        let kids = RwSignal::new(None::<Vec<Entry>>);
        Effect::new(move |_| {
            if !open.get() {
                return;
            }
            spawn_local(async move {
                let args = json!({ "src": src.key(), "path": path.get_value(), "current": false });
                let found = call::<Vec<Entry>>("list", args).await.map(|es| tree_order(ctx.visible(es))).unwrap_or_default();
                let _ = kids.try_set(Some(found));
            });
        });
        let toggle = move |_| {
            let x = path.get_value();
            p.tree_open.update(|o| {
                if !o.remove(&x) {
                    o.insert(x);
                }
            });
        };
        let leaf = move || kids.with(|k| k.as_ref().is_some_and(Vec::is_empty));
        view! {
            <div class="tree-row" style=style role="treeitem" aria-expanded=move || open.get().to_string()>
                <button class="twisty" class:none=leaf tabindex="-1" aria-hidden="true" on:click=toggle>{chev(open.into())}</button>
                <button class="item dir" title=title on:click=toggle>{name}</button>
            </div>
            {move || open.get().then(|| kids.get().map(|k| k.into_iter().map(|e| {
                view! { <CodeNode pane=p entry=e depth=depth + 1/> }.into_any()
            }).collect_view()))}
        }
        .into_any()
    } else {
        let active = Memo::new(move |_| p.doc.with(|d| d.as_ref().is_some_and(|d| d.src == src && path.with_value(|x| *x == d.path))));
        let row = NodeRef::<html::Button>::new();
        // The open file's row is scrolled to, when it's opened and when it's first shown.
        Effect::new(move |_| {
            if active.get() {
                request_animation_frame(move || {
                    if let Some(el) = row.get_untracked() {
                        let opts = web_sys::ScrollIntoViewOptions::new();
                        opts.set_block(web_sys::ScrollLogicalPosition::Nearest);
                        el.scroll_into_view_with_scroll_into_view_options(&opts);
                    }
                });
            }
        });
        let text = kind(&entry) == Kind::Text;
        let e = StoredValue::new(entry);
        view! {
            <div class="tree-row file" style=style role="treeitem" aria-selected=move || active.get().to_string()>
                <span class="twisty none"></span>
                <button class="item" class:active=active class:off=!text node_ref=row
                    title=if text { title } else { format!("{title}\nNot a text file: double-click to open it") }
                    on:click=move |_| if text { ctx.open_doc(p, path.get_value()) }
                    on:dblclick=move |_| if !text { ctx.open(p, e.get_value()) }>{name}</button>
            </div>
        }
        .into_any()
    }
}

/// The Editor view's tabs: each file's name, • while it has unsaved changes, × to close it.
#[component]
fn DocTabs(pane: Pane) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let p = pane;
    let n = move || p.docs.with(Vec::len);
    view! {
        <div class="doc-tabs" role="tablist" hidden=move || n() == 0>
            {move || (0..n()).map(|i| {
                let shown = move || p.doc_at.get() == i;
                let path = move || p.docs.with(|ds| ds.get(i).map(|d| d.ed.path.clone()).unwrap_or_default());
                // The shown tab's text is in the editor; the others' in their entries.
                let dirty = move || if shown() { p.slot().dirty() } else { p.docs.with(|ds| ds.get(i).is_some_and(|d| d.ed.saved_text != d.text)) };
                let label = move || format!("Close {}", base(&path()));
                // Its folder's name, when another tab's file has the same name.
                let where_ = move || {
                    let path = path();
                    let name = base(&path);
                    let clash = p.docs.with(|ds| ds.iter().filter(|d| base(&d.ed.path) == name).count() > 1);
                    clash.then(|| base(&parent(&path)))
                };
                view! {
                    <div class="doc-tab" class:shown=shown class:dirty=dirty role="tab" aria-selected=move || shown().to_string() title=path
                        on:click=move |_| ctx.show_doc(p, i)
                        // A middle click closes it.
                        on:auxclick=move |ev| if ev.button() == 1 { ev.prevent_default(); ctx.close_doc(p, i, false) }>
                        <span class="name">{move || base(&path())}</span>
                        <span class="where">{where_}</span>
                        <button class="close" title=move || if dirty() { "Unsaved changes. Close…".to_string() } else { ctx.with_key("Close", "close_tab") } aria-label=label
                            on:click=move |ev| { ev.stop_propagation(); ctx.close_doc(p, i, false) }>
                            <span class="dot" aria-hidden="true"></span>{ico(Icon::Close)}
                        </button>
                    </div>
                }
            }).collect_view()}
        </div>
    }
}
