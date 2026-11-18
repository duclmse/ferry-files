//! Safety: the session history, free space, eject, and the backups and history windows.

use super::*;

impl Ctx {
    /// Notes an action in the session's history.
    pub(super) fn log(self, what: impl Into<String>) {
        let d = js_sys::Date::new_0();
        let at = format!("{:02}:{:02}:{:02}", d.get_hours(), d.get_minutes(), d.get_seconds());
        self.history.update(|h| h.push((at, what.into())));
    }

    /// Asks how much space is free on `src`'s storages.
    pub(super) fn load_space(self, src: Src) {
        spawn_local(async move {
            if let Ok(s) = call::<Vec<Space>>("space", json!({ "src": src.key() })).await {
                self.space.update(|m| {
                    m.insert(src.key(), s);
                });
            }
        });
    }

    /// Free space on the storage holding the pane's folder, e.g. "12.3 GB free of 64 GB".
    pub(super) fn free_text(self, p: Pane) -> Option<String> {
        let (src, cwd) = (p.src.get(), p.cwd.get());
        let transport = self.src_info(src).map(|i| i.transport).unwrap_or_default();
        self.space.with(|m| {
            let list = m.get(&src.key())?;
            let s = match (src, transport.as_str()) {
                (Src::Mac, _) => {
                    let disk = cwd.strip_prefix("/Volumes/").map(|r| r.split('/').next().unwrap_or_default().to_string());
                    list.iter().find(|s| Some(&s.name) == disk.as_ref()).or_else(|| list.iter().find(|s| s.name == "Mac"))
                }
                (_, "mtp") => {
                    let first = cwd.trim_start_matches('/').split('/').next().unwrap_or_default();
                    list.iter().find(|s| s.name == first)
                }
                _ => list.first(),
            }?;
            Some(format!("{} free of {}", human_size(s.free), human_size(s.total)))
        })
    }

    /// Ejects a disk. Panes showing it go back to the home folder first.
    pub(super) fn eject(self, path: String) {
        if self.job.with_untracked(|j| j.is_some()) {
            self.act().error("Wait for the copy to finish before ejecting.");
            return;
        }
        for p in self.panes {
            if p.src.get_untracked() == Src::Mac && within(&p.cwd.get_untracked(), &path) {
                if let Some(home) = self.mac.get_untracked().map(|i| i.default_dir) {
                    self.load_select(p, Src::Mac, home, None);
                }
            }
        }
        spawn_local(async move {
            match call::<()>("eject", json!({ "path": path })).await {
                Ok(()) => self.log(format!("Ejected {}", base(&path))),
                Err(e) => self.act().error(e),
            }
            self.scan();
        });
    }
}

