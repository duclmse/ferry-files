//! The root components: the app's dialogs and panels around the browser, and the browser itself.

use super::*;

#[component]
pub fn App() -> impl IntoView {
    rename_stored_keys();
    let ctx = Ctx::new();
    apply_theme(&ctx.theme.get_untracked());
    provide_context(ctx);
    provide_context(VideoPrefs::load());
    let ep = ctx.ep;
    provide_context(ep);
    // Saving a book writes the file and its backup, so the panes are listed again.
    Effect::new(move |was: Option<Option<(u32, bool)>>| {
        let now = ep.book.with(|b| b.as_ref().map(|b| (b.id, b.dirty)));
        if let (Some(Some((id, true))), Some((id2, false))) = (was, now) {
            if id == id2 {
                ctx.reload_all();
            }
        }
        now
    });

    // Pick up existing device connections after a webview reload, then bring back the last
    // session's tabs (or start on the remembered folders).
    spawn_local(async move {
        if let Ok(Some(mac)) = call::<Option<Info>>("info", json!({ "src": "mac" })).await {
            ctx.mac.set(Some(mac));
        }
        for info in call::<Vec<Info>>("connections", json!({}))
            .await
            .unwrap_or_default()
        {
            ctx.connected(info, false);
        }
        ctx.restore_session();
        ctx.check_conns();
    });
    ctx.keep_session();
    ctx.watch_terminals();
    // Look for devices and disks now, then when the backend says something was plugged in or taken
    // out. Every few seconds as well while a phone waits for USB debugging to be allowed (that's
    // not a USB change), after a look that couldn't start, and, when USB can't be watched, while
    // no phone is connected.
    ctx.scan();
    ctx.reconnect_wifi();
    ctx.load_jobs();
    ctx.load_scripts();
    let watching = StoredValue::new(false);
    spawn_local(async move {
        if let Ok(on) = call::<bool>("watch_devices", json!({})).await {
            watching.set_value(on);
        }
    });
    crate::api::listen("devices-changed", move |_: ()| ctx.scan());
    // fsd saw folders on the computer change (fsd.rs `follow`).
    crate::api::listen("folders-changed", move |dirs: Vec<String>| {
        ctx.changed_outside(dirs)
    });
    let _ = set_interval_with_handle(
        move || {
            let waiting = ctx
                .devices
                .with_untracked(|d| d.as_ref().is_some_and(|d| d.adb_waiting));
            let unwatched = !watching.get_value() && !ctx.has_phone();
            if waiting || unwatched || ctx.scan_again.get_value() {
                ctx.scan();
            }
        },
        std::time::Duration::from_secs(3),
    );
    let _ = set_interval_with_handle(move || ctx.check_conns(), everything::EVERYTHING_EVERY);

    // Keyboard. ⌘S saves in the editors. Elsewhere the shortcuts are the user's (keymap.rs), but
    // for arrows, which move (←/→ also go up/in in the column view and move across in the
    // thumbnail view), and Escape, which unpicks.
    // Letting go of the forward key: a skip, or the end of 2×.
    let _keys_up = window_event_listener(ev::keyup, move |e| {
        let bound = chord_of(&e).and_then(|c| ctx.command_for(&c));
        if bound.is_some_and(|b| b.id == "media_forward")
            && media_key(ctx.act().id, MediaKey::ForwardUp)
        {
            e.prevent_default();
        }
    });
    // The mouse's Back button goes up a folder, in the pane under the pointer (or the active one).
    // Its Forward button does nothing; both are kept from the web view, which would go back a page.
    let _mouse_nav = window_event_listener(ev::mouseup, move |e| {
        if !matches!(e.button(), 3 | 4) {
            return;
        }
        e.prevent_default();
        if e.button() != 3
            || ctx.settings.get_untracked()
            || ctx.panel.get_untracked().is_some()
            || ctx.editor.get_untracked().is_some()
        {
            return;
        }
        let under = e
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .and_then(|el| el.closest("[data-pane]").ok().flatten())
            .and_then(|el| el.get_attribute("data-pane"))
            .and_then(|i| i.parse::<usize>().ok())
            .and_then(|i| ctx.panes.get(i).copied());
        let p = under.unwrap_or_else(|| ctx.act());
        ctx.active.set(p.id);
        ctx.go_up(p);
    });
    // What was clicked last, for the book editor's and reader's shortcuts when nothing there has
    // the focus (a click on a button often leaves it on the page).
    let clicked = StoredValue::new_local(None::<web_sys::Element>);
    // Type to select: what was typed, and when the last letter was.
    let typed = StoredValue::new((String::new(), 0.0_f64));
    // In the preview that holds the book editor: its bar and page, and the line above them (the
    // book's name, Read and Edit).
    let in_editor = |el: &web_sys::Element| {
        el.closest(".book-editor").ok().flatten().is_some()
            || el
                .closest(".preview")
                .ok()
                .flatten()
                .and_then(|p| p.query_selector(".book-editor").ok().flatten())
                .is_some()
    };
    let _mouse_nav_down = window_event_listener(ev::mousedown, move |e| {
        if matches!(e.button(), 3 | 4) {
            e.prevent_default();
        }
        clicked.set_value(
            e.target()
                .and_then(|t| t.dyn_into::<web_sys::Element>().ok()),
        );
    });
    // The pane whose preview is reading a book or showing a PDF, with `el` in it.
    let in_reader = |el: &web_sys::Element| {
        let preview = el.closest(".preview").ok().flatten()?;
        preview
            .query_selector(".book-reader, .pdf-reader")
            .ok()
            .flatten()?;
        preview
            .closest("[data-pane]")
            .ok()
            .flatten()?
            .get_attribute("data-pane")?
            .parse::<usize>()
            .ok()
    };
    let _keys = window_event_listener(ev::keydown, move |e| {
        let meta = e.meta_key() || e.ctrl_key();
        if ctx.menu.get_untracked().is_some() {
            if e.key() == "Escape" {
                e.prevent_default();
            }
            ctx.menu.set(None);
            if e.key() == "Escape" {
                return;
            }
        }
        if ctx.panel.get_untracked().is_some() {
            if e.key() == "Escape" {
                e.prevent_default();
                ctx.close_panel();
            }
            return;
        }
        let chord = chord_of(&e);
        let bound = chord.as_deref().and_then(|c| ctx.command_for(c));
        if ctx.settings.get_untracked() {
            // Changing a shortcut: the next key press is the new one; Esc stops waiting.
            // Giving a script its shortcut (Settings > Scripts): Esc stops, ⌫ takes it away.
            if let Some(path) = ctx.script_capturing.get_untracked() {
                if let Some(c) = chord {
                    e.prevent_default();
                    e.stop_propagation();
                    ctx.capture_script_key(path, c);
                }
                return;
            }
            if let Some(id) = ctx.capturing.get_untracked() {
                if let Some(c) = chord {
                    e.prevent_default();
                    if c != "Escape" {
                        ctx.bind(id, c);
                    }
                    ctx.capturing.set(None);
                }
                return;
            }
            if e.key() == "Escape" {
                e.prevent_default();
                ctx.settings.set(false);
            }
            return;
        }
        // In the preview's book editor: ⌘S saves the book, ⌘F finds in it and ⌘R (or ⌥⌘F)
        // replaces in it.
        // Where the keys go: what has the focus, or else what was clicked last.
        let place = || {
            document()
                .active_element()
                .filter(|el| el.tag_name() != "BODY")
                .or_else(|| clicked.with_value(Clone::clone))
        };
        if meta && matches!(e.key().as_str(), "f" | "ƒ") {
            if let Some(id) = place().and_then(|el| in_reader(&el)) {
                e.prevent_default();
                ctx.read_find.update(|(p, n)| (*p, *n) = (id, *n + 1));
                return;
            }
        }
        if meta
            && matches!(e.key().as_str(), "s" | "f" | "ƒ" | "r")
            && ep.book.with_untracked(|b| b.is_some())
        {
            let in_book =
                place().is_some_and(|el| in_editor(&el)) && ep.host.get_untracked().is_some();
            if in_book {
                e.prevent_default();
                match e.key().as_str() {
                    "s" => ep.save(),
                    "r" => ep.open_find(true),
                    _ => ep.open_find(e.alt_key()),
                }
                return;
            }
        }
        if ctx.editor.get_untracked().is_some() {
            if meta && e.key() == "s" {
                e.prevent_default();
                ctx.save();
            }
            return;
        }
        let p = ctx.act();
        // The Editor view: ⌘S saves, ⌘W closes the file's tab, and only shortcuts that work
        // anywhere do the rest; the others act on files, which aren't shown.
        if p.view.get_untracked() == ViewMode::Code
            && p.notice
                .with_untracked(|n| matches!(n, Notice::None | Notice::Ok(_) | Notice::Error(_)))
        {
            if e.default_prevented() {
                return;
            }
            let id = bound.map(|b| b.id);
            if meta && e.key() == "s" {
                e.prevent_default();
                ctx.save_in(p.slot());
            } else if meta
                && matches!(e.key().as_str(), "f" | "ƒ")
                && p.doc.with_untracked(Option::is_some)
            {
                // Find in the shown file, from anywhere in the pane (the editor's own ⌘F takes the selection).
                e.prevent_default();
                if e.alt_key() {
                    ctx.find_state.replacing.set(true);
                }
                p.doc_find.set(true);
            } else if id == Some("rename") {
                // The tree's row with the focus, or else the shown file. Not from a field: the
                // rename box is one.
                let focused = document().active_element();
                let in_field = focused.as_ref().is_some_and(|el| el.tag_name() == "INPUT");
                let row = focused
                    .and_then(|el| el.closest(".code-tree .tree-row").ok().flatten())
                    .and_then(|row| row.get_attribute("data-path"));
                let shown = || {
                    if p.peek_on.get_untracked() {
                        p.doc_peek
                            .with_untracked(|e| e.as_ref().map(|e| e.path.clone()))
                    } else {
                        p.docs.with_untracked(|ds| {
                            ds.get(p.doc_at.get_untracked())
                                .map(|d| d.path().to_string())
                        })
                    }
                };
                if !in_field {
                    if let Some(path) = row.or_else(shown) {
                        e.prevent_default();
                        p.renaming.set(Some(path));
                    }
                }
                return;
            } else if id == Some("close_tab") && p.peek_on.get_untracked() {
                e.prevent_default();
                ctx.close_peek(p);
            } else if id == Some("close_tab") && p.docs.with_untracked(|ds| !ds.is_empty()) {
                e.prevent_default();
                ctx.close_doc(p, p.doc_at.get_untracked(), false);
            } else if bound
                .is_some_and(|b| b.anywhere && !matches!(b.id, "replace" | "replace_names"))
                && chord.as_deref().is_some_and(held)
            {
                // Falls through to the shortcut below.
            } else {
                return;
            }
        }
        if !matches!(
            p.notice.get_untracked(),
            Notice::None | Notice::Ok(_) | Notice::Error(_)
        ) {
            return;
        }
        // Typing in a field (the filter) is the field's, but for shortcuts with ⌘ or ⌃ that work
        // anywhere: tabs, the palette and the sidebar.
        let typing = e
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .is_some_and(|el| el.closest("input, textarea").ok().flatten().is_some());
        if typing && !bound.is_some_and(|b| b.anywhere && chord.as_deref().is_some_and(held)) {
            return;
        }
        // A script's shortcut (Settings > Scripts) runs it on what's picked, or the folder.
        if bound.is_none() {
            if let Some(s) = chord.as_deref().and_then(|c| ctx.script_for(c)) {
                e.prevent_default();
                let items = p.picked().into_iter().map(|x| x.path).collect();
                ctx.run_script(p, s, items, false);
                return;
            }
        }
        let entries = p.shown_rows();
        let idx = p
            .selected
            .get_untracked()
            .and_then(|s| entries.iter().position(|x| x.path == s));
        let current = idx.and_then(|i| entries.get(i)).cloned();
        let view = p.view.get_untracked();
        let step = |delta: isize| {
            let last = entries.len() as isize - 1;
            let next = match idx {
                None => 0,
                Some(i) => (i as isize + delta).clamp(0, last.max(0)),
            };
            if let Some(x) = entries.get(next as usize) {
                p.select_only(Some(x.path.clone()));
            }
        };
        // Tiles per row in the thumbnail view: the grid's columns.
        let per_row = || {
            document()
                .query_selector(".pane.active .thumbs")
                .ok()
                .flatten()
                .and_then(|g| window().get_computed_style(&g).ok().flatten())
                .and_then(|s| s.get_property_value("grid-template-columns").ok())
                .map_or(1, |v| v.split_whitespace().count().max(1) as isize)
        };
        // Type to select: letters typed less than a second apart pick the item whose name starts
        // with them. While typing, they're all letters (a space too), not shortcuts.
        let key = e.key();
        let letter = (!meta && !e.ctrl_key() && !e.alt_key() && key.chars().count() == 1)
            .then_some(key.as_str());
        let now = js_sys::Date::now();
        let typing_on = typed.with_value(|(s, at)| !s.is_empty() && now - at < 1000.0);
        let type_ahead = |letter: &str| {
            let mut so_far = if typing_on {
                typed.with_value(|(s, _)| s.clone())
            } else {
                String::new()
            };
            so_far.push_str(letter);
            let names: Vec<&str> = entries.iter().map(|x| x.name.as_str()).collect();
            if let Some(i) = typed_match(&names, idx, &so_far) {
                p.select_only(Some(entries[i].path.clone()));
            }
            typed.set_value((so_far, now));
        };
        if let Some(l) = letter.filter(|_| typing_on) {
            e.prevent_default();
            type_ahead(l);
            return;
        }
        if let Some(b) = bound {
            let done = match b.id {
                "palette" => {
                    ctx.panel.set(Some(Panel::Palette("")));
                    true
                }
                "find_files" => {
                    ctx.panel.set(Some(Panel::Palette(":")));
                    true
                }
                "new_tab" => {
                    ctx.new_tab(p, p.src.get_untracked(), p.cwd.get_untracked());
                    true
                }
                // The last tab is left to the window (the menu's Close Window).
                "close_tab" if p.tabs.with_untracked(Vec::len) > 1 => {
                    ctx.close_tab(p, p.tab.get_untracked());
                    true
                }
                "next_tab" | "prev_tab" => {
                    ctx.step_tab(p, if b.id == "next_tab" { 1 } else { -1 });
                    true
                }
                "sidebar" => {
                    ctx.set_side(!ctx.side.get_untracked());
                    true
                }
                "other_pane" if ctx.split.get_untracked() => {
                    ctx.active.set(1 - p.id);
                    true
                }
                "split" => {
                    ctx.set_split(!ctx.split.get_untracked());
                    true
                }
                "hidden" => {
                    ctx.set_show_hidden(!ctx.show_hidden.get_untracked());
                    true
                }
                "terminal" => {
                    ctx.toggle_terminal(p);
                    true
                }
                "copy" | "move" => {
                    ctx.send_to_other(p, b.id == "move");
                    true
                }
                // Not while text is selected (in a preview): ⌘C copies that.
                "copy_items" | "cut_items" if !text_selected() => {
                    ctx.copy_items(p, b.id == "cut_items");
                    true
                }
                "paste" => {
                    ctx.paste(p, None);
                    true
                }
                "rename" => {
                    ctx.ask_rename(p);
                    true
                }
                "replace_names" => {
                    ctx.replace_names(p);
                    true
                }
                "undo" => {
                    ctx.undo_last();
                    true
                }
                "pick_all" if select_all_peek() => true,
                "pick_all" => {
                    p.marked
                        .set(entries.iter().map(|x| x.path.clone()).collect());
                    true
                }
                "quick_look" => {
                    ctx.quick_look(p);
                    true
                }
                "media_play" => media_key(p.id, MediaKey::Play),
                "media_back" => media_key(p.id, MediaKey::Back),
                // Held: once, then repeats, which the player ignores until it's let go.
                "media_forward" => e.repeat() || media_key(p.id, MediaKey::ForwardDown),
                "media_mute" => media_key(p.id, MediaKey::Mute),
                "pdf_actual" => pdf_fit_key(p.id, Fit::Actual),
                "pdf_fit_width" => pdf_fit_key(p.id, Fit::Width),
                "pdf_fit_page" => pdf_fit_key(p.id, Fit::Page),
                "filter" => {
                    ctx.open_search(p, false);
                    true
                }
                "replace" => {
                    ctx.toggle_replace(p);
                    true
                }
                "delete" => {
                    ctx.ask_delete(p, false);
                    true
                }
                "delete_forever" => {
                    ctx.ask_delete(p, true);
                    true
                }
                "help" => {
                    ctx.show_shortcuts();
                    true
                }
                "type_path" => {
                    p.editing_path.set(true);
                    true
                }
                "open" => match current.clone() {
                    Some(x) => {
                        ctx.open(p, x);
                        true
                    }
                    None => false,
                },
                "up" => {
                    ctx.go_up(p);
                    true
                }
                _ => false,
            };
            if done {
                e.prevent_default();
                return;
            }
        }
        if let Some(l) = letter.filter(|l| *l != " ") {
            e.prevent_default();
            type_ahead(l);
            return;
        }
        // Moving around isn't changeable.
        match (e.key().as_str(), meta) {
            ("Escape", false) => p.marked.set(HashSet::new()),
            // In the list, ← and → go to the previous and next picture, video or sound.
            ("ArrowRight", false) | ("ArrowLeft", false) if view == ViewMode::List => {
                if let Some(x) = current.clone().filter(|x| media(x).is_some()) {
                    e.prevent_default();
                    step_media(p, &x, if e.key() == "ArrowRight" { 1 } else { -1 });
                } else if let Some(x) = current.clone().filter(|_| untrack(|| p.tree_on())) {
                    e.prevent_default();
                    ctx.tree_key(p, &x, e.key() == "ArrowRight", &entries);
                }
            }
            ("ArrowDown", false) => {
                e.prevent_default();
                step(if view == ViewMode::Thumbs && idx.is_some() {
                    per_row()
                } else {
                    1
                });
            }
            ("ArrowUp", false) => {
                e.prevent_default();
                step(if view == ViewMode::Thumbs {
                    -per_row()
                } else {
                    -1
                });
            }
            ("ArrowRight", false) if view == ViewMode::Thumbs => {
                e.prevent_default();
                step(1);
            }
            ("ArrowLeft", false) if view == ViewMode::Thumbs => {
                e.prevent_default();
                step(-1);
            }
            ("ArrowRight", false) if view == ViewMode::Columns => {
                if let Some(x) = current.filter(|x| x.dir) {
                    e.prevent_default();
                    ctx.navigate(p, x.path);
                }
            }
            ("ArrowLeft", false) if view == ViewMode::Columns => {
                e.prevent_default();
                ctx.go_up(p);
            }
            _ => {}
        }
    });

    // The context menu closes on any click outside it, and when the window loses focus.
    let _press = window_event_listener(ev::mousedown, move |e| {
        let inside = e
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .is_some_and(|el| el.closest(".menu").ok().flatten().is_some());
        if !inside {
            ctx.menu.set(None);
        }
    });
    let _blur = window_event_listener(ev::blur, move |_| ctx.menu.set(None));
    // Back from Finder, maybe with files copied there to paste.
    let _focus = window_event_listener(ev::focus, move |_| ctx.check_clipboard());
    ctx.check_clipboard();
    let _resize = window_event_listener(ev::resize, move |_| ctx.menu.set(None));
    // No browser menu (Reload, Inspect…) outside text fields; the panes have their own.
    let _ctx_menu = window_event_listener(ev::contextmenu, move |e| {
        let editable = e
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .is_some_and(|el| el.closest("input, textarea").ok().flatten().is_some());
        if !editable {
            e.prevent_default();
        }
    });

    crate::api::listen("script", move |s: crate::api::ScriptProgress| {
        ctx.script_live.update(|(lines, changed)| {
            lines.extend(s.lines);
            let over = lines.len().saturating_sub(super::scripts::LIVE_MAX);
            lines.drain(..over);
            *changed = s.changed;
        })
    });
    crate::api::listen("script_ask", move |q: crate::api::ScriptQuestion| {
        ctx.script_ask.set(Some(q))
    });
    crate::api::listen("dupes", move |d: DupeProgress| {
        ctx.dupe_progress.set(Some((d.job, d.text)))
    });
    crate::api::listen("transfer", move |pr: Progress| {
        ctx.jobs.update(|js| {
            if let Some(j) = js.iter_mut().find(|j| j.id == pr.job) {
                j.progress = Some(pr);
            }
        });
    });
    // Files dragged in from the file manager are copied to the folder (or pane) they're dropped on,
    // except subtitles dropped on a video.
    crate::api::listen("tauri://drag-drop", move |d: FileDrop| {
        let dpr = window().device_pixel_ratio().max(1.0);
        let Some(el) =
            document().element_from_point((d.position.x / dpr) as f32, (d.position.y / dpr) as f32)
        else {
            return;
        };
        // Subtitle files dropped on a video are shown with it.
        if video::drop_on_player(
            &el,
            Src::Mac,
            d.paths.iter().map(|f| crate::os::app_path(f)).collect(),
        ) {
            return;
        }
        let Some(pane_el) = el.closest("[data-pane]").ok().flatten() else {
            return;
        };
        let Some(p) = pane_el
            .get_attribute("data-pane")
            .and_then(|i| i.parse::<usize>().ok())
            .and_then(|i| ctx.panes.get(i).copied())
        else {
            return;
        };
        let dir = el
            .closest("[data-dir]")
            .ok()
            .flatten()
            .and_then(|f| f.get_attribute("data-dir"))
            .unwrap_or_else(|| p.cwd.get_untracked());
        ctx.active.set(p.id);
        let paths = d.paths.iter().map(|f| crate::os::app_path(f)).collect();
        ctx.transfer(p, Src::Mac, paths, p.src.get_untracked(), dir, false);
    });

    // Keep the item last picked visible when the keyboard moves the selection. That item, not
    // the first one picked: with several picked, ⌘-clicking one mustn't scroll to another. Only a
    // pane whose selection (or view) changed scrolls: clicking into a pane makes it active first,
    // and scrolling then would move the list to the old selection under the pointer.
    Effect::new(move |prev: Option<[(Option<String>, ViewMode); 2]>| {
        let now = ctx.panes.map(|p| (p.selected.get(), p.view.get()));
        for (i, (path, _)) in now.iter().enumerate() {
            let Some(path) = path.clone() else { continue };
            if prev.as_ref().is_some_and(|prev| prev[i] == now[i]) {
                continue;
            }
            request_animation_frame(move || {
                let Ok(found) =
                    document().query_selector_all(&format!(".pane[data-pane=\"{i}\"] [data-path]"))
                else {
                    return;
                };
                let el = (0..found.length())
                    .filter_map(|n| {
                        found
                            .item(n)
                            .and_then(|e| e.dyn_into::<web_sys::Element>().ok())
                    })
                    .find(|e| e.get_attribute("data-path").as_deref() == Some(path.as_str()));
                if let Some(el) = el {
                    let opts = web_sys::ScrollIntoViewOptions::new();
                    opts.set_block(web_sys::ScrollLogicalPosition::Nearest);
                    opts.set_inline(web_sys::ScrollLogicalPosition::Nearest);
                    el.scroll_into_view_with_scroll_into_view_options(&opts);
                } else if let Some(sc) = document()
                    .query_selector(&format!(".pane[data-pane=\"{i}\"] [data-rows]"))
                    .ok()
                    .flatten()
                {
                    // Not rendered, being out of view in a long list: scroll to where it would be.
                    let rows = untrack(|| ctx.panes[i].list_rows());
                    let at = rows
                        .iter()
                        .position(|r| r.0.as_ref().is_some_and(|e| e.path == path));
                    let at = match ctx.panes[i].view.get_untracked() {
                        ViewMode::List => at,
                        _ => ctx.panes[i]
                            .shown_rows()
                            .iter()
                            .position(|e| e.path == path),
                    };
                    if let Some(idx) = at {
                        rows::reveal(i, &sc, idx);
                    }
                }
            });
        }
        now
    });

    view! {
        <Browser/>
        <ContextMenu/>
        {move || ctx.settings.get().then(|| view! { <Settings/> })}
        {move || ctx.wifi.get().then(|| view! { <WifiDialog/> })}
        {move || ctx.panel.get().map(|w| match w {
            Panel::Backups => view! { <Backups/> }.into_any(),
            Panel::History => view! { <History/> }.into_any(),
            Panel::Rename(pane, items) => view! { <BatchRename pane=pane items=items/> }.into_any(),
            Panel::Dupes(folders) => view! { <Duplicates folders=folders/> }.into_any(),
            Panel::Import(pane, from, targets) => view! { <ImportPhotos pane from targets/> }.into_any(),
            Panel::Library(src, dir) => view! { <Library src=src dir=dir/> }.into_any(),
            Panel::Palette(start) => view! { <Palette start=start/> }.into_any(),
        })}
    }
}

