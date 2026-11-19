//! The text editor: opening, saving and closing a text file.

use super::*;

impl Ctx {
    pub(super) fn open_text(self, p: Pane, path: String) {
        let src = p.src.get_untracked();
        self.busy.set(true);
        spawn_local(async move {
            match call::<TextFile>("read_text", json!({ "src": src.key(), "path": path })).await {
                Ok(f) => {
                    self.text.set(f.text.clone());
                    self.editor.set(Some(Editor { src, pane: p.id, path, version: f.version, saved_text: f.text }));
                    self.status.set((String::new(), ""));
                    self.confirm_discard.set(false);
                }
                Err(e) => p.error(e),
            }
            self.busy.set(false);
        });
    }

    pub(super) fn save(self) {
        let Some(ed) = self.editor.get_untracked() else { return };
        if self.busy.get_untracked() {
            return;
        }
        let text = self.text.get_untracked();
        self.busy.set(true);
        self.status.set(("Saving…".into(), ""));
        spawn_local(async move {
            let args = json!({ "src": ed.src.key(), "path": ed.path, "text": text, "base_version": ed.version, "create": false });
            match call::<WriteResult>("write_text", args).await {
                Ok(r) => {
                    let note = if r.backup.is_some() { format!(" The old version is backed up on {}.", crate::os::the_computer()) } else { String::new() };
                    self.status.set((format!("Saved to {}.{note}", self.place(ed.src)), "ok"));
                    self.log(format!("Saved {} on {}", base(&ed.path), self.place(ed.src)));
                    self.editor.set(Some(Editor { version: r.version, saved_text: text, ..ed }));
                    self.confirm_discard.set(false);
                }
                Err(e) => self.status.set((e, "error")),
            }
            self.busy.set(false);
        });
    }

    pub(super) fn close_editor(self, force: bool) {
        if !force && self.dirty() {
            self.confirm_discard.set(true);
            return;
        }
        self.leave_editor();
        self.confirm_discard.set(false);
        self.reload_all();
    }

    /// Closes the editor and lets go of its text (a file of up to a few MB, held twice).
    pub(super) fn leave_editor(self) {
        self.editor.set(None);
        self.text.set(String::new());
    }
}

#[component]
pub(super) fn TextEditor() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let path = move || ctx.editor.get().map(|e| e.path).unwrap_or_default();
    let dirty = Memo::new(move |_| ctx.dirty());
    view! {
        <div class="bar">
            <button class="btn back" title="Back to files" aria-label="Back to files" on:click=move |_| ctx.close_editor(false)><svg class="back-icon" viewBox="0 0 16 16" aria-hidden="true"><path d="M13 8H3M7.5 3.5 3 8l4.5 4.5"/></svg></button>
            <span class="title">{path}{move || dirty.get().then_some(" •")}</span>
            <span class="kind">{move || ctx.editor.get().map(|e| match e.src {
                Src::Mac => crate::os::computer().to_string(),
                src => ctx.src_info(src).map_or_else(|| "Device".into(), |i| i.label),
            })}</span>
            <span class="spacer"></span>
            <button class="btn primary" title=crate::os::keys("⌘S") disabled=move || !dirty.get() || ctx.busy.get() on:click=move |_| ctx.save()>{ico(Icon::Save)}"Save"</button>
        </div>
        {move || ctx.confirm_discard.get().then(|| view! {
            <div class="notice">
                <span class="text">"You have unsaved changes."</span>
                <button class="btn primary" on:click=move |_| ctx.save()>{ico(Icon::Save)}"Save"</button>
                <button class="btn danger" on:click=move |_| ctx.close_editor(true)>{ico(Icon::Trash)}"Discard changes"</button>
                <button class="btn" on:click=move |_| ctx.confirm_discard.set(false)>"Keep editing"</button>
            </div>
        })}
        <div class="editor">
            <textarea spellcheck="false" prop:value=move || ctx.text.get()
                on:input=move |ev| ctx.text.set(event_target_value(&ev))></textarea>
        </div>
        <div class="statusbar">
            <span class=move || format!("status {}", ctx.status.get().1)>{move || {
                let (msg, _) = ctx.status.get();
                if !msg.is_empty() { msg } else if dirty.get() { "Unsaved changes".into() } else { "Saved".into() }
            }}</span>
            <span class="spacer"></span>
            <span>{move || ctx.text.with(|t| {
                let lines = t.lines().count().max(1);
                let chars = t.chars().count();
                format!("{lines} {} · {chars} {}", if lines == 1 { "line" } else { "lines" }, if chars == 1 { "character" } else { "characters" })
            })}</span>
        </div>
    }
}
