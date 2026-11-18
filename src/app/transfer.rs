//! Copying and moving between panes, compare and sync, and the progress bar.

use super::*;

impl Ctx {
    /// Copies or moves the picked items into the other pane's folder.
    pub(super) fn send_to_other(self, p: Pane, moving: bool) {
        let items: Vec<String> = p.picked().into_iter().map(|e| e.path).collect();
        if !self.split.get_untracked() || items.is_empty() {
            return;
        }
        let o = self.other(p);
        let (from_src, to_src, to_dir) = (p.src.get_untracked(), o.src.get_untracked(), o.cwd.get_untracked());
        if to_src == from_src && to_dir == p.cwd.get_untracked() {
            p.error("Both panes show this folder. Open another folder in the other pane first.");
            return;
        }
        self.transfer(p, from_src, items, to_src, to_dir, moving);
    }

    /// Copies (or moves) files and folders into `to_dir`, with progress below the panes.
    /// Files already there are skipped, never replaced. The report shows in pane `p`.
    pub(super) fn transfer(self, p: Pane, from_src: Src, items: Vec<String>, to_src: Src, to_dir: String, moving: bool) {
        if self.job.with_untracked(|j| j.is_some()) {
            p.error("Another copy is running. Wait for it to finish, or cancel it.");
            return;
        }
        if to_dir == "/" && self.src_info(to_src).is_some_and(|i| i.transport == "mtp") {
            p.error("Open a storage first: files can't go at the top level over MTP.");
            return;
        }
        let id = self.next_job.get_value();
        self.next_job.set_value(id + 1);
        self.job.set(Some(Job { id, moving, progress: None }));
        p.notice.set(Notice::None);
        let froms: Vec<String> = items.iter().map(|x| parent(x)).collect();
        spawn_local(async move {
            let args = json!({
                "job": id, "from_src": from_src.key(), "paths": items, "to_src": to_src.key(), "to_dir": to_dir,
                "mode": if moving { "move" } else { "copy" },
            });
            match call::<TransferReport>("transfer", args).await {
                Ok(r) => {
                    let n = |c: u32| format!("{c} {}", if c == 1 { "file" } else { "files" });
                    let verb = if moving { "Moved" } else { "Copied" };
                    let mut lines = vec![format!("{verb} {} to {}.", n(r.copied), to_dir)];
                    if r.cancelled {
                        lines.push("Cancelled: the rest wasn't copied.".into());
                    }
                    if moving && !r.backups.is_empty() {
                        lines.push(format!("The originals are backed up on {}.", crate::os::the_computer()));
                    }
                    let shown = 5;
                    for (name, why) in r.skipped.iter().take(shown) {
                        lines.push(format!("Skipped {name}: {why}"));
                    }
                    if r.skipped.len() > shown {
                        lines.push(format!("…and {} more skipped.", r.skipped.len() - shown));
                    }
                    self.log(lines[0].clone());
                    let msg = lines.join("\n");
                    p.notice.set(if r.skipped.is_empty() { Notice::Ok(msg) } else { Notice::Error(msg) });
                }
                Err(e) => p.error(e),
            }
            self.job.set(None);
            self.changed(to_src, &to_dir);
            if moving {
                let mut seen = vec![];
                for d in froms {
                    if !seen.contains(&d) {
                        self.changed(from_src, &d);
                        seen.push(d);
                    }
                }
            }
        });
    }

    /// Compares this pane's folder with the other pane's, and asks before copying what's missing.
    pub(super) fn compare(self, p: Pane) {
        let o = self.other(p);
        let from = (p.src.get_untracked(), p.cwd.get_untracked());
        let to = (o.src.get_untracked(), o.cwd.get_untracked());
        if to.1 == "/" && self.src_info(to.0).is_some_and(|i| i.transport == "mtp") {
            p.error("Open a storage in the other pane first: files can't go at the top level over MTP.");
            return;
        }
        p.notice.set(Notice::Ok("Comparing the two folders…".into()));
        let hidden = self.show_hidden.get_untracked();
        spawn_local(async move {
            let args = json!({ "from_src": from.0.key(), "from_dir": from.1, "to_src": to.0.key(), "to_dir": to.1, "hidden": hidden });
            match call::<CompareReport>("compare", args).await {
                Ok(report) => p.notice.set(Notice::Sync { report, from, to }),
                Err(e) => p.error(e),
            }
        });
    }

    /// Copies what `compare` found missing. Runs as a transfer, with progress and Cancel.
    pub(super) fn sync(self, p: Pane, from: (Src, String), to: (Src, String)) {
        if self.job.with_untracked(|j| j.is_some()) {
            p.error("Another copy is running. Wait for it to finish, or cancel it.");
            return;
        }
        let id = self.next_job.get_value();
        self.next_job.set_value(id + 1);
        self.job.set(Some(Job { id, moving: false, progress: None }));
        p.notice.set(Notice::None);
        let hidden = self.show_hidden.get_untracked();
        spawn_local(async move {
            let args = json!({
                "job": id, "from_src": from.0.key(), "from_dir": from.1, "to_src": to.0.key(), "to_dir": to.1, "hidden": hidden,
            });
            match call::<TransferReport>("sync", args).await {
                Ok(r) => {
                    let mut lines = vec![format!("Synced {} into {}: copied {} {}.", base(&from.1), to.1, r.copied, if r.copied == 1 { "file" } else { "files" })];
                    if r.cancelled {
                        lines.push("Cancelled: the rest wasn't copied.".into());
                    }
                    for (name, why) in r.skipped.iter().take(5) {
                        lines.push(format!("Skipped {name}: {why}"));
                    }
                    if r.skipped.len() > 5 {
                        lines.push(format!("…and {} more skipped.", r.skipped.len() - 5));
                    }
                    self.log(lines[0].clone());
                    let msg = lines.join("\n");
                    p.notice.set(if r.skipped.is_empty() { Notice::Ok(msg) } else { Notice::Error(msg) });
                }
                Err(e) => p.error(e),
            }
            self.job.set(None);
            self.changed(to.0, &to.1);
        });
    }

    pub(super) fn cancel_transfer(self) {
        if let Some(j) = self.job.get_untracked() {
            spawn_local(async move {
                let _ = call::<()>("cancel_transfer", json!({ "job": j.id })).await;
            });
        }
    }
}

/// The copy or move under way: what it's on, how far it is, and Cancel.
#[component]
pub(super) fn TransferBar() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    move || ctx.job.get().map(|j| {
        let verb = if j.moving { "Moving" } else { "Copying" };
        let (text, pct) = match &j.progress {
            None => (format!("{verb}: looking at what to copy…"), 0.0),
            Some(pr) => {
                let pct = if pr.bytes_total > 0 { pr.bytes_done as f64 * 100.0 / pr.bytes_total as f64 } else { 0.0 };
                let now = if pr.current.is_empty() { String::new() } else { format!(" {}", pr.current) };
                (format!("{verb}{now} · {} of {} files · {} of {}", pr.files_done, pr.files_total,
                    human_size(pr.bytes_done), human_size(pr.bytes_total)), pct)
            }
        };
        view! {
            <div class="transfer">
                <span class="spin"></span>
                <span class="text">{text}</span>
                <div class="meter"><div style=format!("width:{pct:.1}%")></div></div>
                <button class="btn" title="Stops after the file being copied now"
                    on:click=move |_| ctx.cancel_transfer()>"Cancel"</button>
            </div>
        }
    })
}
