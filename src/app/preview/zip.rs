//! What's in a ZIP, and a file picked in it, unpacked or read in place.

use super::*;

/// A row of a ZIP's tree: its full name in the ZIP (folders end in /), its name, how deep it is,
/// and its unpacked size (a folder's: all that's in it).
#[derive(Clone, Debug, PartialEq)]
struct ZipRow {
    path: String,
    name: String,
    depth: usize,
    dir: bool,
    size: u64,
}

/// A ZIP's items as a tree, in the order shown: each folder before what's in it, folders first,
/// then by name. Folders are also made up from the files' names, since a ZIP needn't list them.
fn zip_tree(items: &[Packed]) -> Vec<ZipRow> {
    #[derive(Default)]
    struct Node {
        dir: bool,
        size: u64,
        kids: std::collections::BTreeMap<String, Node>,
    }
    let mut top = Node {
        dir: true,
        ..Default::default()
    };
    for i in items {
        let parts: Vec<&str> = i.name.split('/').filter(|s| !s.is_empty()).collect();
        let Some((last, dirs)) = parts.split_last() else {
            continue;
        };
        let mut at = &mut top;
        for d in dirs {
            at = at.kids.entry((*d).to_string()).or_default();
            at.dir = true;
        }
        let node = at.kids.entry((*last).to_string()).or_default();
        node.dir |= i.dir || i.name.ends_with('/');
        if !node.dir {
            node.size = i.size;
        }
    }
    fn total(n: &mut Node) -> u64 {
        if n.dir {
            n.size = n.kids.values_mut().map(total).sum();
        }
        n.size
    }
    total(&mut top);
    fn walk(n: &Node, prefix: &str, depth: usize, out: &mut Vec<ZipRow>) {
        let mut kids: Vec<(&String, &Node)> = n.kids.iter().collect();
        kids.sort_by_key(|(name, k)| (!k.dir, name.to_lowercase()));
        for (name, k) in kids {
            let path = format!("{prefix}{name}{}", if k.dir { "/" } else { "" });
            out.push(ZipRow {
                path: path.clone(),
                name: name.clone(),
                depth,
                dir: k.dir,
                size: k.size,
            });
            if k.dir {
                walk(k, &path, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(&top, "", 0, &mut out);
    out
}

/// The folders `path` is in, in a ZIP: "a/b/c" is in "a/" and "a/b/".
fn zip_parents(path: &str) -> impl Iterator<Item = &str> {
    let path = path.strip_suffix('/').unwrap_or(path);
    path.match_indices('/').map(move |(at, _)| &path[..=at])
}

/// What's in a ZIP (or APK, JAR, CBZ), as a tree whose folders open and close, read from the end
/// of the file. With `columns` (the pane's column view, which shows what's in it as columns of
/// its own), only how many items and how big.
#[component]
pub(super) fn ZipList(
    src: Src,
    entry: Entry,
    #[prop(into)] columns: Signal<bool>,
) -> impl IntoView {
    let got = RwSignal::new(None::<Result<Listing, String>>);
    let packed = entry.size;
    let args = json!({ "src": src.key(), "path": entry.path, "size": entry.size });
    spawn_local(async move {
        let r = call::<Listing>("zip_list", args).await;
        let _ = got.try_set(Some(r));
    });
    // The folders open. A ZIP whose all is in one folder opens with it open.
    let open = RwSignal::new(HashSet::<String>::new());
    view! {
        <div class="zip">
            {move || match got.get() {
                None => view! { <span class="spin"></span> }.into_any(),
                Some(Err(e)) => view! { <span class="hint">{e}</span> }.into_any(),
                Some(Ok(l)) => {
                    let unpacked: u64 = l.items.iter().map(|i| i.size).sum();
                    let shown = l.items.len() as u64;
                    let rows = zip_tree(&l.items);
                    let tops: Vec<&ZipRow> = rows.iter().filter(|r| r.depth == 0).collect();
                    if let [one] = tops.as_slice() {
                        if one.dir {
                            let x = one.path.clone();
                            open.update_untracked(|o| { o.insert(x); });
                        }
                    }
                    view! {
                        <span class="sub">{format!("{} {} · {} packed, {} unpacked", l.total, if l.total == 1 { "item" } else { "items" }, human_size(packed), human_size(unpacked))}</span>
                        <div class="zip-list" hidden=move || columns.get()>
                            <table>
                                <tbody>
                                    {rows.into_iter().map(|r| {
                                        let path = StoredValue::new(r.path.clone());
                                        let hidden = move || path.with_value(|x| open.with(|o| !zip_parents(x).all(|d| o.contains(d))));
                                        let is_open = Signal::derive(move || path.with_value(|x| open.with(|o| o.contains(x))));
                                        let toggle = move |_| {
                                            let x = path.get_value();
                                            open.update(|o| if !o.remove(&x) { o.insert(x); });
                                        };
                                        view! {
                                            <tr class:dir=r.dir hidden=hidden on:click=move |ev| if r.dir { toggle(ev) }
                                                aria-expanded=r.dir.then(|| move || is_open.get().to_string())>
                                                <td class="n" style=format!("--depth: {}", r.depth) title=r.path.clone()>
                                                    <span class="twisty">{r.dir.then(|| chev(is_open))}</span>
                                                    {r.name.clone()}
                                                </td>
                                                <td class="s">{human_size(r.size)}</td>
                                            </tr>
                                        }
                                    }).collect_view()}
                                </tbody>
                            </table>
                        </div>
                        {(shown < l.total).then(|| view! { <span class="more">{format!("Only the first {shown} are listed.")}</span> })}
                        {(l.total == 0).then(|| view! { <span class="hint">"Empty"</span> })}
                    }.into_any()
                }
            }}
        </div>
    }
}

/// A file packed in a ZIP: its name in it, and the ZIP's path and size.
#[derive(Clone)]
pub(in crate::app) struct Inner {
    pub(super) name: String,
    pub(super) zip_path: String,
    pub(super) zip_size: u64,
}

/// Files in a ZIP are unpacked whole for the preview, up to this size (as the backend's).
const UNPACK_MAX: u64 = 64 * 1024 * 1024;

/// A file picked in a ZIP's columns, `size` bytes unpacked: shown like any other file, from the
/// ZIP unpacked, or read in place when it's `stored` without compression.
#[component]
pub(super) fn Zipped(
    pane: Pane,
    src: Src,
    zip: Entry,
    name: String,
    size: u64,
    stored: bool,
) -> impl IntoView {
    let p = pane;
    let short = name.rsplit('/').next().unwrap_or(&name).to_string();
    let e = Entry {
        name: short.clone(),
        path: name.clone(),
        dir: false,
        size,
        mtime: 0,
        hidden: false,
    };
    let url = super::views::zipped_url(src, &zip, &name);
    // An EPUB in the ZIP shows its cover.
    let cover = crate::os::served(&format!(
        "cover?src={}&p={}&s={}&m={}&e={}",
        src.key(),
        uri_part(&zip.path),
        zip.size,
        zip.mtime,
        uri_part(&name),
    ));
    let inner = Inner {
        name: name.clone(),
        zip_path: zip.path.clone(),
        zip_size: zip.size,
    };
    let (pic_turn, video_turn) = (Turn::new(), Turn::new());
    let info = RwSignal::new(MediaInfo::default());
    let forced = RwSignal::new(None::<PeekAs>);
    // ‹ › and the picture's sides go through the pictures (or videos, or sounds) in its folder.
    let step = Callback::new(move |d| super::views::step_zipped(p, d));
    // Whether the body is text (as the match below picks it), for the wrap and line-number toggles.
    let (text_e, known_e) = (e.clone(), e.clone());
    let body = move || {
        let (e, url, inner, cover) = (e.clone(), url.clone(), inner.clone(), cover.clone());
        if size > UNPACK_MAX && !stored {
            return view! { <span class="hint">"Too big to show from inside the ZIP."</span> }
                .into_any();
        }
        match (forced.get(), media(&e)) {
            (Some(PeekAs::Text), _) => view! { <TextPeek src entry=e inner/> }.into_any(),
            (Some(PeekAs::Hex), _) => view! { <HexPeek src entry=e inner/> }.into_any(),
            (Some(PeekAs::Image), _) => view! { <FullImage pane=p src entry=e info turn=pic_turn url step/> }.into_any(),
            (Some(PeekAs::Video), _) => view! { <Player pane=p entry=e url video=true info turn=video_turn alone=true step/> }.into_any(),
            // A ZIP in a ZIP isn't listed.
            (Some(PeekAs::Zip), _) => view! { <span class="hint">"Not shown from inside a ZIP."</span> }.into_any(),
            (None, Some(Media::Image)) => view! { <FullImage pane=p src entry=e info turn=pic_turn url step/> }.into_any(),
            (None, Some(Media::Video)) => view! { <Player pane=p entry=e url video=true info turn=video_turn alone=true step/> }.into_any(),
            (None, Some(Media::Audio)) => view! { <Player pane=p entry=e url video=false info turn=video_turn alone=true step/> }.into_any(),
            _ if has_ext(&e, PDF_EXTS) => view! { <PdfView pane=p src entry=e url inner/> }.into_any(),
            _ if has_ext(&e, &["epub"]) => view! { <ZippedCover url=cover/> }.into_any(),
            _ if is_text(&e) => view! { <TextPeek src entry=e inner/> }.into_any(),
            // A type the preview doesn't know: "Open as…" under it.
            _ => view! { <OpenAs pick=move |h| forced.set(Some(h))/> }.into_any(),
        }
    };
    let text = Memo::new(move |_| {
        if size > UNPACK_MAX && !stored {
            return false;
        }
        match forced.get() {
            Some(how) => how == PeekAs::Text,
            None => {
                media(&text_e).is_none()
                    && !has_ext(&text_e, PDF_EXTS)
                    && !has_ext(&text_e, &["epub"])
                    && is_text(&text_e)
            }
        }
    });
    // Whether the preview can tell how to show it (as the match above picks it): "Open as…" goes
    // at the end of the line then, and in the middle of the view when it can't.
    let known = Memo::new(move |_| {
        !(size > UNPACK_MAX && !stored)
            && (forced.get().is_some()
                || media(&known_e).is_some()
                || has_ext(&known_e, PDF_EXTS)
                || has_ext(&known_e, &["epub"])
                || is_text(&known_e))
    });
    let folder = match name.rfind('/') {
        Some(at) => format!("{}/{}", zip.name, &name[..at]),
        None => zip.name.clone(),
    };
    let measures = Signal::derive(move || measures(Some(human_size(size)), info));
    let facts = Signal::derive(move || format!("In a ZIP · {folder}"));
    view! {
        <div class="meta">
            <div class="info">
                <span class="head">
                    <b class="name" title=format!("{}/{name}", zip.path)>{short.clone()}</b>
                    <span class="measures">{move || measures.get()}</span>
                </span>
                <span class="sub" title=move || facts.get()>{move || facts.get()}</span>
            </div>
            {move || text.get().then(text_tools)}
            {move || known.get().then(|| peek_as_buttons(forced.get(), false, move |how| forced.set(how)))}
        </div>
        <div class="view">{body}</div>
    }
}

/// A text file, a piece at a time: the next piece loads when the end is scrolled near. Code is
/// coloured unless that's turned off in Settings.

#[cfg(test)]
mod tests {
    use super::*;

    fn packed(name: &str, size: u64) -> Packed {
        Packed {
            name: name.into(),
            size,
            dir: name.ends_with('/'),
            stored: false,
        }
    }

    #[test]
    fn zip_tree_nests_and_sorts() {
        let items = [
            packed("b.txt", 1),
            packed("a/z.txt", 2),
            packed("a/c/d.txt", 3),
            packed("A2/", 0),
        ];
        let rows: Vec<(String, usize, bool, u64)> = zip_tree(&items)
            .into_iter()
            .map(|r| (r.path, r.depth, r.dir, r.size))
            .collect();
        assert_eq!(
            rows,
            [
                ("a/".into(), 0, true, 5),
                ("a/c/".into(), 1, true, 3),
                ("a/c/d.txt".into(), 2, false, 3),
                ("a/z.txt".into(), 1, false, 2),
                ("A2/".into(), 0, true, 0),
                ("b.txt".into(), 0, false, 1),
            ]
        );
    }

    #[test]
    fn zip_parents_lists_folders() {
        assert_eq!(zip_parents("a/b/c.txt").collect::<Vec<_>>(), ["a/", "a/b/"]);
        assert_eq!(zip_parents("a/b/").collect::<Vec<_>>(), ["a/"]);
        assert_eq!(zip_parents("x").count(), 0);
    }
}
