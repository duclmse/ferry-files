//! The Find duplicates window: files with the same content under one or two folders (on any
//! devices), in groups, to pick the extra copies and delete them (backed up first).

use super::*;
use crate::api::Dupe;

/// Smallest file looked at: label and bytes.
const MIN_SIZES: [(&str, u64); 5] = [("Any size", 1), ("100 KB and up", 100_000), ("1 MB and up", 1_000_000), ("10 MB and up", 10_000_000), ("100 MB and up", 100_000_000)];

/// The copy to keep in a group: one on the computer, else the oldest.
fn keeper(g: &[Dupe], folders: &[(Src, String)]) -> usize {
    (0..g.len()).min_by_key(|&i| (folders[g[i].at].0 != Src::Mac, g[i].mtime, i)).unwrap_or(0)
}

#[component]
pub(super) fn Duplicates(folders: Vec<(Src, String)>) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let folders = StoredValue::new(folders);
    let use_folder = RwSignal::new(folders.with_value(|f| vec![true; f.len()]));
    let min = RwSignal::new(1usize);
    let job = RwSignal::new(None::<u32>);
    let found = RwSignal::new(None::<Dupes>);
    let chosen = RwSignal::new(HashSet::<(usize, String)>::new());
    let message = RwSignal::new(None::<(String, bool)>);
    let confirming = RwSignal::new(false);
    let deleting = RwSignal::new(false);

    let search = move || {
        let picked: Vec<(Src, String)> = folders.with_value(|f| f.iter().zip(use_folder.get_untracked()).filter(|x| x.1).map(|x| x.0.clone()).collect());
        if picked.is_empty() {
            message.set(Some(("Pick a folder to look in.".into(), false)));
            return;
        }
        let id = ctx.next_job.get_value();
        ctx.next_job.set_value(id + 1);
        job.set(Some(id));
        found.set(None);
        chosen.set(HashSet::new());
        message.set(None);
        ctx.dupe_progress.set(None);
        // Results index the folders searched; map them back to all the window's folders.
        let index: Vec<usize> = folders.with_value(|f| (0..f.len()).filter(|&i| use_folder.get_untracked()[i]).collect());
        let args = json!({
            "job": id,
            "folders": picked.iter().map(|(s, d)| (s.key(), d.clone())).collect::<Vec<_>>(),
            "hidden": ctx.show_hidden.get_untracked(),
            "min_size": MIN_SIZES[min.get_untracked()].1,
        });
        spawn_local(async move {
            let r = call::<Dupes>("duplicates", args).await;
            if job.get_untracked() != Some(id) {
                return;
            }
            job.set(None);
            match r {
                Ok(mut d) => {
                    for g in &mut d.groups {
                        for f in g.iter_mut() {
                            f.at = index[f.at];
                        }
                    }
                    found.set(Some(d));
                }
                Err(e) => message.set(Some((e, false))),
            }
        });
    };
    let stop = move || {
        if let Some(id) = job.get_untracked() {
            spawn_local(async move {
                let _ = call::<()>("cancel_transfer", json!({ "job": id })).await;
            });
        }
    };
    let keep_one = move || {
        let Some(d) = found.get_untracked() else { return };
        let mut set = HashSet::new();
        folders.with_value(|f| {
            for g in &d.groups {
                let k = keeper(g, f);
                set.extend(g.iter().enumerate().filter(|(i, _)| *i != k).map(|(_, x)| (x.at, x.path.clone())));
            }
        });
        chosen.set(set);
    };
    let chosen_bytes = move || {
        found.with(|d| d.as_ref().map_or(0, |d| d.groups.iter().flatten().filter(|x| chosen.with(|c| c.contains(&(x.at, x.path.clone())))).map(|x| x.size).sum::<u64>()))
    };
    // A group with every copy picked would lose the file.
    let all_of_one = move || found.with(|d| d.as_ref().is_some_and(|d| d.groups.iter().any(|g| chosen.with(|c| g.iter().all(|x| c.contains(&(x.at, x.path.clone())))))));

    let delete = move || {
        confirming.set(false);
        if all_of_one() {
            message.set(Some(("Keep at least one copy in each group.".into(), false)));
            return;
        }
        let picked: Vec<(usize, String)> = chosen.get_untracked().into_iter().collect();
        let list = folders.get_value();
        deleting.set(true);
        spawn_local(async move {
            let mut gone: Vec<(usize, String)> = vec![];
            let mut undo: HashMap<Src, Vec<(String, bool, Option<String>)>> = HashMap::new();
            let mut error = None;
            for (at, path) in &picked {
                let src = list[*at].0;
                match call::<Option<String>>("remove", json!({ "src": src.key(), "path": path })).await {
                    Ok(backup) => {
                        undo.entry(src).or_default().push((path.clone(), false, backup));
                        gone.push((*at, path.clone()));
                    }
                    Err(e) => {
                        error = Some(format!("{}: {e}", base(path)));
                        break;
                    }
                }
            }
            deleting.set(false);
            for (src, items) in undo {
                let what = if items.len() == 1 { base(&items[0].0) } else { format!("{} duplicates", items.len()) };
                ctx.log_undo(format!("Deleted {what} from {}", ctx.place(src)), Undo::Delete { src, items: items.clone() });
                let mut dirs: Vec<String> = items.iter().map(|x| parent(&x.0)).collect();
                dirs.sort();
                dirs.dedup();
                for d in dirs {
                    ctx.changed(src, &d);
                }
            }
            found.update(|d| {
                if let Some(d) = d {
                    for g in &mut d.groups {
                        g.retain(|x| !gone.contains(&(x.at, x.path.clone())));
                    }
                    d.groups.retain(|g| g.len() > 1);
                }
            });
            chosen.update(|c| c.retain(|x| !gone.contains(x)));
            let note = format!("Deleted {} file{}. They're in Backups on {}.", gone.len(), if gone.len() == 1 { "" } else { "s" }, crate::os::the_computer());
            message.set(Some(match error {
                None => (note, true),
                Some(e) => (format!("{note} Then: {e}"), false),
            }));
        });
    };
    let show = move |at: usize, path: String| {
        let src = folders.with_value(|f| f[at].0);
        let p = ctx.act();
        ctx.load_select(p, src, parent(&path), Some(path));
        ctx.panel.set(None);
    };
    let close = move || {
        stop();
        ctx.panel.set(None);
    };

    view! {
        <div class="overlay" on:click=move |_| if !deleting.get_untracked() { close() }>
            <div class="dialog wide dupes" on:click=|e| e.stop_propagation()>
                <h2>"Find duplicates"</h2>
                <p class="hint">"Files with the same content, in these folders and their subfolders. Compared by size, then by their bytes."</p>
                <div class="dupe-folders">
                    {folders.get_value().into_iter().enumerate().map(|(i, (src, dir))| view! {
                        <label class="row">
                            <input type="checkbox" prop:checked=move || use_folder.with(|u| u[i]) on:change=move |e| use_folder.update(|u| u[i] = event_target_checked(&e))/>
                            {format!("{dir} on {}", ctx.place(src))}
                        </label>
                    }).collect_view()}
                </div>
                <div class="actions">
                    <select aria-label="Smallest file" on:change=move |e| min.set(event_target_value(&e).parse().unwrap_or(0))>
                        {MIN_SIZES.iter().enumerate().map(|(i, (label, _))| view! { <option value=i.to_string() selected=move || min.get() == i>{*label}</option> }).collect_view()}
                    </select>
                    {move || if job.get().is_some() {
                        view! {
                            <span class="hint">{move || ctx.dupe_progress.get().filter(|(j, _)| Some(*j) == job.get()).map_or("Looking…".into(), |(_, t)| t)}</span>
                            <button class="btn" on:click=move |_| stop()>"Stop"</button>
                        }.into_any()
                    } else {
                        view! { <button class="btn primary" on:click=move |_| search()>"Find"</button> }.into_any()
                    }}
                </div>
                {move || message.get().map(|(m, ok)| view! { <p class:ok=ok class:error=!ok>{m}</p> })}
                {move || found.get().map(|d| {
                    let groups = d.groups.len();
                    let extra: u64 = d.groups.iter().map(|g| g[0].size * (g.len() as u64 - 1)).sum();
                    let summary = if groups == 0 {
                        format!("No duplicates among {} files.", d.files)
                    } else {
                        format!("{groups} group{} of duplicates among {} files; the extra copies take {}.", if groups == 1 { "" } else { "s" }, d.files, human_size(extra))
                    };
                    let partial = (!d.complete).then_some(" Stopped before the end, so there may be more.");
                    view! {
                        <p class="hint">{summary}{partial}</p>
                        <div class="dupe-groups">
                            {d.groups.into_iter().map(|g| {
                                let head = format!("{} copies · {} each", g.len(), human_size(g[0].size));
                                view! {
                                    <div class="dupe-group">
                                        <div class="head">{head}</div>
                                        {g.into_iter().map(|x| {
                                            let (src, place) = folders.with_value(|f| (f[x.at].0, ctx.place(f[x.at].0)));
                                            let key = (x.at, x.path.clone());
                                            let k2 = key.clone();
                                            let entry = Entry { name: x.name.clone(), path: x.path.clone(), dir: false, size: x.size, mtime: x.mtime, hidden: false };
                                            let path = x.path.clone();
                                            view! {
                                                <label class="dupe">
                                                    <input type="checkbox" prop:checked=move || chosen.with(|c| c.contains(&key))
                                                        on:change=move |e| { let on = event_target_checked(&e); chosen.update(|c| { if on { c.insert(k2.clone()); } else { c.remove(&k2); } }); }/>
                                                    <span class="pic"><Thumbnail src=src entry=entry/></span>
                                                    <span class="what">
                                                        <b>{x.name.clone()}</b>
                                                        <span class="about">{format!("{} · {place}", when(x.mtime))}</span>
                                                        <span class="path">{x.path.clone()}</span>
                                                    </span>
                                                    <button class="btn small" title="Show it in the active pane" on:click=move |e| { e.prevent_default(); show(x.at, path.clone()); }>"Show"</button>
                                                </label>
                                            }
                                        }).collect_view()}
                                    </div>
                                }
                            }).collect_view()}
                        </div>
                    }
                })}
                <div class="actions">
                    {move || found.with(|d| d.as_ref().is_some_and(|d| !d.groups.is_empty())).then(|| view! {
                        <button class="btn" title="Pick every copy but one in each group: the one on the computer, else the oldest" on:click=move |_| keep_one()>"Keep one per group"</button>
                        <button class="btn" on:click=move |_| chosen.set(HashSet::new())>"Clear"</button>
                    })}
                    <span class="grow"></span>
                    {move || {
                        let n = chosen.with(HashSet::len);
                        if confirming.get() {
                            view! {
                                <span class="hint">{format!("Delete {n} file{} ({})? They're backed up first.", if n == 1 { "" } else { "s" }, human_size(chosen_bytes()))}</span>
                                <button class="btn" on:click=move |_| confirming.set(false)>"Cancel"</button>
                                <button class="btn danger solid" on:click=move |_| delete()>{ico(Icon::Trash)}"Delete"</button>
                            }.into_any()
                        } else {
                            view! {
                                <button class="btn danger" disabled=move || n == 0 || deleting.get() || all_of_one()
                                    title=move || if all_of_one() { "Every copy in a group is picked: keep at least one" } else { "Delete the picked files (backed up first)" }
                                    on:click=move |_| confirming.set(true)>{ico(Icon::Trash)}{move || if deleting.get() { "Deleting…".to_string() } else { format!("Delete {n}…") }}</button>
                                <button class="btn primary" on:click=move |_| close()>"Done"</button>
                            }.into_any()
                        }
                    }}
                </div>
            </div>
        </div>
    }
}
