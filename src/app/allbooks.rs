//! The library across devices: the books in each place's book folder (the computer and each
//! connected device), matched by title and author, showing where each one is, and copying a book
//! to a place that lacks it.

use super::*;

const BOOK_DIRS_KEY: &str = "ferry-files.book-dirs";

/// A place's book folder: the folder Books was last opened on there (`Fav` names the place).
fn book_dirs() -> Vec<Fav> {
    stored(BOOK_DIRS_KEY).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

/// What makes two files the same book: the title and first author, else the file name.
fn book_key(e: &Entry, info: Option<&BookInfo>) -> String {
    let squash = |s: &str| s.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ");
    match info {
        Some(i) if i.problem.is_none() && !i.title.trim().is_empty() => format!("{}|{}", squash(&i.title), squash(i.creators.first().map_or("", |c| c.as_str()))),
        _ => format!("file|{}", squash(e.name.trim_end_matches(".epub").trim_end_matches(".EPUB"))),
    }
}

impl Ctx {
    /// Remembers `dir` as the book folder of `src`'s place, for the library across devices.
    pub(super) fn remember_book_dir(self, src: Src, dir: &str) {
        let place = self.place_key(src);
        if place.is_empty() || dir.is_empty() {
            return;
        }
        let label = self.src_info(src).map_or_else(String::new, |i| i.label);
        let mut dirs = book_dirs();
        dirs.retain(|d| d.place != place);
        dirs.push(Fav { place, label, path: dir.to_string() });
        store(BOOK_DIRS_KEY, &serde_json::to_string(&dirs).unwrap_or_default());
    }
}

/// A book across places: title, about, and its copies (place, file).
type Merged = (String, String, Vec<(usize, Entry)>);

/// A place in the library: where it is, its book folder, and what's been found there.
#[derive(Clone)]
struct Place {
    src: Src,
    dir: String,
    name: String,
}

#[component]
pub(super) fn AllBooks() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let (on, off): (Vec<_>, Vec<_>) = book_dirs().into_iter().partition(|f| ctx.fav_src(f).is_some());
    let mut places: Vec<Place> = on.iter().map(|f| {
        let src = ctx.fav_src(f).unwrap_or(Src::Mac);
        Place { src, dir: f.path.clone(), name: ctx.place(src) }
    }).collect();
    // The computer first.
    places.sort_by_key(|p| p.src != Src::Mac);
    let places = StoredValue::new(places);
    let away: Vec<String> = off.into_iter().map(|f| format!("{} ({})", f.label, f.path)).collect();
    let n = places.with_value(Vec::len);
    // Per place: its books, or why not (None while looking).
    let found = RwSignal::new(vec![None::<Result<Vec<Entry>, String>>; n]);
    let infos = RwSignal::new(HashMap::<(usize, String), BookInfo>::new());
    let filter = RwSignal::new(String::new());
    // Show only books missing from this place.
    let missing_on = RwSignal::new(None::<usize>);
    let picked = RwSignal::new(HashSet::<String>::new());
    let send_to = RwSignal::new(0usize);
    let message = RwSignal::new(None::<(String, bool)>);
    let alive = StoredValue::new(());

    for (at, pl) in places.get_value().into_iter().enumerate() {
        spawn_local(async move {
            #[derive(serde::Deserialize)]
            struct Matches {
                entries: Vec<Entry>,
            }
            let args = json!({ "src": pl.src.key(), "dir": pl.dir, "hidden": ctx.show_hidden.get_untracked() });
            let r = call::<Matches>("library", args).await.map(|m| m.entries);
            let entries = r.clone().unwrap_or_default();
            found.try_update(|f| f[at] = Some(r));
            for e in entries {
                if alive.try_get_value().is_none() {
                    return;
                }
                let args = json!({ "src": pl.src.key(), "path": e.path, "size": e.size, "mtime": e.mtime });
                let info = call::<BookInfo>("book_info", args).await.unwrap_or_else(|err| BookInfo { problem: Some(err), ..Default::default() });
                infos.try_update(|i| {
                    i.insert((at, e.path.clone()), info);
                });
            }
        });
    }

    // The books, matched across places: key, title, about, and the copies (place, file).
    let books = Memo::new(move |_| {
        let mut by: std::collections::BTreeMap<String, Merged> = Default::default();
        found.with(|f| {
            infos.with(|inf| {
                for (at, list) in f.iter().enumerate() {
                    for e in list.iter().flat_map(|r| r.iter().flatten()) {
                        let info = inf.get(&(at, e.path.clone()));
                        let k = book_key(e, info);
                        let slot = by.entry(k).or_insert_with(|| {
                            let (title, about) = match info {
                                Some(i) if i.problem.is_none() && !i.title.is_empty() => (i.title.clone(), i.creators.join(", ")),
                                _ => (e.name.clone(), String::new()),
                            };
                            (title, about, vec![])
                        });
                        slot.2.push((at, e.clone()));
                    }
                }
            })
        });
        let mut v: Vec<_> = by.into_iter().map(|(k, (t, a, c))| (k, t, a, c)).collect();
        v.sort_by_key(|b| b.1.to_lowercase());
        v
    });
    let shown = move || {
        let f = filter.get().to_lowercase();
        let miss = missing_on.get();
        books.get().into_iter().filter(|(_, t, a, c)| {
            (f.is_empty() || t.to_lowercase().contains(&f) || a.to_lowercase().contains(&f) || c.iter().any(|(_, e)| e.name.to_lowercase().contains(&f)))
                && miss.is_none_or(|m| !c.iter().any(|(at, _)| *at == m))
        }).collect::<Vec<_>>()
    };

    // Copies books to place `to`, each from the computer when it's there, else the first place
    // with it; books already there are left out. One queued copy per place copied from.
    let send = move |keys: Vec<String>, to: usize| {
        let all = books.get_untracked();
        let mut from: HashMap<usize, Vec<String>> = HashMap::new();
        for (_, _, _, copies) in all.iter().filter(|b| keys.contains(&b.0)) {
            if copies.iter().any(|(at, _)| *at == to) {
                continue;
            }
            if let Some((at, e)) = copies.iter().min_by_key(|(at, _)| *at) {
                from.entry(*at).or_default().push(e.path.clone());
            }
        }
        if from.is_empty() {
            message.set(Some(("Those books are already there.".into(), true)));
            return;
        }
        let (to_src, to_dir, to_name) = places.with_value(|p| (p[to].src, p[to].dir.clone(), p[to].name.clone()));
        let mut n = 0;
        for (at, items) in from {
            n += items.len();
            let src = places.with_value(|p| p[at].src);
            ctx.transfer(ctx.act(), src, items, to_src, to_dir.clone(), false);
        }
        message.set(Some((format!("Copying {n} {} to {to_dir} on {to_name}: see the queue below the panes.", if n == 1 { "book" } else { "books" }), true)));
    };

    let close = move || ctx.panel.set(None);
    view! {
        <div class="overlay" on:click=move |_| close()>
            <div class="dialog library all-books" on:click=|e| e.stop_propagation()>
                <h2>"Books on all devices"</h2>
                <p class="hint">{move || places.with_value(|p| {
                    let list = p.iter().enumerate().map(|(i, pl)| {
                        let state = found.with(|f| match &f[i] {
                            None => "looking…".to_string(),
                            Some(Ok(v)) => format!("{} books", v.len()),
                            Some(Err(e)) => e.clone(),
                        });
                        format!("{} on {} ({state})", pl.dir, pl.name)
                    }).collect::<Vec<_>>().join("; ");
                    format!("The book folder of each place: the folder Books was last opened on there. {list}.")
                })}
                {(!away.is_empty()).then(|| format!(" Not connected: {}.", away.join(", ")))}</p>
                {(n == 0).then(|| view! { <p class="hint">"No book folders yet. Open a folder of books, then pick Books in this folder… from its menu: it becomes the book folder of that place."</p> })}
                <div class="lib-bar">
                    <input class="filter-wide" type="search" placeholder="Filter by title, author or file name"
                        prop:value=move || filter.get() on:input=move |ev| filter.set(event_target_value(&ev))/>
                    <select aria-label="Show" on:change=move |e| missing_on.set(event_target_value(&e).parse::<usize>().ok())>
                        <option value="all">"All books"</option>
                        {places.get_value().into_iter().enumerate().map(|(i, pl)| view! { <option value=i.to_string()>{format!("Missing on {}", pl.name)}</option> }).collect_view()}
                    </select>
                    <span class="grow"></span>
                    {move || (n > 1).then(|| view! {
                        <span class="about">{move || format!("{} picked", picked.with(HashSet::len))}</span>
                        <select aria-label="Copy to" on:change=move |e| send_to.set(event_target_value(&e).parse().unwrap_or(0))>
                            {places.get_value().into_iter().enumerate().map(|(i, pl)| view! { <option value=i.to_string() selected=move || send_to.get() == i>{format!("to {}", pl.name)}</option> }).collect_view()}
                        </select>
                        <button class="btn" disabled=move || picked.with(HashSet::is_empty)
                            on:click=move |_| send(picked.get_untracked().into_iter().collect(), send_to.get_untracked())>{ico(Icon::Copy)}"Copy picked"</button>
                    })}
                </div>
                {move || message.get().map(|(m, ok)| view! { <p class:ok=ok class:error=!ok>{m}</p> })}
                <div class="books">
                    {move || {
                        let list = shown();
                        if list.is_empty() {
                            let looking = found.with(|f| f.iter().any(Option::is_none));
                            return view! { <p class="hint">{if looking { "Looking for books…" } else { "No books to show." }}</p> }.into_any();
                        }
                        list.into_iter().map(|(key, title, about, copies)| {
                            let (k1, k2, k3) = (key.clone(), key.clone(), key.clone());
                            let (at0, e0) = copies[0].clone();
                            let cover = thumb_url(places.with_value(|p| p[at0].src), &e0);
                            let chips = places.get_value().into_iter().enumerate().map(|(i, pl)| {
                                let here = copies.iter().find(|(at, _)| *at == i).map(|(_, e)| e.path.clone());
                                let k = key.clone();
                                match here {
                                    Some(path) => view! { <span class="chip here" title=path>{format!("✓ {}", pl.name)}</span> }.into_any(),
                                    None if n > 1 => view! {
                                        <button class="chip missing" title=format!("Copy it to {} on {}", pl.dir, pl.name)
                                            on:click=move |_| send(vec![k.clone()], i)>{format!("Copy to {}", pl.name)}</button>
                                    }.into_any(),
                                    None => ().into_any(),
                                }
                            }).collect_view();
                            view! {
                                <div class="book" class:picked=move || picked.with(|p| p.contains(&k1))>
                                    <input type="checkbox" prop:checked=move || picked.with(|p| p.contains(&k2))
                                        on:change=move |_| picked.update(|p| if !p.remove(&k3) { p.insert(k3.clone()); })/>
                                    <div class="cover">{cover.map(|u| view! { <img src=u loading="lazy" alt=""/> })}</div>
                                    <div class="what">
                                        <b>{title}</b>
                                        <span class="about">{about}</span>
                                        <span class="where">{chips}</span>
                                    </div>
                                </div>
                            }
                        }).collect_view().into_any()
                    }}
                </div>
                <div class="actions">
                    <span class="about">{move || format!("{} books", books.with(Vec::len))}</span>
                    <span class="grow"></span>
                    <button class="btn primary" on:click=move |_| close()>"Done"</button>
                </div>
            </div>
        </div>
    }
}
