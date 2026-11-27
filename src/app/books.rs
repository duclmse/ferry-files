//! Books: checking one, and the library and reader windows.

use super::*;

impl Ctx {
    /// Checks one book and says what's wrong with it.
    pub(super) fn check_book(self, p: Pane, e: Entry) {
        let src = p.src.get_untracked();
        p.notice.set(Notice::Ok(format!("Checking {}…", e.name)));
        spawn_local(async move {
            match call::<Vec<Issue>>("book_check", json!({ "src": src.key(), "path": e.path }))
                .await
            {
                Ok(is) if is.is_empty() => p
                    .notice
                    .set(Notice::Ok(format!("{}: no problems found.", e.name))),
                Ok(is) => {
                    let serious = is.iter().any(|i| i.serious);
                    let mut lines = vec![format!(
                        "{}: {}",
                        e.name,
                        if serious {
                            "readers may refuse or misshow this book."
                        } else {
                            "only minor issues."
                        }
                    )];
                    lines.extend(
                        is.into_iter()
                            .map(|i| format!("{} {}", if i.serious { "•" } else { "◦" }, i.text)),
                    );
                    let msg = lines.join("\n");
                    p.notice.set(if serious {
                        Notice::Error(msg)
                    } else {
                        Notice::Ok(msg)
                    });
                }
                Err(err) => p.error(err),
            }
        });
    }
}

/// Runs `f` on each book in turn, showing "Checking 3 of 10: name" and how far along it is (a
/// percentage) in `busy`, until done or stopped.
pub(super) async fn for_each_book<F, Fut>(
    busy: RwSignal<Option<(String, f64)>>,
    stop: StoredValue<bool>,
    verb: &str,
    books: Vec<Entry>,
    f: F,
) -> usize
where
    F: Fn(Entry) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    stop.set_value(false);
    let n = books.len();
    let mut done = 0;
    for (k, e) in books.into_iter().enumerate() {
        if stop.try_get_value().unwrap_or(true) {
            break;
        }
        busy.try_set(Some((
            format!("{verb} {} of {n}: {}", k + 1, e.name),
            k as f64 * 100.0 / n as f64,
        )));
        f(e).await;
        done += 1;
    }
    busy.try_set(None);
    done
}

