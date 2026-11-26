//! The Chapters and Contents tabs beside the editor.

use super::*;

impl Ep {
    /// The Chapters (or Contents) tab: opens the pane on it, or closes the pane when it's showing.
    pub(super) fn show_chapters(self) {
        if self.outline.get_untracked() && !self.finding.get_untracked() {
            self.toggle_outline();
        } else {
            self.finding.set(false);
            if !self.outline.get_untracked() {
                self.toggle_outline();
            }
        }
    }

    /// Opens an entry of the contents: its chapter, at its place.
    pub(super) fn open_entry(self, path: String, fragment: Option<String>) {
        let here = self
            .view
            .with_untracked(|v| matches!(v, View::Chapter(p) if *p == path));
        if here {
            if let Some(f) = fragment {
                self.scroll_to(&f);
            }
        } else {
            self.anchor.set_value(fragment);
            self.go(View::Chapter(path));
        }
    }

    pub(super) fn scroll_to(self, id: &str) {
        if let Some(el) = self.frame_doc().and_then(|d| d.get_element_by_id(id)) {
            el.scroll_into_view_with_bool(true);
        }
    }

    /// Adds the cursor's place in the open chapter to the contents (see `epub_toc_add`).
    pub(super) fn toc_add(self, label: String, level: &'static str) {
        let View::Chapter(path) = self.view.get_untracked() else {
            return self.say(
                "Open a chapter and click where the entry should go first.",
                "error",
            );
        };
        let cut = match self.cut(true) {
            Ok(c) => c,
            Err(e) => return self.say(e, "error"),
        };
        self.act(move || async move {
            let mut args = cut;
            args["id"] = json!(self.id());
            args["path"] = json!(path);
            args["label"] = json!(label);
            args["level"] = json!(level);
            let b = call::<Book>("epub_toc_add", args).await?;
            let added = b.toc.len()
                > self
                    .book
                    .with_untracked(|o| o.as_ref().map_or(0, |o| o.toc.len()));
            self.book.set(Some(b));
            // The place may have been given an id.
            self.load_chapter(&path).await?;
            if added {
                self.say(
                    format!(
                        "Added to the contents. Save to write the book to {}.",
                        self.place()
                    ),
                    "ok",
                );
            }
            Ok(())
        });
    }

    pub(super) fn toc_remove(self, index: usize) {
        self.act(move || async move {
            let label = self.book.with_untracked(|b| {
                b.as_ref()
                    .and_then(|b| b.toc.get(index))
                    .map(|e| e.label.clone())
                    .unwrap_or_default()
            });
            let b = call::<Book>(
                "epub_toc_remove",
                json!({ "id": self.id(), "index": index }),
            )
            .await?;
            self.book.set(Some(b));
            self.say(
                format!(
                    "Took “{label}” out of the contents. Save to write the book to {}.",
                    self.place()
                ),
                "ok",
            );
            Ok(())
        });
    }
}