/// The backups the app has made, newest first, each with Restore (to where it came from, when that
/// device is connected) and Restore here (into the active pane's folder).
#[component]
pub(super) fn Backups() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let list = RwSignal::new(None::<Result<Vec<Backup>, String>>);
    let load = move || spawn_local(async move { list.set(Some(call("backups", json!({})).await)) });
    load();
    let message = RwSignal::new(None::<(String, bool)>);
    let filter = RwSignal::new(String::new());
    let restore = move |b: Backup, src: Src, path: String| {
        spawn_local(async move {
            match call::<WriteResult>("restore", json!({ "file": b.file, "src": src.key(), "path": path })).await {
                Ok(r) => {
                    let note = if r.backup.is_some() { " The file that was there is backed up too." } else { "" };
                    message.set(Some((format!("Restored {} to {path} on {}.{note}", b.name, ctx.place(src)), true)));
                    ctx.log(format!("Restored {} to {path}", b.name));
                    ctx.changed(src, &parent(&path));
                    load();
                }
                Err(e) => message.set(Some((e, false))),
            }
        });
    };
    let close = move || ctx.panel.set(None);
    view! {
        <div class="overlay" on:click=move |_| close()>
            <div class="dialog wide" on:click=|e| e.stop_propagation()>
                <h2>"Backups"</h2>
                <p class="hint">"Copies of files from before the app replaced or deleted them. Restoring puts a copy back; a file already there is backed up first."</p>
                <input class="filter-wide" type="search" placeholder="Filter by name" prop:value=move || filter.get()
                    on:input=move |ev| filter.set(event_target_value(&ev))/>
                {move || message.get().map(|(m, ok)| view! { <p class:ok=ok class:error=!ok>{m}</p> })}
                <div class="backups">
                    {move || match list.get() {
                        None => view! { <p class="hint">"Loading…"</p> }.into_any(),
                        Some(Err(e)) => view! { <p class="error">{e}</p> }.into_any(),
                        Some(Ok(bs)) if bs.is_empty() => view! { <p class="hint">"No backups yet."</p> }.into_any(),
                        Some(Ok(bs)) => {
                            let f = filter.get().to_lowercase();
                            bs.into_iter().filter(|b| b.name.to_lowercase().contains(&f)).map(|b| {
                                let origin = b.note.as_ref().map(|n| {
                                    let on = if n.place == "mac" { "the Mac".to_string() } else { n.place.split_once(':').map_or(n.place.clone(), |(_, l)| l.to_string()) };
                                    format!("{} {} on {on}", if n.reason == "deleted" { "Deleted from" } else { "Replaced at" }, n.path)
                                }).unwrap_or_else(|| "Where it came from wasn't noted (an older backup).".into());
                                let back = b.note.as_ref().and_then(|n| ctx.fav_src(&Fav { place: n.place.clone(), label: String::new(), path: n.path.clone() }).map(|s| (s, n.path.clone())));
                                let back_tip = if back.is_some() { "Put it back where it was".to_string() } else if b.note.is_some() { "Connect that device to put it back where it was".into() } else { "Where it came from isn't known".into() };
                                let (b1, b2) = (b.clone(), b.clone());
                                let here_tip = "Restore into the folder open in the active pane";
                                view! {
                                    <div class="backup">
                                        <div class="what">
                                            <b>{b.name.clone()}</b>
                                            <span class="about">{format!("{} · {}", when(b.mtime), human_size(b.size))}</span>
                                            <span class="path">{origin}</span>
                                        </div>
                                        <button class="btn" title=back_tip disabled=back.is_none()
                                            on:click=move |_| if let Some((s, path)) = back.clone() { restore(b1.clone(), s, path) }>"Restore"</button>
                                        <button class="btn" title=here_tip on:click=move |_| {
                                            let p = ctx.act();
                                            let dir = p.cwd.get_untracked();
                                            if dir.is_empty() { return }
                                            restore(b2.clone(), p.src.get_untracked(), join(&dir, &b2.name));
                                        }>"Restore here"</button>
                                    </div>
                                }
                            }).collect_view().into_any()
                        }
                    }}
                </div>
                <div class="actions">
                    <button class="btn" on:click=move |_| { let _ = spawn_local(async move { let _ = call::<()>("open_folder", json!({ "which": "backups" })).await; }); }>"Open in Finder"</button>
                    <span class="grow"></span>
                    <button class="btn primary" on:click=move |_| close()>"Done"</button>
                </div>
            </div>
        </div>
    }
}

/// What was done since the app started.
#[component]
pub(super) fn History() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let close = move || ctx.panel.set(None);
    view! {
        <div class="overlay" on:click=move |_| close()>
            <div class="dialog wide" on:click=|e| e.stop_propagation()>
                <h2>"History"</h2>
                <p class="hint">"What was done since the app started. Replaced and deleted files can be put back from Backups."</p>
                <div class="history">
                    {move || {
                        let h = ctx.history.get();
                        if h.is_empty() {
                            return view! { <p class="hint">"Nothing yet."</p> }.into_any();
                        }
                        h.into_iter().rev().map(|(at, what)| view! {
                            <div class="event"><span class="at">{at}</span><span>{what}</span></div>
                        }).collect_view().into_any()
                    }}
                </div>
                <div class="actions">
                    <button class="btn" on:click=move |_| ctx.panel.set(Some(Panel::Backups))>"Backups…"</button>
                    <span class="grow"></span>
                    <button class="btn primary" on:click=move |_| close()>"Done"</button>
                </div>
            </div>
        </div>
    }
}
