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

    /// Copies (or moves) files and folders into `to_dir`, with progress below the panes. Files
    /// already there are skipped, never replaced. Queued behind any copy already running; the
    /// report shows in pane `p`.
    pub(super) fn transfer(self, p: Pane, from_src: Src, items: Vec<String>, to_src: Src, to_dir: String, moving: bool) {
        if to_dir == "/" && self.src_info(to_src).is_some_and(|i| i.transport == "mtp") {
            p.error("Open a storage first: files can't go at the top level over MTP.");
            return;
        }
        let work = Work::Copy { from: self.side(from_src), items, to: self.side(to_src), to_dir, moving, replace: false };
        self.enqueue(p.id, work, false);
    }

    /// Compares this pane's folder with the other pane's, and asks before copying what's missing.
    pub(super) fn compare(self, p: Pane) {
        let o = self.other(p);
        let from = (p.src.get_untracked(), p.cwd.get_untracked());
        let to = (o.src.get_untracked(), o.cwd.get_untracked());
        self.preview_sync(p, from, to, "copy".into());
    }

    /// Compares `from` with `to` and shows in pane `p` what syncing would do, to confirm.
    pub(super) fn preview_sync(self, p: Pane, from: (Src, String), to: (Src, String), mode: String) {
        if to.1 == "/" && self.src_info(to.0).is_some_and(|i| i.transport == "mtp") {
            p.error("Open a storage in the other pane first: files can't go at the top level over MTP.");
            return;
        }
        p.notice.set(Notice::Ok("Comparing the two folders…".into()));
        let hidden = self.show_hidden.get_untracked();
        spawn_local(async move {
            let args = json!({ "from_src": from.0.key(), "from_dir": from.1, "to_src": to.0.key(), "to_dir": to.1, "hidden": hidden });
            match call::<CompareReport>("compare", args).await {
                Ok(report) => p.notice.set(Notice::Sync { report, from, to, mode }),
                Err(e) => p.error(e),
            }
        });
    }

    /// Syncs as `preview_sync` showed, as a queued job. `remove`: in mirror mode, what the
    /// preview listed to remove (nothing else is).
    pub(super) fn sync(self, p: Pane, from: (Src, String), to: (Src, String), mode: String, remove: Vec<String>) {
        p.notice.set(Notice::None);
        let hidden = self.show_hidden.get_untracked();
        let work = Work::Sync { from: self.side(from.0), from_dir: from.1, to: self.side(to.0), to_dir: to.1, hidden, mode, remove };
        self.enqueue(p.id, work, false);
    }

    /// Saves a sync to the sidebar, to run again; a pair already saved gets the new mode.
    pub(super) fn save_pair(self, from: (Src, String), to: (Src, String), mode: String) {
        let pair = SyncPair { from: self.side(from.0), from_dir: from.1, to: self.side(to.0), to_dir: to.1, mode };
        let mut pairs = self.sync_pairs.get_untracked();
        pairs.retain(|x| !(x.from == pair.from && x.from_dir == pair.from_dir && x.to == pair.to && x.to_dir == pair.to_dir));
        pairs.push(pair);
        self.set_pairs(pairs);
    }

    pub(super) fn set_pairs(self, pairs: Vec<SyncPair>) {
        store(SYNC_PAIRS_KEY, &serde_json::to_string(&pairs).unwrap_or_default());
        self.sync_pairs.set(pairs);
    }

    /// Previews a saved sync in the active pane.
    pub(super) fn open_pair(self, pair: &SyncPair) {
        let p = self.act();
        match (self.src_of(&pair.from), self.src_of(&pair.to)) {
            (Ok(a), Ok(b)) => self.preview_sync(p, (a, pair.from_dir.clone()), (b, pair.to_dir.clone()), pair.mode.clone()),
            (Err(e), _) | (_, Err(e)) => p.error(e),
        }
    }

    /// What a side is called: "the computer", or the phone's name.
    pub(super) fn side_name(self, side: &Side) -> String {
        match side {
            Side::Mac => crate::os::the_computer().to_string(),
            Side::Phone { label, .. } => label.clone(),
        }
    }

    /// Whether a job is running now.
    pub(super) fn running(self) -> bool {
        self.jobs.with_untracked(|js| js.iter().any(|j| j.state == JobState::Running))
    }

    /// How a job finds `src` again.
    pub(super) fn side(self, src: Src) -> Side {
        match (src, self.src_info(src)) {
            (Src::Dev(_), Some(i)) => Side::Phone { serial: i.serial, id: i.id, label: i.label },
            _ => Side::Mac,
        }
    }

    /// Where `side` is now: the computer, or the phone with that serial if it's connected.
    pub(super) fn src_of(self, side: &Side) -> Result<Src, String> {
        match side {
            Side::Mac => Ok(Src::Mac),
            Side::Phone { serial, id, label } => self
                .phones
                .with_untracked(|ps| {
                    ps.iter().find(|i| if serial.is_some() { i.serial == *serial } else { i.id == *id }).map(|i| Src::of(&i.id))
                })
                .ok_or_else(|| format!("{label} isn't connected. Connect it, then retry.")),
        }
    }

    pub(super) fn enqueue(self, pane: usize, work: Work, resume: bool) {
        let id = self.next_job.get_value();
        self.next_job.set_value(id + 1);
        self.jobs.update(|js| js.push(Job { id, work, pane, state: JobState::Queued, progress: None, resume }));
        if self.running() {
            self.panes[pane].notice.set(Notice::Ok("Queued: it starts when the copy before it is done.".into()));
        }
        self.save_jobs();
        self.pump();
    }

    /// Starts the first waiting job, unless one is running.
    fn pump(self) {
        if self.running() {
            return;
        }
        let Some(job) = self.jobs.with_untracked(|js| js.iter().find(|j| j.state == JobState::Queued).cloned()) else { return };
        self.set_state(job.id, JobState::Running);
        spawn_local(async move { self.run_job(job).await });
    }

    fn set_state(self, id: u32, state: JobState) {
        self.jobs.update(|js| {
            if let Some(j) = js.iter_mut().find(|j| j.id == id) {
                j.state = state;
            }
        });
        self.save_jobs();
    }

    async fn run_job(self, job: Job) {
        let p = self.panes[job.pane];
        let (from, to) = match &job.work {
            Work::Copy { from, to, .. } | Work::Sync { from, to, .. } | Work::Import { from, to, .. } => (self.src_of(from), self.src_of(to)),
        };
        let (from_src, to_src) = match (from, to) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(e), _) | (_, Err(e)) => {
                p.error(e.clone());
                self.set_state(job.id, JobState::Failed(e));
                self.pump();
                return;
            }
        };
        let r = match &job.work {
            Work::Copy { items, to_dir, moving, replace, .. } => {
                let mode = if *moving { "move" } else if *replace { "replace" } else { "copy" };
                let args = json!({
                    "job": job.id, "from_src": from_src.key(), "paths": items, "to_src": to_src.key(), "to_dir": to_dir,
                    "mode": mode, "resume": job.resume, "verify": self.verify.get_untracked(),
                });
                call::<TransferReport>("transfer", args).await
            }
            Work::Sync { from_dir, to_dir, hidden, mode, remove, .. } => {
                let args = json!({
                    "job": job.id, "from_src": from_src.key(), "from_dir": from_dir, "to_src": to_src.key(), "to_dir": to_dir, "hidden": hidden,
                    "verify": self.verify.get_untracked(), "mode": if mode.is_empty() { "copy" } else { mode }, "remove": remove,
                });
                call::<TransferReport>("sync", args).await
            }
            Work::Import { from_dir, to_dir, pattern, moving, .. } => {
                let args = json!({
                    "job": job.id, "from_src": from_src.key(), "from_dir": from_dir, "to_src": to_src.key(), "to_dir": to_dir,
                    "pattern": pattern, "hidden": self.show_hidden.get_untracked(), "moving": moving, "verify": self.verify.get_untracked(),
                });
                call::<TransferReport>("import_photos", args).await
            }
        };
        let state = match r {
            Ok(r) => {
                let lines = report_lines(&job.work, &r);
                match &job.work {
                    Work::Copy { moving: false, .. } if !r.made.is_empty() => {
                        self.log_undo(lines[0].clone(), Undo::Copy { to: to_src, made: r.made.clone() });
                    }
                    // A move goes back only when it went whole into new places: otherwise moving
                    // back would take files that were there before too.
                    Work::Copy { moving: true, items, .. }
                        if !r.cancelled && r.skipped.is_empty() && r.already == 0 && r.made.len() == items.len() =>
                    {
                        let back = r.made.iter().cloned().zip(items.iter().map(|x| parent(x))).collect();
                        self.log_undo(lines[0].clone(), Undo::Move { from: from_src, to: to_src, back });
                    }
                    _ => self.log(lines[0].clone()),
                }
                let msg = lines.join("\n");
                let left = r.cancelled || !r.skipped.is_empty();
                p.notice.set(if r.skipped.is_empty() { Notice::Ok(msg) } else { Notice::Error(msg) });
                left.then(|| JobState::Stopped(lines[1..].first().cloned().unwrap_or_else(|| lines[0].clone())))
            }
            Err(e) => {
                p.error(e.clone());
                Some(JobState::Failed(e))
            }
        };
        match state {
            Some(s) => self.set_state(job.id, s),
            // Done: it leaves the queue (the report is in the pane and History).
            None => self.drop_job(job.id),
        }
        match &job.work {
            Work::Copy { items, to_dir, moving, .. } => {
                self.changed(to_src, to_dir);
                if *moving {
                    let mut seen = vec![];
                    for d in items.iter().map(|x| parent(x)) {
                        if !seen.contains(&d) {
                            self.changed(from_src, &d);
                            seen.push(d);
                        }
                    }
                }
            }
            Work::Sync { to_dir, .. } => self.changed(to_src, to_dir),
            Work::Import { from_dir, to_dir, moving, .. } => {
                self.changed(to_src, to_dir);
                if *moving {
                    self.changed(from_src, from_dir);
                }
            }
        }
        self.pump();
    }

    /// Runs a stopped, failed or interrupted job again, at the end of the queue. Files it already
    /// copied are counted as done, not copied again.
    pub(super) fn retry_job(self, id: u32) {
        let Some(j) = self.jobs.with_untracked(|js| js.iter().find(|j| j.id == id).cloned()) else { return };
        self.drop_job(id);
        self.enqueue(j.pane, j.work, true);
    }

    /// Cancels a job: a waiting one leaves the queue; a running one stops after its current file.
    pub(super) fn cancel_job(self, id: u32) {
        let running = self.jobs.with_untracked(|js| js.iter().any(|j| j.id == id && j.state == JobState::Running));
        if running {
            spawn_local(async move {
                let _ = call::<()>("cancel_transfer", json!({ "job": id })).await;
            });
        } else {
            self.drop_job(id);
        }
    }

    pub(super) fn drop_job(self, id: u32) {
        self.jobs.update(|js| js.retain(|j| j.id != id));
        self.save_jobs();
    }

    /// Keeps the unfinished jobs (waiting, running, interrupted) for the next launch. Phones
    /// without a serial can't be found again then, so their jobs aren't kept.
    fn save_jobs(self) {
        let keep: Vec<Work> = self.jobs.with_untracked(|js| {
            js.iter()
                .filter(|j| matches!(j.state, JobState::Queued | JobState::Running | JobState::Paused))
                .map(|j| j.work.clone())
                .filter(|w| {
                    let (a, b) = match w {
                        Work::Copy { from, to, .. } | Work::Sync { from, to, .. } | Work::Import { from, to, .. } => (from, to),
                    };
                    [a, b].iter().all(|s| !matches!(s, Side::Phone { serial: None, .. }))
                })
                .collect()
        });
        store(JOBS_KEY, &serde_json::to_string(&keep).unwrap_or_default());
    }

    /// At launch: jobs the app quit during, offered to resume.
    pub(super) fn load_jobs(self) {
        let works: Vec<Work> = stored(JOBS_KEY).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        for work in works {
            let id = self.next_job.get_value();
            self.next_job.set_value(id + 1);
            self.jobs.update(|js| js.push(Job { id, work, pane: 0, state: JobState::Paused, progress: None, resume: true }));
        }
    }
}