/// The table of contents, in its tab beside the editor: its entries as a tree (a click opens
/// one; ✕ takes it out), and a form that adds the cursor's place as a new entry.
#[component]
pub(super) fn ContentsView() -> impl IntoView {
    let ep = expect_context::<Ep>();
    let label = RwSignal::new(String::new());
    // As the EPUB nests its contents: beside the entry before it, a level up, or under it.
    let level = RwSignal::new("chapter");
    let in_chapter = move || ep.view.with(|v| matches!(v, View::Chapter(_)));
    let levels: [(&'static str, &'static str, &'static str); 3] = [
        (
            "section",
            "Section",
            "A level up from the entry before it (a part, say, holding what follows)",
        ),
        ("chapter", "Chapter", "At the level of the entry before it"),
        (
            "subsection",
            "Subsection",
            "A level in, under the entry before it",
        ),
    ];
    let add = move || {
        ep.toc_add(label.get_untracked(), level.get_untracked());
        label.set(String::new());
    };
    let entries = move || {
        ep.book
            .with(|b| b.as_ref().map(|b| b.toc.clone()).unwrap_or_default())
    };
    let here = move |path: &str, frag: &Option<String>| {
        ep.view.with(|v| matches!(v, View::Chapter(p) if p == path)) && frag.is_none()
    };
    view! {
        <div class="toc-view">
            <form class="toc-add" on:submit=move |e| { e.prevent_default(); add(); }>
                <input type="text" class="toc-label" aria-label="Name of the new entry" spellcheck="false"
                    placeholder="New entry: the paragraph's text, or a name"
                    prop:value=move || label.get() on:input=move |e| label.set(event_target_value(&e))
                    on:keydown=|e| e.stop_propagation()/>
                <div class="toc-add-row">
                    <div class="seg toc-levels" role="radiogroup" aria-label="Level">
                        {levels.into_iter().map(|(v, name, tip)| view! {
                            <button type="button" role="radio" class:active=move || level.get() == v aria-checked=move || (level.get() == v).to_string()
                                title=tip on:click=move |_| level.set(v)>{name}</button>
                        }).collect_view()}
                    </div>
                    <button type="submit" class="btn icon-only" disabled=move || !in_chapter()
                        title=move || if in_chapter() { "Add the cursor's place to the contents" } else { "Open a chapter and click where the entry should go" }
                        aria-label="Add to the contents">{ico(Icon::Plus)}</button>
                </div>
            </form>
            {move || {
                let list = entries();
                if list.is_empty() {
                    return view! { <p class="hint">"The contents are empty. Click in a chapter where an entry should go, then Add."</p> }.into_any();
                }
                view! {
                    <ul class="toc-tree" role="tree">
                        {list.into_iter().enumerate().map(|(k, e)| {
                            let (path, frag) = (e.path.clone(), e.fragment.clone());
                            let (p2, f2) = (e.path.clone(), e.fragment.clone());
                            view! {
                                <li role="treeitem" class="toc-entry" style=format!("--depth: {}", e.depth - 1)>
                                    <button class="pdf-head" class:here=move || here(&p2, &f2) title=e.label.clone()
                                        on:click=move |_| ep.open_entry(path.clone(), frag.clone())>
                                        <span class="name">{e.label.clone()}</span>
                                    </button>
                                    <button class="ch-rename" title="Take this entry out of the contents" aria-label="Take out of the contents"
                                        on:click=move |_| ep.toc_remove(k)>{ico(Icon::Close)}</button>
                                </li>
                            }
                        }).collect_view()}
                    </ul>
                }.into_any()
            }}
        </div>
    }
}

/// A chapter in the outline beside the editor: opens it, and its name can be changed there
/// (double-click, or the pencil).
#[component]
pub(super) fn OutlineChapter(path: String, label: String) -> impl IntoView {
    let ep = expect_context::<Ep>();
    let path = StoredValue::new(path);
    let label = StoredValue::new(label);
    let editing = move || {
        ep.renaming
            .with(|r| path.with_value(|p| r.as_ref() == Some(p)))
    };
    let here = move || {
        ep.view
            .with(|v| matches!(v, View::Chapter(p) if path.with_value(|q| q == p)))
    };
    move || {
        if editing() {
            let input = NodeRef::<html::Input>::new();
            input.on_load(move |el| {
                request_animation_frame(move || {
                    let _ = el.focus();
                    el.select();
                });
            });
            let done = move |keep: bool| {
                if !editing() {
                    return;
                }
                match input.get_untracked().filter(|_| keep) {
                    Some(el) => ep.rename_chapter(path.get_value(), el.value()),
                    None => ep.renaming.set(None),
                }
            };
            view! {
                <input class="ch-name" node_ref=input type="text" aria-label="Chapter name" prop:value=label.get_value()
                    on:keydown=move |e| {
                        e.stop_propagation();
                        match e.key().as_str() {
                            "Enter" => done(true),
                            "Escape" => done(false),
                            _ => {}
                        }
                    }
                    on:blur=move |_| done(true)/>
            }
            .into_any()
        } else {
            let name = label.get_value();
            view! {
                <div class="epub-ch" class:here=here>
                    <button class="pdf-head" class:here=here title=name.clone() on:click=move |_| ep.go(View::Chapter(path.get_value()))
                        on:dblclick=move |_| ep.start_rename(path.get_value())>
                        <span class="name">{name.clone()}</span>
                    </button>
                    <button class="ch-rename" title="Rename this chapter" aria-label="Rename this chapter" disabled=move || ep.busy.get()
                        on:click=move |_| ep.start_rename(path.get_value())>{ico(Icon::Rename)}</button>
                </div>
            }
            .into_any()
        }
    }
}
