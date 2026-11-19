//! The filter pane under a pane's bar: one box that filters the folder as you type (once you
//! pause) and searches its subfolders on Enter, the kind/size/date options, and Replace with
//! (⌘R, or the chevron before the box), which renames the shown items, previewing each new name
//! in the list first.

use super::*;

/// How long typing must pause before the list is filtered.
const FILTER_DELAY: std::time::Duration = std::time::Duration::from_millis(180);

/// Path → (new name, what's wrong with it).
pub(super) type ReplaceRows = HashMap<String, (String, Option<String>)>;

/// Focuses the first element matching `sel` in pane `id`, once it's drawn.
fn focus_in(id: usize, sel: &'static str) {
    request_animation_frame(move || {
        let found = document().query_selector(&format!(".pane[data-pane=\"{id}\"] {sel}")).ok().flatten();
        if let Some(el) = found.and_then(|el| el.dyn_into::<web_sys::HtmlInputElement>().ok()) {
            let _ = el.focus();
            el.select();
        }
    });
}

impl Ctx {
    /// Shows the filter pane and puts the cursor in the search box, or with `replace`, in Replace
    /// with.
    pub(super) fn open_search(self, p: Pane, replace: bool) {
        self.active.set(p.id);
        p.filters_open.set(true);
        if replace {
            p.replacing.set(true);
        }
        focus_in(p.id, if replace { ".replace-box input" } else { ".search-box input" });
    }

    /// ⌘R: shows Replace with, or hides it when it's already there.
    pub(super) fn toggle_replace(self, p: Pane) {
        if p.filters_open.get_untracked() && p.replacing.get_untracked() {
            p.replacing.set(false);
            focus_in(p.id, ".search-box input");
        } else {
            self.open_search(p, true);
        }
    }

    /// Hides the filter pane; the folder shows everything again.
    pub(super) fn close_search(self, p: Pane) {
        p.filters_open.set(false);
        p.replacing.set(false);
        if !p.filter.with_untracked(String::is_empty) {
            p.filter.set(String::new());
            p.arrange();
        }
    }

    /// Renames the shown items as previewed: what the search box matches, replaced.
    pub(super) fn replace_all(self, p: Pane) {
        let plan: Vec<(String, String)> = match p.replace_rows.get_untracked() {
            Ok(rows) if !rows.is_empty() && rows.values().all(|(_, problem)| problem.is_none()) => {
                rows.into_iter().map(|(path, (new, _))| (path, new)).collect()
            }
            _ => return,
        };
        if !self.renamable(p) {
            return;
        }
        let src = p.src.get_untracked();
        p.loading.set(true);
        spawn_local(async move {
            let (done, err) = batch::rename_all(src, plan).await;
            p.loading.set(false);
            if let Some(e) = self.renamed(p, src, done, err) {
                p.error(e);
            }
        });
    }
}

