//! Find and replace across the book.

use super::*;

impl Ep {
    pub(super) async fn search(self) -> Result<(), String> {
        let q = self.query.get_untracked();
        let f = call::<Found>("epub_find", json!({ "id": self.id(), "query": q })).await?;
        self.found.set(Some(f));
        Ok(())
    }

    pub(super) fn find(self) {
        if self.query.with_untracked(|q| q.find.trim().is_empty()) {
            self.found.set(None);
            return;
        }
        self.act(move || self.search());
    }

    /// Finds once typing has stopped for a moment (or, while something else runs, after it).
    pub(super) fn find_soon(self) {
        if let Some(h) = self.find_timer.get_value() {
            h.clear();
        }
        let h = set_timeout_with_handle(
            move || {
                self.find_timer.set_value(None);
                if self.busy.get_untracked() {
                    self.find_soon();
                } else {
                    self.find();
                }
            },
            FIND_DELAY,
        );
        self.find_timer.set_value(h.ok());
    }

    /// Whether the Search tab shows (the pane beside the page is open on it).
    pub(super) fn searching(self) -> bool {
        self.outline.get() && self.finding.get()
    }

    /// Replaces the next match: in the open chapter after the last one replaced there, else in
    /// the next chapter with matches (which is opened). The matches are found again after it.
    pub(super) fn replace_next(self) {
        self.act(move || async move {
            if self.found.with_untracked(Option::is_none) {
                self.search().await?;
            }
            let q = self.query.get_untracked();
            let open = match self.view.get_untracked() {
                View::Chapter(p) => Some(p),
                View::Info => None,
            };
            let hits: Vec<(String, String)> = self.found.with_untracked(|f| {
                f.iter()
                    .flat_map(|f| f.hits.iter().map(|h| (h.path.clone(), h.label.clone())))
                    .collect()
            });
            // The open chapter first (on from the last replacement), then those after it, round.
            let k = open
                .as_ref()
                .and_then(|o| hits.iter().position(|h| h.0 == *o))
                .map_or(0, |k| k + 1);
            let mut order: Vec<(String, usize)> = vec![];
            if let Some(o) = &open {
                let from = self.replace_from.with_value(|r| {
                    r.as_ref()
                        .filter(|(rq, rp, _)| *rq == q && rp == o)
                        .map_or(0, |r| r.2)
                });
                order.push((o.clone(), from));
            }
            order.extend(
                hits[k.min(hits.len())..]
                    .iter()
                    .chain(&hits[..k.min(hits.len())])
                    .filter(|h| Some(&h.0) != open.as_ref())
                    .map(|h| (h.0.clone(), 0)),
            );
            for (path, from) in order {
                let r = call::<ReplacedOne>(
                    "epub_replace_one",
                    json!({ "id": self.id(), "query": q, "path": path, "from": from }),
                )
                .await?;
                let Some(at) = r.at else { continue };
                let label = r
                    .book
                    .chapters
                    .iter()
                    .find(|c| c.path == path)
                    .map(|c| c.label.clone())
                    .unwrap_or_default();
                self.book.set(Some(r.book));
                self.replace_from
                    .set_value(Some((q.clone(), path.clone(), at)));
                self.load_chapter(&path).await?;
                self.view.set(View::Chapter(path));
                self.search().await?;
                let left = self
                    .found
                    .with_untracked(|f| f.as_ref().map_or(0, |f| f.total));
                let left = if left == 0 {
                    "No more matches.".to_string()
                } else {
                    format!("{} left.", plural(left, "match", "matches"))
                };
                self.say(
                    format!(
                        "Replaced one in “{label}”. {left} Save to write the book to {}.",
                        self.place()
                    ),
                    "ok",
                );
                return Ok(());
            }
            self.replace_from.set_value(None);
            self.search().await?;
            self.say("No more matches to replace.", "");
            Ok(())
        });
    }

    pub(super) fn replace(self) {
        self.act(move || async move {
            let q = self.query.get_untracked();
            let r =
                call::<Replaced>("epub_replace", json!({ "id": self.id(), "query": q })).await?;
            self.book.set(Some(r.book));
            self.search().await?;
            self.say(
                format!(
                    "Replaced {}. Save to write the book to {}.",
                    plural(r.count, "match", "matches"),
                    self.place()
                ),
                "ok",
            );
            Ok(())
        });
    }
}