/// The report of a finished job, first line first.
fn report_lines(work: &Work, r: &TransferReport) -> Vec<String> {
    let n = |c: u32| format!("{c} {}", if c == 1 { "file" } else { "files" });
    let mut lines = vec![match work {
        Work::Copy { replace: true, to_dir, .. } => format!("Replaced {} in {}. The files that were there are in Backups.", n(r.copied), to_dir),
        Work::Copy { moving, to_dir, .. } => format!("{} {} to {}.", if *moving { "Moved" } else { "Copied" }, n(r.copied), to_dir),
        Work::Sync { from_dir, to_dir, .. } => {
            let removed = if r.removed > 0 { format!(", removed {} {}", r.removed, if r.removed == 1 { "item" } else { "items" }) } else { String::new() };
            format!("Synced {} into {}: copied {}{removed}.", base(from_dir), to_dir, n(r.copied))
        }
        Work::Import { to_dir, moving, .. } => format!("{} {} into {}.", if *moving { "Imported and removed" } else { "Imported" }, n(r.copied), to_dir),
    }];
    if matches!(work, Work::Sync { .. }) && (!r.backups.is_empty() || r.removed > 0) {
        lines.push(format!("What was replaced or removed is in Backups on {}.", crate::os::the_computer()));
    }
    let moving = matches!(work, Work::Copy { moving: true, .. } | Work::Import { moving: true, .. });
    if r.already > 0 {
        lines.push(format!("{} already there from before{}.", n(r.already), if moving { "; their originals were kept" } else { "" }));
    }
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
    lines
}