#[component]
fn Browser() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    // Only opening and closing the editor rebuilds the main area, not saving in it.
    let editing = Memo::new(move |_| ctx.editor.with(Option::is_some));
    move || {
        view! {
            <div class="app browse" class:no-side=move || !ctx.side.get() style=move || match ctx.side_w.get() {
                w if w > 0.0 && ctx.side.get() => format!("grid-template-columns: auto {}px auto 1fr", w.max(SIDE_MIN)),
                _ => String::new(),
            }>
                <ActivityBar/>
                {move || ctx.side.get().then(|| view! {
                    <Sidebar/>
                    <Grip size=ctx.side_w key=SIDE_W_KEY.into() min=SIDE_MIN max=480.0 room=480.0 class="grip side-grip"/>
                })}
                <div class="main">
                    {move || if editing.get() { view! { <TextEditor file=ctx.full_slot()/> }.into_any() } else { view! {
                        <div class="panes">
                            <PaneView pane=ctx.panes[0]/>
                            {move || ctx.split.get().then(|| view! {
                                <Grip size=ctx.pane_w key=PANE_W_KEY.into() min=240.0 room=RIGHT_PANE_MIN class="grip pane-grip"/>
                                <PaneView pane=ctx.panes[1]/>
                            })}
                        </div>
                    }.into_any() }}
                    <TransferBar/>
                </div>
            </div>
        }
    }
}
