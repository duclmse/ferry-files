//! The Import photos window: copies (or moves) the photos and videos under a folder into folders
//! by the date each was taken, as a queued job.

use super::*;

/// Folder patterns: value (as `import_photos` takes it) and an example.
const PATTERNS: [(&str, &str); 4] = [("y/m", "2024/05"), ("ymd", "2024-05-14"), ("y/ymd", "2024/2024-05-14"), ("none", "All in one folder")];
const PATTERN_KEY: &str = "ferry-files.import-pattern";

impl Ctx {
    /// Opens the Import photos window for pane `p`'s folder.
    pub(super) fn ask_import(self, p: Pane) {
        let from = (p.src.get_untracked(), p.cwd.get_untracked());
        let mut targets = vec![];
        let o = self.other(p);
        let theirs = (o.src.get_untracked(), o.cwd.get_untracked());
        if self.split.get_untracked() && !theirs.1.is_empty() && theirs != from {
            targets.push(theirs);
        }
        if let Some(home) = self.mac.get_untracked().map(|i| i.default_dir) {
            let pics = (Src::Mac, format!("{}/Pictures", home.trim_end_matches('/')));
            if !targets.contains(&pics) && pics != from {
                targets.push(pics);
            }
        }
        if targets.is_empty() {
            p.error("Open the folder to import into in the other pane first.");
            return;
        }
        self.panel.set(Some(Panel::Import(p.id, from, targets)));
    }
}

#[component]
pub(super) fn ImportPhotos(pane: usize, from: (Src, String), targets: Vec<(Src, String)>) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let targets = StoredValue::new(targets);
    let from = StoredValue::new(from);
    let target = RwSignal::new(0usize);
    let pattern = RwSignal::new(stored(PATTERN_KEY).filter(|p| PATTERNS.iter().any(|x| x.0 == p)).unwrap_or_else(|| "y/m".into()));
    let moving = RwSignal::new(false);
    let phone = from.with_value(|f| f.0 != Src::Mac);
    let close = move || ctx.panel.set(None);
    let start = move || {
        let (src, dir) = from.get_value();
        let (to, to_dir) = targets.with_value(|t| t[target.get_untracked()].clone());
        store(PATTERN_KEY, &pattern.get_untracked());
        let work =
            Work::Import { from: ctx.side(src), from_dir: dir, to: ctx.side(to), to_dir, pattern: pattern.get_untracked(), moving: moving.get_untracked() };
        ctx.enqueue(pane, work, false);
        close();
    };
    let example = move || {
        let (_, dir) = targets.with_value(|t| t[target.get()].clone());
        let sub = PATTERNS.iter().find(|x| x.0 == pattern.get()).filter(|x| x.0 != "none").map_or(String::new(), |x| format!("/{}", x.1));
        format!("For example {dir}{sub}/IMG_0001.jpg")
    };
    view! {
        <div class="overlay" on:click=move |_| close()>
            <div class="dialog import" on:click=|e| e.stop_propagation()>
                <h2>"Import photos"</h2>
                <p class="hint">{move || from.with_value(|(s, d)| format!("Photos and videos in {d} on {}, and in its folders, sorted by the date each was taken. Ones imported before (same name and size) are left out.", ctx.place(*s)))}</p>
                <label class="row">"Into"
                    <select on:change=move |e| target.set(event_target_value(&e).parse().unwrap_or(0))>
                        {targets.get_value().into_iter().enumerate().map(|(i, (s, d))| view! {
                            <option value=i.to_string() selected=move || target.get() == i>{format!("{d} on {}", ctx.place(s))}</option>
                        }).collect_view()}
                    </select>
                </label>
                <label class="row">"Folders"
                    <select on:change=move |e| pattern.set(event_target_value(&e))>
                        {PATTERNS.iter().map(|(v, l)| view! { <option value=*v selected=move || pattern.with(|p| p == v)>{*l}</option> }).collect_view()}
                    </select>
                </label>
                <p class="hint">{example}</p>
                <label class="row">
                    <input type="checkbox" prop:checked=move || moving.get() on:change=move |e| moving.set(event_target_checked(&e))/>
                    {if phone { "Remove them from the phone once imported" } else { "Remove the originals once imported" }}
                </label>
                {move || moving.get().then(|| view! {
                    <p class="hint">{format!("The originals are backed up on {} first. Ones found imported before are kept.", crate::os::the_computer())}</p>
                })}
                <div class="actions">
                    <span class="grow"></span>
                    <button class="btn" on:click=move |_| close()>"Cancel"</button>
                    <button class="btn primary" on:click=move |_| start()>{move || if moving.get() { "Import and remove" } else { "Import" }}</button>
                </div>
            </div>
        </div>
    }
}
