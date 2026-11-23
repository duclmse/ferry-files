//! Comparing the two panes' folders: each item marked only here, newer, older, differs or the
//! same as the other side's item of that name, with a bar to show only the differences and to copy
//! across what's missing or newer.

use super::*;

impl Ctx {
    /// Marks the items of both panes against each other (again, if they were already).
    pub(super) fn compare_marks(self, p: Pane) {
        let o = self.other(p);
        if !self.split.get_untracked() {
            return;
        }
        let (a, b) = ((p.src.get_untracked(), p.cwd.get_untracked()), (o.src.get_untracked(), o.cwd.get_untracked()));
        if a.1.is_empty() || b.1.is_empty() {
            return;
        }
        // Pane 0's view is "a" whichever pane asked, so the two never swap sides between runs.
        let (first, second) = if p.id == 0 { (p, o) } else { (o, p) };
        let ((a_src, a_dir), (b_src, b_dir)) = if p.id == 0 { (a, b) } else { (b, a) };
        self.comparing.set(true);
        let args = json!({
            "a_src": a_src.key(), "a_dir": a_dir, "b_src": b_src.key(), "b_dir": b_dir,
            "hidden": self.show_hidden.get_untracked(), "content": self.cmp_content.get_untracked(),
        });
        spawn_local(async move {
            let r = call::<Marks>("compare_marks", args).await;
            self.comparing.set(false);
            // Not if either pane went elsewhere meanwhile.
            if first.cwd.get_untracked() != a_dir || second.cwd.get_untracked() != b_dir {
                return;
            }
            match r {
                Ok(m) => {
                    first.cmp.set(Some(m.a));
                    second.cmp.set(Some(m.b));
                    first.arrange();
                    second.arrange();
                }
                Err(e) => p.error(e),
            }
        });
    }

    /// Clears both panes' marks.
    pub(super) fn stop_compare(self) {
        for p in self.panes {
            if p.cmp.with_untracked(Option::is_some) {
                p.cmp.set(None);
                p.diff_only.set(false);
                p.arrange();
            }
        }
    }

    /// The paths of pane `p`'s items marked `mark` (files only, unless `folders`).
    fn with_mark(p: Pane, mark: &str, folders: bool) -> Vec<String> {
        let marks = p.cmp.get_untracked().unwrap_or_default();
        p.all
            .with_untracked(|all| all.iter().filter(|e| (folders || !e.dir) && marks.get(&e.name).is_some_and(|m| m == mark)).map(|e| e.path.clone()).collect())
    }

    /// Copies what's only in `p`'s folder to the other pane's.
    fn copy_only_here(self, p: Pane) {
        let o = self.other(p);
        let items = Self::with_mark(p, "only", true);
        if !items.is_empty() {
            self.transfer(p, p.src.get_untracked(), items, o.src.get_untracked(), o.cwd.get_untracked(), false);
        }
    }

    /// Writes `p`'s newer files over the other pane's older ones, which are backed up first.
    fn replace_older(self, p: Pane) {
        let o = self.other(p);
        let items = Self::with_mark(p, "newer", false);
        let to_dir = o.cwd.get_untracked();
        if items.is_empty() {
            return;
        }
        if self.read_only(o.src.get_untracked()) || to_dir == "/" && self.src_info(o.src.get_untracked()).is_some_and(|i| i.transport == "mtp") {
            return;
        }
        let work = Work::Copy { from: self.side(p.src.get_untracked()), items, to: self.side(o.src.get_untracked()), to_dir, moving: false, replace: true };
        self.enqueue(p.id, work, false);
    }
}

/// The mark of `e` in pane `p`, for its row's `data-cmp` (only for items of the folder itself, not
/// a column view's other columns).
pub(super) fn cmp_mark(p: Pane, e: &Entry) -> Option<String> {
    if parent(&e.path) != p.cwd.get() {
        return None;
    }
    p.cmp.with(|m| m.as_ref().and_then(|m| m.get(&e.name).cloned()))
}

/// Above the files while comparing: counts, "Differences only", and copying across.
#[component]
pub(super) fn CompareBar(pane: Pane) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let p = pane;
    let count = move |mark: &str| p.cmp.with(|m| m.as_ref().map_or(0, |m| m.values().filter(|x| *x == mark).count()));
    let files = move |mark: &'static str| {
        p.cmp.track();
        Ctx::with_mark(p, mark, false).len()
    };
    move || {
        p.cmp.with(Option::is_some).then(|| {
            let (only, newer, older, differs, same) = (count("only"), count("newer"), count("older"), count("differs"), count("same"));
            let mut parts = vec![];
            for (n, what) in [(only, "only here"), (newer, "newer here"), (older, "older here"), (differs, "different"), (same, "the same")] {
                if n > 0 {
                    parts.push(format!("{n} {what}"));
                }
            }
            let summary = if parts.is_empty() { "Both folders are empty.".to_string() } else { parts.join(" · ") };
            let replace_n = files("newer");
            let there = base(&ctx.other(p).cwd.get());
            view! {
                <div class="compare-bar">
                    <span class="summary" title="The stripe by each item: blue only here, green newer here, orange older here, red different. Items that are the same are faded.">{move || if ctx.comparing.get() { "Comparing…".to_string() } else { summary.clone() }}</span>
                    <label class="row" title="Hide the items that are the same on both sides">
                        <input type="checkbox" prop:checked=move || p.diff_only.get()
                            on:change=move |e| { p.diff_only.set(event_target_checked(&e)); p.arrange(); }/>"Differences only"</label>
                    <label class="row" title="Compare files of the same size byte by byte too (slower)">
                        <input type="checkbox" prop:checked=move || ctx.cmp_content.get()
                            on:change=move |e| { ctx.cmp_content.set(event_target_checked(&e)); ctx.compare_marks(p); }/>"By content"</label>
                    <span class="grow"></span>
                    {(only > 0).then(|| view! {
                        <button class="btn small" title=format!("Copy what's only here into {there}") on:click=move |_| ctx.copy_only_here(p)>
                            {format!("Copy {only} to {there}")}</button>
                    })}
                    {(replace_n > 0).then(|| view! {
                        <button class="btn small" title=format!("Write the newer files here over the older ones in {there}. Those are backed up first.")
                            on:click=move |_| ctx.replace_older(p)>{format!("Replace {replace_n} older in {there}")}</button>
                    })}
                    <button class="btn small" title="Compare again" disabled=move || ctx.comparing.get() on:click=move |_| ctx.compare_marks(p)>"Again"</button>
                    <button class="btn small" title="Stop comparing" on:click=move |_| ctx.stop_compare()>"Done"</button>
                </div>
            }
        })
    }
}