/// Splits "Ann Smith; Bob Jones" into names.
pub(super) fn names(s: &str) -> Vec<String> {
    s.split(';')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum BookForm {
    Info,
    Shrink,
}

/// Every EPUB under a folder, with covers and info, and work on many at once: check, change info,
/// make smaller copies.
#[component]
pub(super) fn Library(src: Src, dir: String) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let dir = StoredValue::new(dir);
    let list = RwSignal::new(None::<Result<(Vec<Entry>, bool), String>>);
    let infos = RwSignal::new(HashMap::<String, BookInfo>::new());
    let notes = RwSignal::new(HashMap::<String, (String, bool)>::new());
    let issues = RwSignal::new(HashMap::<String, Vec<Issue>>::new());
    let shown_issues = RwSignal::new(None::<String>);
    let picked = RwSignal::new(std::collections::HashSet::<String>::new());
    let filter = RwSignal::new(String::new());
    let busy = RwSignal::new(None::<(String, f64)>);
    let stop = StoredValue::new(false);
    let form = RwSignal::new(None::<BookForm>);
    let message = RwSignal::new(None::<(String, bool)>);
    let (authors, language, publisher, subjects) = (
        RwSignal::new(String::new()),
        RwSignal::new(String::new()),
        RwSignal::new(String::new()),
        RwSignal::new(String::new()),
    );
    let (side, quality) = (
        RwSignal::new("1600".to_string()),
        RwSignal::new("80".to_string()),
    );

    let load = move || {
        spawn_local(async move {
            #[derive(serde::Deserialize)]
            struct Matches {
                entries: Vec<Entry>,
                complete: bool,
            }
            let args = json!({ "src": src.key(), "dir": dir.get_value(), "hidden": ctx.show_hidden.get_untracked() });
            match call::<Matches>("library", args).await {
                Ok(m) => {
                    list.try_set(Some(Ok((m.entries.clone(), m.complete))));
                    for e in m.entries {
                        if stop.try_get_value().is_none() {
                            return;
                        }
                        if infos
                            .try_with_untracked(|i| i.contains_key(&e.path))
                            .unwrap_or(true)
                        {
                            continue;
                        }
                        let args = json!({ "src": src.key(), "path": e.path, "size": e.size, "mtime": e.mtime });
                        let info =
                            call::<BookInfo>("book_info", args)
                                .await
                                .unwrap_or_else(|err| BookInfo {
                                    problem: Some(err),
                                    ..Default::default()
                                });
                        infos.try_update(|i| {
                            i.insert(e.path.clone(), info);
                        });
                    }
                }
                Err(e) => {
                    list.try_set(Some(Err(e)));
                }
            }
        })
    };
    load();

    let shown = move || -> Vec<Entry> {
        let f = filter.get().to_lowercase();
        let Some(Ok((all, _))) = list.get() else {
            return vec![];
        };
        infos.with(|i| {
            all.into_iter()
                .filter(|e| {
                    f.is_empty()
                        || e.name.to_lowercase().contains(&f)
                        || i.get(&e.path).is_some_and(|b| {
                            b.title.to_lowercase().contains(&f)
                                || b.creators.iter().any(|c| c.to_lowercase().contains(&f))
                        })
                })
                .collect()
        })
    };
    let targets = move || -> Vec<Entry> {
        let p = picked.get_untracked();
        list.get_untracked()
            .and_then(|l| l.ok())
            .map(|(all, _)| all.into_iter().filter(|e| p.contains(&e.path)).collect())
            .unwrap_or_default()
    };
    let n_picked = move || picked.with(|p| p.len());
    let note = move |path: &str, text: String, ok: bool| {
        notes.try_update(|n| {
            n.insert(path.to_string(), (text, ok));
        });
    };

    let check = move || {
        let books = targets();
        message.set(None);
        spawn_local(async move {
            let n = for_each_book(busy, stop, "Checking", books, |e| async move {
                match call::<Vec<Issue>>("book_check", json!({ "src": src.key(), "path": e.path }))
                    .await
                {
                    Ok(is) => {
                        let serious = is.iter().filter(|i| i.serious).count();
                        let text = match (serious, is.len()) {
                            (_, 0) => "No problems found.".to_string(),
                            (0, n) => {
                                format!("{n} minor {}.", if n == 1 { "issue" } else { "issues" })
                            }
                            (s, n) => format!(
                                "{s} {} (and {} minor).",
                                if s == 1 { "problem" } else { "problems" },
                                n - s
                            ),
                        };
                        note(&e.path, text, serious == 0);
                        issues.try_update(|m| {
                            m.insert(e.path.clone(), is);
                        });
                    }
                    Err(err) => note(&e.path, err, false),
                }
            })
            .await;
            message.try_set(Some((
                format!("Checked {n} {}.", if n == 1 { "book" } else { "books" }),
                true,
            )));
        });
    };

    let change = move || {
        let mut c = serde_json::Map::new();
        let a = names(&authors.get_untracked());
        if !a.is_empty() {
            c.insert("creators".into(), json!(a));
        }
        for (k, v) in [("language", language), ("publisher", publisher)] {
            let v = v.get_untracked().trim().to_string();
            if !v.is_empty() {
                c.insert(k.into(), json!(v));
            }
        }
        let s = names(&subjects.get_untracked().replace(',', ";"));
        if !s.is_empty() {
            c.insert("add_subjects".into(), json!(s));
        }
        if c.is_empty() {
            message.set(Some((
                "Fill in at least one field. Empty fields are left as they are.".into(),
                false,
            )));
            return;
        }
        let books = targets();
        form.set(None);
        message.set(None);
        let c = serde_json::Value::Object(c);
        spawn_local(async move {
            let changed = std::rc::Rc::new(std::cell::Cell::new(0));
            let c2 = changed.clone();
            let n = for_each_book(busy, stop, "Changing", books, move |e| {
                let (c, changed) = (c.clone(), c2.clone());
                async move {
                    match call::<serde_json::Value>(
                        "book_change",
                        json!({ "src": src.key(), "path": e.path, "change": c }),
                    )
                    .await
                    {
                        Ok(r) if r["changed"] == json!(true) => {
                            changed.set(changed.get() + 1);
                            note(
                                &e.path,
                                "Info changed. The old file is backed up.".into(),
                                true,
                            );
                        }
                        Ok(_) => note(&e.path, "Already had this info.".into(), true),
                        Err(err) => note(&e.path, err, false),
                    }
                }
            })
            .await;
            let k = changed.get();
            if k > 0 {
                ctx.log(format!(
                    "Changed the info of {k} {} in {}",
                    if k == 1 { "book" } else { "books" },
                    dir.try_get_value().unwrap_or_default()
                ));
            }
            message.try_set(Some((
                format!(
                    "Changed {k} of {n} {}.",
                    if n == 1 { "book" } else { "books" }
                ),
                true,
            )));
            if stop.try_get_value().is_some() {
                infos.update(|i| i.clear());
                load();
            }
        });
    };

    let shrink = move || {
        let max_side: u32 = side.get_untracked().trim().parse().unwrap_or(1600);
        let quality: u8 = quality.get_untracked().trim().parse().unwrap_or(80);
        let books = targets();
        form.set(None);
        message.set(None);
        spawn_local(async move {
            #[derive(serde::Deserialize)]
            struct Shrunk {
                before: u64,
                after: u64,
                images: u32,
                path: Option<String>,
            }
            let made = std::rc::Rc::new(std::cell::Cell::new(0));
            let m2 = made.clone();
            let n = for_each_book(busy, stop, "Shrinking", books, move |e| {
                let made = m2.clone();
                async move {
                    let args = json!({ "src": src.key(), "path": e.path, "max_side": max_side, "quality": quality });
                    match call::<Shrunk>("book_shrink", args).await {
                        Ok(Shrunk { before, after, images, path: Some(to) }) => {
                            made.set(made.get() + 1);
                            note(
                                &e.path,
                                format!(
                                    "{} → {} ({images} {} made smaller): saved as {}.",
                                    human_size(before),
                                    human_size(after),
                                    if images == 1 { "image" } else { "images" },
                                    base(&to)
                                ),
                                true,
                            );
                            ctx.changed(src, &parent(&to));
                        }
                        Ok(_) => note(&e.path, "Already as small as it gets: no copy made.".into(), true),
                        Err(err) => note(&e.path, err, false),
                    }
                }
            })
            .await;
            let k = made.get();
            if k > 0 {
                ctx.log(format!(
                    "Made smaller copies of {k} {}",
                    if k == 1 { "book" } else { "books" }
                ));
            }
            message.try_set(Some((
                format!(
                    "Made {k} smaller {} from {n}. The books themselves are unchanged.",
                    if k == 1 { "copy" } else { "copies" }
                ),
                true,
            )));
            if stop.try_get_value().is_some() {
                load();
            }
        });
    };

    let close = move || ctx.close_panel();
    let field = |label: &'static str, hint: &'static str, sig: RwSignal<String>| {
        view! {
            <label class="field"><span>{label}</span>
                <input placeholder=hint prop:value=move || sig.get() on:input=move |ev| sig.set(event_target_value(&ev))/></label>
        }
    };
    let place = ctx.place(src);
    view! {
        <div class="overlay" on:click=move |_| close()>
            <div class="dialog library" on:click=|e| e.stop_propagation()>
                <h2>"Books in " {base(&dir.get_value())} <span class="about">{format!(" on {place}, with its subfolders")}</span></h2>
                <div class="lib-bar">
                    <input class="filter-wide" type="search" placeholder="Filter by title, author or file name"
                        prop:value=move || filter.get() on:input=move |ev| filter.set(event_target_value(&ev))/>
                    <button class="btn" on:click=move |_| {
                        let all: std::collections::HashSet<String> = shown().into_iter().map(|e| e.path).collect();
                        picked.update(|p| if all.iter().all(|x| p.contains(x)) { p.clear() } else { p.extend(all) });
                    }>{move || if n_picked() > 0 { "Pick none" } else { "Pick all" }}</button>
                    <span class="grow"></span>
                    <span class="about">{move || match n_picked() { 0 => "Pick books to work on".to_string(), n => format!("{n} picked") }}</span>
                    <button class="btn" disabled=move || n_picked() == 0 || busy.get().is_some() on:click=move |_| check()>{ico(Icon::Check)}"Check"</button>
                    <button class="btn" disabled=move || n_picked() == 0 || busy.get().is_some()
                        on:click=move |_| form.set(if form.get_untracked() == Some(BookForm::Info) { None } else { Some(BookForm::Info) })>{ico(Icon::Edit)}"Change info…"</button>
                    <button class="btn" disabled=move || n_picked() == 0 || busy.get().is_some()
                        on:click=move |_| form.set(if form.get_untracked() == Some(BookForm::Shrink) { None } else { Some(BookForm::Shrink) })>"Make smaller…"</button>
                </div>
                {move || form.get().map(|f| match f {
                    BookForm::Info => view! {
                        <div class="lib-form">
                            <p class="hint">{format!("Sets these on the {} picked books; empty fields stay as they are. Each book is backed up first.", n_picked())}</p>
                            {field("Authors", "Ann Smith; Bob Jones (replaces the authors)", authors)}
                            {field("Language", "en", language)}
                            {field("Publisher", "", publisher)}
                            {field("Add subjects", "Fiction; Poetry", subjects)}
                            <div class="actions"><span class="grow"></span>
                                <button class="btn" on:click=move |_| form.set(None)>"Cancel"</button>
                                <button class="btn primary" on:click=move |_| change()>"Change"</button></div>
                        </div>
                    }.into_any(),
                    BookForm::Shrink => view! {
                        <div class="lib-form">
                            <p class="hint">"Saves “… (smaller).epub” next to each book, with big pictures scaled down and photos at the quality below. The books aren't changed."</p>
                            {field("Longest side of pictures (pixels)", "1600", side)}
                            {field("Photo quality (30–95)", "80", quality)}
                            <div class="actions"><span class="grow"></span>
                                <button class="btn" on:click=move |_| form.set(None)>"Cancel"</button>
                                <button class="btn primary" on:click=move |_| shrink()>"Make copies"</button></div>
                        </div>
                    }.into_any(),
                })}
                {move || busy.get().map(|(b, pct)| view! {
                    <div class="lib-busy"><span class="spin"></span><span class="text">{b}</span>
                        {super::transfer::meter(pct, "Done so far")}
                        <button class="btn small" on:click=move |_| stop.set_value(true)>{ico(Icon::Stop)}"Stop"</button></div>
                })}
                {move || message.get().map(|(m, ok)| view! { <p class:ok=ok class:error=!ok>{m}</p> })}
                <div class="books">
                    {move || match list.get() {
                        None => view! { <p class="hint">"Looking for books…"</p> }.into_any(),
                        Some(Err(e)) => view! { <p class="error">{e}</p> }.into_any(),
                        Some(Ok((all, _))) if all.is_empty() => view! { <p class="hint">"No EPUB books in this folder or its subfolders."</p> }.into_any(),
                        Some(Ok((_, complete))) => view! {
                            {(!complete).then(|| view! { <p class="hint">"Too many files to look through. Open a smaller folder to see all its books."</p> })}
                            <For each=shown key=|e| (e.path.clone(), e.size, e.mtime) let:e>
                                {
                                    let e = StoredValue::new(e);
                                    let path = e.get_value().path;
                                    let (p1, p2, p3, p4, p5) = (path.clone(), path.clone(), path.clone(), path.clone(), path.clone());
                                    let rel = path.strip_prefix(&dir.get_value()).unwrap_or(&path).trim_start_matches('/').to_string();
                                    let cover = thumb_url(src, &e.get_value());
                                    view! {
                                        <div class="book" class:picked=move || picked.with(|p| p.contains(&p1))>
                                            <input type="checkbox" prop:checked=move || picked.with(|p| p.contains(&p2))
                                                on:change=move |_| picked.update(|p| if !p.remove(&p3) { p.insert(p3.clone()); })/>
                                            <div class="cover">{cover.map(|u| view! { <img src=u loading="lazy" alt=""/> })}</div>
                                            <div class="what">
                                                {move || {
                                                    let i = infos.with(|m| m.get(&p4).cloned());
                                                    let name = e.get_value().name;
                                                    match i {
                                                        None => view! { <b>{name}</b><span class="about">"Reading…"</span> }.into_any(),
                                                        Some(BookInfo { problem: Some(p), .. }) => view! { <b>{name}</b><span class="about error">{p}</span> }.into_any(),
                                                        Some(i) => {
                                                            let title = if i.title.is_empty() { name } else { i.title };
                                                            let about = [i.creators.join(", "), i.language, i.publisher].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(" · ");
                                                            view! { <b>{title}</b><span class="about">{about}</span> }.into_any()
                                                        }
                                                    }
                                                }}
                                                <span class="path">{format!("{rel} · {}", human_size(e.get_value().size))}</span>
                                                {move || notes.with(|n| n.get(&p5).cloned()).map(|(t, ok)| {
                                                    let path = e.get_value().path;
                                                    let has = issues.with(|m| m.get(&path).is_some_and(|v| !v.is_empty()));
                                                    view! {
                                                        <span class="note" class:ok=ok class:error=!ok>{t}
                                                            {has.then(|| view! {
                                                                <button class="link" on:click=move |_| {
                                                                    let path = e.get_value().path;
                                                                    shown_issues.update(|s| *s = if s.as_deref() == Some(path.as_str()) { None } else { Some(path) });
                                                                }>" Details"</button>
                                                            })}
                                                        </span>
                                                    }
                                                })}
                                                {move || {
                                                    let path = e.get_value().path;
                                                    (shown_issues.get().as_deref() == Some(path.as_str())).then(|| view! {
                                                        <ul class="issues">
                                                            {issues.with(|m| m.get(&path).cloned().unwrap_or_default()).into_iter().map(|i| view! {
                                                                <li class:serious=i.serious>{i.text}</li>
                                                            }).collect_view()}
                                                        </ul>
                                                    })
                                                }}
                                            </div>
                                            <button class="btn" title="Read the book" on:click=move |_| {
                                                ctx.panel.set(None);
                                                ctx.read_book(src, e.get_value().path);
                                            }>{ico(Icon::Book)}"Read"</button>
                                            <button class="btn" title="Edit in the preview" on:click=move |_| {
                                                ctx.panel.set(None);
                                                ctx.edit_book(src, e.get_value().path);
                                            }>{ico(Icon::Edit)}"Edit"</button>
                                        </div>
                                    }
                                }
                            </For>
                        }.into_any(),
                    }}
                </div>
                <div class="actions">
                    <span class="about">{move || match list.get() {
                        Some(Ok((all, _))) => {
                            let read = infos.with(|i| i.len()).min(all.len());
                            if read < all.len() { format!("{} books · reading info {read} of {}…", all.len(), all.len()) } else { format!("{} books", all.len()) }
                        }
                        _ => String::new(),
                    }}</span>
                    <span class="grow"></span>
                    </div>
                    <button class="btn primary" on:click=move |_| close()>"Done"</button>
            </div>
        </div>
    }
}