#[component]
pub(super) fn SearchPane(pane: Pane) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let p = pane;

    // The preview of Replace with, kept up to date while it shows.
    Effect::new(move || {
        let on = p.filters_open.get() && p.replacing.get();
        let find = p.filter.get();
        let rows = if !on || find.is_empty() {
            Ok(HashMap::new())
        } else {
            let replace = p.replace.get();
            let regex = p.regex.get();
            // What else is in the folder, to catch names already taken there.
            let shown: HashSet<String> = p.entries.with(|es| es.iter().map(|e| e.path.clone()).collect());
            let others: HashSet<String> = p.all.with(|all| all.iter().filter(|e| !shown.contains(&e.path)).map(|e| e.name.to_lowercase()).collect());
            let taken = HashMap::from([(p.cwd.get(), others)]);
            p.entries.with(|es| batch::replace_plan(es, &find, &replace, regex, &taken)).map(|rows| {
                rows.into_iter().filter(|(e, new, _)| *new != e.name).map(|(e, new, problem)| (e.path, (new, problem))).collect()
            })
        };
        if p.replace_rows.with_untracked(|r| *r != rows) {
            p.replace_rows.set(rows);
        }
    });

    let toggle_regex = move |ev: leptos::ev::MouseEvent| {
        ev.stop_propagation();
        let on = !p.regex.get_untracked();
        p.regex.set(on);
        store(&format!("{REGEX_KEY}.{}", p.id), if on { "1" } else { "0" });
        p.arrange();
    };
    let changes = move || p.replace_rows.with(|r| r.as_ref().map_or(0, HashMap::len));
    let problems = move || p.replace_rows.with(|r| r.as_ref().map_or(0, |m| m.values().filter(|(_, x)| x.is_some()).count()));
    let replace_error = move || p.replace_rows.with(|r| r.as_ref().err().cloned());
    // Typing filters once it pauses: each key would otherwise go through the whole folder.
    let pending = StoredValue::new_local(None::<TimeoutHandle>);
    let cancel = move || {
        if let Some(h) = pending.try_update_value(Option::take).flatten() {
            h.clear();
        }
    };
    on_cleanup(cancel);
    // Filters by what the box has now; it may have changed since the timer started.
    let apply = move |input: &web_sys::HtmlInputElement| {
        cancel();
        let text = input.value();
        if p.filters_open.get_untracked() && p.filter.with_untracked(|f| *f != text) {
            p.filter.set(text);
            p.arrange();
        }
    };
    let typed = move |ev: leptos::ev::Event| {
        let Some(input) = ev.target().and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok()) else { return };
        cancel();
        if input.value().is_empty() {
            // Clearing shows everything right away.
            apply(&input);
        } else if let Ok(h) = set_timeout_with_handle(move || apply(&input), FILTER_DELAY) {
            let _ = pending.try_set_value(Some(h));
        }
    };
    let pick = |label: &'static str, sig: RwSignal<usize>, names: Vec<&'static str>| view! {
        <select aria-label=label prop:value=move || sig.get().to_string()
            on:change=move |e| sig.set(event_target_value(&e).parse().unwrap_or(0))>
            {names.into_iter().enumerate().map(|(i, n)| view! { <option value=i.to_string()>{n}</option> }).collect_view()}
        </select>
    };

    move || p.filters_open.get().then(|| view! {
        <div class="search-filters">
            <div class="line">
                <button class="filter-opt chevron" class:open=move || p.replacing.get() aria-expanded=move || p.replacing.get().to_string()
                    title=move || ctx.with_key(if p.replacing.get() { "Hide Replace with" } else { "Replace in names" }, "replace") aria-label="Replace"
                    on:mousedown=|ev| ev.prevent_default() on:click=move |_| ctx.toggle_replace(p)>{ico(Icon::Chevron)}</button>
                <div class="filter-box search-box" class:bad=move || p.bad_filter.with(Option::is_some)>
                    {ico(Icon::Search)}
                    <datalist id=format!("searches-{}", p.id)>
                        {move || ctx.searches.get().into_iter().map(|q| view! { <option value=q></option> }).collect_view()}
                    </datalist>
                    <input class="filter" type="search" list=format!("searches-{}", p.id) aria-label="Filter and search"
                        spellcheck="false" autocomplete="off"
                        placeholder=move || if p.regex.get() { "Filter (regex); Enter searches subfolders" } else { "Filter; Enter searches subfolders" }
                        title=move || p.bad_filter.get().unwrap_or_else(|| {
                            if p.regex.get() { "Names matching this regular expression, any case".into() } else { "Names containing this; * and ? make a pattern, like IMG_*.jpg".into() }
                        })
                        prop:value=move || p.filter.get()
                        on:input=typed
                        on:keydown=move |ev| match ev.key().as_str() {
                            "Enter" => {
                                if let Some(input) = ev.target().and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok()) {
                                    apply(&input);
                                }
                                ctx.find(p);
                            }
                            "Escape" => { cancel(); ctx.close_search(p); }
                            _ => {}
                        }/>
                    {move || (!p.filter.with(String::is_empty)).then(|| view! {
                        <button class="filter-clear" title="Clear" aria-label="Clear the filter"
                            on:mousedown=|ev| ev.prevent_default()
                            on:click=move |_| { cancel(); p.filter.set(String::new()); p.arrange(); }>"✕"</button>
                    })}
                    <button class="filter-opt regex" class:on=move || p.regex.get() aria-pressed=move || p.regex.get().to_string()
                        title=move || if p.regex.get() { "Regular expression: on" } else { "Regular expression: off" } aria-label="Regular expression"
                        on:mousedown=|ev| ev.prevent_default() on:click=toggle_regex>".*"</button>
                </div>
                {pick("Kind", p.search_kind, SEARCH_KINDS.iter().map(|x| x.0).collect())}
                {pick("Size", p.search_size, SEARCH_SIZES.iter().map(|x| x.0).collect())}
                {pick("Modified", p.search_time, SEARCH_TIMES.iter().map(|x| x.0).collect())}
                <button class="btn small primary" title="Search this folder and its subfolders" on:click=move |_| ctx.find(p)>"Search"</button>
                <button class="btn small" title="Clear the text, and back to any kind, size and time" on:click=move |_| {
                    cancel();
                    p.filter.set(String::new());
                    p.search_kind.set(0);
                    p.search_size.set(0);
                    p.search_time.set(0);
                    p.arrange();
                }>"Clear"</button>
                <span class="grow"></span>
                <button class="btn icon-only small" title="Close" aria-label="Close filter and search"
                    on:click=move |_| { cancel(); ctx.close_search(p); }>{ico(Icon::Close)}</button>
            </div>
            {move || p.replacing.get().then(|| view! {
                <div class="line">
                    <span class="chevron-room"></span>
                    <div class="filter-box replace-box">
                        <span class="replace-icon" aria-hidden="true">"⇄"</span>
                        <input class="filter" type="text" aria-label="Replace with" spellcheck="false" autocomplete="off"
                            placeholder=move || if p.regex.get() { "Replace with ($1 for groups)" } else { "Replace with" }
                            prop:value=move || p.replace.get()
                            on:input=move |ev| p.replace.set(event_target_value(&ev))
                            on:keydown=move |ev| match ev.key().as_str() {
                                "Enter" => ctx.replace_all(p),
                                "Escape" => { p.replacing.set(false); focus_in(p.id, ".search-box input"); }
                                _ => {}
                            }/>
                    </div>
                    <button class="btn small primary" disabled=move || { changes() == 0 || problems() > 0 || !ctx.renamable(p) }
                        title="Rename the shown items as previewed in the list" on:click=move |_| ctx.replace_all(p)>
                        {move || match changes() { 1 => "Rename 1 item".to_string(), n => format!("Rename {n} items") }}</button>
                    <span class="hint" class:error=move || { problems() > 0 || replace_error().is_some() }>{move || {
                        if let Some(e) = replace_error() {
                            e
                        } else if p.filter.with(String::is_empty) {
                            "Type what to find in the box above.".into()
                        } else if problems() > 0 {
                            format!("{} new name{} won't work; see the list.", problems(), if problems() == 1 { "" } else { "s" })
                        } else if changes() == 0 {
                            "No shown name changes.".into()
                        } else {
                            format!("The list shows each new name. {} undoes the renames.", ctx.key_label("undo"))
                        }
                    }}</span>
                </div>
            })}
        </div>
    })
}