/// Find and replace across the book, in the Search tab beside the page: the find box with its
/// options, Replace with under it (the chevron), and the chapters it's found in below. It
/// searches as you type, once typing stops. Esc goes back to the chapters.
#[component]
pub(super) fn FindView() -> impl IntoView {
    let ep = expect_context::<Ep>();
    let confirm = RwSignal::new(false);
    let replacing = ep.replacing;
    let q = ep.query;
    let find_box = NodeRef::<html::Input>::new();
    let replace_box = NodeRef::<html::Input>::new();
    // On opening, and on ⌘F / ⌘R while open: the cursor in the find (or Replace with) box.
    Effect::new(move |_| {
        let (_, replace) = ep.find_focus.get();
        request_animation_frame(move || {
            let el = if replace {
                replace_box.get_untracked()
            } else {
                None
            }
            .or_else(|| find_box.get_untracked());
            if let Some(el) = el {
                let _ = el.focus();
                el.select();
            }
        });
    });
    let empty = move || q.with(|x| x.find.trim().is_empty());
    // An option turned on or off: found again at once.
    let opt = move |label: &'static str,
                    on: fn(&Query) -> bool,
                    flip: fn(&mut Query),
                    glyph: &'static str,
                    class: &'static str| {
        view! {
            <button class="filter-opt" class:on=move || q.with(on) aria-pressed=move || q.with(on).to_string() title=label aria-label=label
                on:mousedown=|e: web_sys::MouseEvent| e.prevent_default()
                on:click=move |_| {
                    q.update(flip);
                    confirm.set(false);
                    ep.find_soon();
                }>
                <span class=class>{glyph}</span>
            </button>
        }
    };
    let count = move || {
        ep.found.with(|f| match f {
            None => String::new(),
            Some(f) if f.total == 0 => "No matches".into(),
            Some(f) => format!(
                "{} in {}",
                plural(f.total, "match", "matches"),
                plural(f.hits.len(), "chapter", "chapters")
            ),
        })
    };
    view! {
        <div class="read-find epub-find" on:keydown=move |e| if e.key() == "Escape" { e.prevent_default(); e.stop_propagation(); ep.finding.set(false); }>
            <div class="read-find-box">
                <div class="filter-box find" class:bad=move || ep.found.with(|f| f.as_ref().is_some_and(|f| f.total == 0))>
                    {ico(Icon::Search)}
                    <input class="filter" node_ref=find_box type="text" placeholder=move || if q.with(|x| x.regex) { "Find (regex)" } else { "Find in every chapter" } aria-label="Find" spellcheck="false"
                        prop:value=move || q.with(|x| x.find.clone())
                        on:input=move |e| { q.update(|x| x.find = event_target_value(&e)); confirm.set(false); ep.find_soon(); }
                        on:keydown=move |e| if e.key() == "Enter" { e.prevent_default(); confirm.set(false); ep.find(); }/>
                    {opt("Match case", |x| x.match_case, |x| x.match_case = !x.match_case, "Aa", "")}
                    {opt("Match whole word", |x| x.whole_word, |x| x.whole_word = !x.whole_word, "ab", "word")}
                    {opt("Use regular expression", |x| x.regex, |x| x.regex = !x.regex, ".*", "")}
                </div>
                {move || replacing.get().then(|| view! {
                    <div class="filter-box with">
                        {ico(Icon::Edit)}
                        <input class="filter" node_ref=replace_box type="text" placeholder=move || if q.with(|x| x.regex) { "Replace with ($1 for groups)" } else { "Replace with" } aria-label="Replace with" spellcheck="false"
                            prop:value=move || q.with(|x| x.replace.clone())
                            on:input=move |e| { q.update(|x| x.replace = event_target_value(&e)); confirm.set(false); }
                            on:keydown=move |e| if e.key() == "Enter" && !empty() {
                                e.prevent_default();
                                if e.meta_key() || e.ctrl_key() { confirm.set(true) } else { confirm.set(false); ep.replace_next() }
                            }/>
                        <button class="filter-opt" class:on=move || q.with(|x| x.preserve_case) aria-pressed=move || q.with(|x| x.preserve_case).to_string()
                            title="Preserve case: Foo becomes Bar, FOO becomes BAR" aria-label="Preserve case"
                            on:mousedown=|e: web_sys::MouseEvent| e.prevent_default() on:click=move |_| q.update(|x| x.preserve_case = !x.preserve_case)><span>"AB"</span></button>
                    </div>
                })}
                <div class="find-tools">
                    <button class="btn icon-only chevron" class:open=replacing aria-expanded=move || replacing.get().to_string()
                        title=move || if replacing.get() { "Hide Replace with" } else { "Replace" } aria-label="Replace with"
                        on:mousedown=|e: web_sys::MouseEvent| e.prevent_default() on:click=move |_| replacing.update(|r| *r = !*r)>{ico(Icon::Chevron)}</button>
                    <span class="count">{count}</span>
                    <span class="spacer"></span>
                    {move || replacing.get().then(|| view! {
                        <button class="btn icon-only" title="Replace (Enter): the next match in this chapter, then in the chapters after it" aria-label="Replace"
                            disabled=empty on:click=move |_| { confirm.set(false); ep.replace_next(); }>{ico(Icon::Replace)}</button>
                        <button class="btn icon-only" title=crate::os::keys("Replace all… (⌘Enter)") aria-label="Replace all" disabled=empty
                            on:click=move |_| confirm.set(true)>{ico(Icon::ReplaceAll)}</button>
                    })}
                </div>
            </div>
            {move || confirm.get().then(|| {
                let x = q.get();
                view! {
                    <div class="confirm">
                        <span class="text">{format!("Replace every “{}” with “{}” in all chapters?", x.find, x.replace)}</span>
                        <button class="btn danger solid" on:click=move |_| { confirm.set(false); ep.replace(); }>"Replace all"</button>
                        <button class="btn" on:click=move |_| confirm.set(false)>"Cancel"</button>
                    </div>
                }
            })}
            {move || ep.found.get().map(|f| {
                let skipped = (!f.skipped.is_empty()).then(|| format!("Not searched, because their XHTML is broken: {}", f.skipped.join(", ")));
                view! {
                    {skipped.map(|s| view! { <p class="hint error">{s}</p> })}
                    <ul class="read-results">
                        {f.hits.into_iter().map(|h| {
                            let path = h.path.clone();
                            view! {
                                <li>
                                    <button type="button" on:click=move |_| ep.go(View::Chapter(path.clone()))>
                                        <b>{h.label}</b>" "<span class="kind">{h.count}</span>
                                        {h.snippets.into_iter().take(3).map(|s| view! {
                                            <span class="snip">"…"{s.before}<mark>{s.text}</mark>{s.after}"…"</span>
                                        }).collect_view()}
                                    </button>
                                </li>
                            }
                        }).collect_view()}
                    </ul>
                }
            })}
        </div>
    }
}