/// A job in a few words: "Copy 3 items to Download".
fn job_title(w: &Work) -> String {
    match w {
        Work::Copy { items, to_dir, moving, replace, .. } => {
            let what = match items.as_slice() {
                [one] => base(one),
                many => format!("{} items", many.len()),
            };
            if *replace {
                return format!("Replace {what} in {}", base(to_dir));
            }
            format!("{} {what} to {}", if *moving { "Move" } else { "Copy" }, base(to_dir))
        }
        Work::Sync { from_dir, to_dir, mode, .. } => {
            let verb = match mode.as_str() {
                "update" => "Update",
                "mirror" => "Mirror",
                _ => "Sync",
            };
            format!("{verb} {} into {}", base(from_dir), base(to_dir))
        }
        Work::Import { from_dir, to_dir, .. } => format!("Import photos from {} into {}", base(from_dir), base(to_dir)),
    }
}

/// The transfer queue below the panes: the running job with its progress, then the waiting ones,
/// and the ones to retry.
#[component]
pub(super) fn TransferBar() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    move || {
        let jobs = ctx.jobs.get();
        (!jobs.is_empty()).then(|| view! {
            <div class="transfers">
                {jobs.into_iter().map(|j| {
                    let id = j.id;
                    let title = job_title(&j.work);
                    match j.state {
                        JobState::Running => {
                            let verb = match &j.work {
                                Work::Copy { moving: true, .. } => "Moving",
                                Work::Copy { replace: true, .. } => "Replacing",
                                Work::Copy { .. } => "Copying",
                                Work::Sync { .. } => "Syncing",
                                Work::Import { .. } => "Importing",
                            };
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
                                    <span class="text" title=title>{text}</span>
                                    <div class="meter"><div style=format!("width:{pct:.1}%")></div></div>
                                    <button class="btn icon-only" title="Cancel: stops after the file being copied now" aria-label="Cancel"
                                        on:click=move |_| ctx.cancel_job(id)>{ico(Icon::Stop)}</button>
                                </div>
                            }.into_any()
                        }
                        state => {
                            let (what, retry) = match state {
                                JobState::Queued => ("Waiting".to_string(), None),
                                JobState::Paused => ("Not finished when the app quit".to_string(), Some("Resume")),
                                JobState::Stopped(why) => (why, Some("Retry")),
                                JobState::Failed(why) => (why, Some("Retry")),
                                JobState::Running => unreachable!(),
                            };
                            let tip = format!("{title}: {what}");
                            view! {
                                <div class="transfer queued">
                                    <span class="text" title=tip><b>{title}</b>" · "{what}</span>
                                    {retry.map(|label| view! {
                                        <button class="btn small" title="Run it again: files already copied are kept, the rest is copied"
                                            on:click=move |_| ctx.retry_job(id)>{ico(Icon::Refresh)}{label}</button>
                                    })}
                                    <button class="btn small" title="Take it off the list" aria-label="Remove"
                                        on:click=move |_| ctx.cancel_job(id)>{ico(Icon::Close)}</button>
                                </div>
                            }.into_any()
                        }
                    }
                }).collect_view()}
            </div>
        })
    }
}
