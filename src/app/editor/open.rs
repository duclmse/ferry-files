//! Opening, saving and closing text files, and keeping each tab's text.

use super::*;

impl Ctx {
    pub(in crate::app) fn open_text(self, p: Pane, path: String) {
        self.open_text_on(p, p.src.get_untracked(), path);
    }

    /// Opens text file `path` on `src` in the editor, from pane `p` (a script, from Settings).
    pub(in crate::app) fn open_text_on(self, p: Pane, src: Src, path: String) {
        self.busy.set(true);
        spawn_local(async move {
            match call::<TextFile>("read_text", json!({ "src": src.key(), "path": path })).await {
                Ok(f) => {
                    let (text, crlf, mixed) = to_lf(f.text);
                    self.editor.set(Some(Editor {
                        src,
                        pane: p.id,
                        path,
                        version: f.version,
                        saved_text: text.as_str().into(),
                        crlf,
                        mixed,
                    }));
                    self.text.set(text);
                    self.status.set((String::new(), ""));
                    self.confirm_discard.set(false);
                    self.finding.set(false);
                }
                Err(e) => p.error(e),
            }
            self.busy.set(false);
        });
    }

    pub(in crate::app) fn save(self) {
        self.save_in(self.full_slot());
    }

    pub(in crate::app) fn save_in(self, slot: Slot) {
        let Some(ed) = slot.editor.get_untracked() else {
            return;
        };
        if self.busy.get_untracked() {
            return;
        }
        let text = slot.text.get_untracked();
        self.busy.set(true);
        slot.status.set(("Saving…".into(), ""));
        spawn_local(async move {
            // With the file's own line endings.
            let crlf = ed.crlf.then(|| text.replace('\n', "\r\n"));
            let args = json!({ "src": ed.src.key(), "path": ed.path, "text": crlf.as_ref().unwrap_or(&text), "base_version": ed.version, "create": false });
            drop(crlf);
            match call::<WriteResult>("write_text", args).await {
                Ok(r) => {
                    let note = if r.backup.is_some() {
                        format!(
                            " The old version is backed up on {}.",
                            crate::os::the_computer()
                        )
                    } else {
                        String::new()
                    };
                    slot.status
                        .set((format!("Saved to {}.{note}", self.place(ed.src)), "ok"));
                    self.log(format!(
                        "Saved {} on {}",
                        base(&ed.path),
                        self.place(ed.src)
                    ));
                    let same = |e: &Editor| e.src == ed.src && e.path == ed.path;
                    let saved = Editor {
                        version: r.version,
                        saved_text: text.into(),
                        ..ed.clone()
                    };
                    if slot.editor.with_untracked(|e| e.as_ref().is_some_and(same)) {
                        slot.editor.set(Some(saved));
                        slot.ask.set(false);
                    } else if let Some(p) = slot.pane {
                        // Another tab was shown meanwhile: the saved file is in the stash.
                        p.docs.update(|ds| {
                            for d in ds {
                                if let Doc::Text { ed, .. } = d {
                                    if same(ed) {
                                        *ed = saved.clone();
                                    }
                                }
                            }
                        });
                    }
                }
                Err(e) => slot.status.set((e, "error")),
            }
            self.busy.set(false);
        });
    }

    /// Opens a text file in the pane's Editor view: in its tab if it's open, else in place of the
    /// shown file, or in a new tab when the shown file has unsaved changes.
    pub(in crate::app) fn open_doc(self, p: Pane, path: String) {
        let src = p.src.get_untracked();
        reveal(p, &path);
        if let Some(i) = p
            .docs
            .with_untracked(|ds| ds.iter().position(|d| d.src() == src && d.path() == path))
        {
            self.show_doc(p, i);
            return;
        }
        self.busy.set(true);
        spawn_local(async move {
            match call::<TextFile>("read_text", json!({ "src": src.key(), "path": path })).await {
                Ok(f) => {
                    let (text, crlf, mixed) = to_lf(f.text);
                    let ed = Editor {
                        src,
                        pane: p.id,
                        path,
                        version: f.version,
                        saved_text: text.as_str().into(),
                        crlf,
                        mixed,
                    };
                    // Its text is in the editor while it's shown.
                    let doc = Doc::Text {
                        ed: ed.clone(),
                        text: None,
                    };
                    let at = p.doc_at.get_untracked();
                    // A shown file with changes, or one that isn't text (kept open), stays.
                    let keep = p.slot().dirty()
                        || p.docs
                            .with_untracked(|ds| matches!(ds.get(at), Some(Doc::File { .. })));
                    stash(p);
                    p.docs.update(|ds| {
                        if ds.is_empty() {
                            ds.push(doc);
                        } else if keep {
                            ds.insert(at + 1, doc);
                        } else {
                            let last = ds.len() - 1;
                            ds[at.min(last)] = doc;
                        }
                    });
                    let at = if keep {
                        at + 1
                    } else {
                        at.min(p.docs.with_untracked(Vec::len) - 1)
                    };
                    show(p, at, ed, text);
                    p.peek_on.set(false);
                }
                Err(e) => p.error(e),
            }
            self.busy.set(false);
        });
    }

    /// Shows a file that isn't text in the Editor view's preview tab, in place of the one there;
    /// or in its own tab, when it's kept open.
    pub(in crate::app) fn peek_doc(self, p: Pane, e: Entry) {
        let src = p.src.get_untracked();
        if let Some(i) = p.docs.with_untracked(|ds| {
            ds.iter()
                .position(|d| matches!(d, Doc::File { .. }) && d.src() == src && d.path() == e.path)
        }) {
            self.show_doc(p, i);
            return;
        }
        reveal(p, &e.path);
        p.doc_peek.set(Some(e));
        p.peek_on.set(true);
    }

    /// Keeps the preview tab's file open in a tab of its own, after the shown one.
    pub(in crate::app) fn keep_peek(self, p: Pane) {
        let Some(entry) = p.doc_peek.get_untracked() else {
            return;
        };
        let src = p.src.get_untracked();
        let n = p.docs.with_untracked(Vec::len);
        let at = if n == 0 {
            0
        } else {
            (p.doc_at.get_untracked() + 1).min(n)
        };
        p.docs.update(|ds| ds.insert(at, Doc::File { src, entry }));
        if p.doc_at.get_untracked() >= at && n > 0 {
            p.doc_at.update(|a| *a += 1);
        }
        p.doc_peek.set(None);
        self.show_doc(p, at);
    }

    /// Closes the Editor view's preview tab.
    pub(in crate::app) fn close_peek(self, p: Pane) {
        p.doc_peek.set(None);
        p.peek_on.set(false);
    }

    /// After `old` on `src` was renamed or moved to `new`: open files in it (or it) and the
    /// preview tab follow it.
    pub(in crate::app) fn doc_moved(self, src: Src, old: &str, new: &str) {
        let moved = |path: &mut String| {
            if let Some(rest) = path
                .strip_prefix(old)
                .filter(|r| r.is_empty() || r.starts_with('/'))
            {
                *path = format!("{new}{rest}");
            }
        };
        for p in self.panes {
            p.docs.update(|ds| {
                for d in ds.iter_mut().filter(|d| d.src() == src) {
                    match d {
                        Doc::Text { ed, .. } => moved(&mut ed.path),
                        Doc::File { entry, .. } => {
                            moved(&mut entry.path);
                            entry.name = base(&entry.path);
                        }
                    }
                }
            });
            p.doc.update(|d| {
                if let Some(ed) = d.as_mut().filter(|ed| ed.src == src) {
                    moved(&mut ed.path);
                }
            });
            if p.src.get_untracked() == src {
                p.doc_peek.update(|e| {
                    if let Some(e) = e.as_mut() {
                        moved(&mut e.path);
                        e.name = base(&e.path);
                    }
                });
            }
        }
    }

    /// Shows the Editor view's tab `i`.
    pub(in crate::app) fn show_doc(self, p: Pane, i: usize) {
        p.peek_on.set(false);
        let n = p.docs.with_untracked(Vec::len);
        let here = i == p.doc_at.get_untracked()
            && (p.doc.with_untracked(Option::is_some)
                || p.docs
                    .with_untracked(|ds| matches!(ds.get(i), Some(Doc::File { .. }))));
        if here || i >= n {
            return;
        }
        stash(p);
        put(p, i);
    }

    /// Closes the Editor view's tab `i`; with unsaved changes, unless `force`, it's shown and asks.
    pub(in crate::app) fn close_doc(self, p: Pane, i: usize, force: bool) {
        let at = p.doc_at.get_untracked();
        let dirty = if i == at {
            p.slot().dirty()
        } else {
            p.docs
                .with_untracked(|ds| ds.get(i).is_some_and(Doc::dirty))
        };
        if dirty && !force {
            self.show_doc(p, i);
            p.doc_ask.set(true);
            return;
        }
        if i >= p.docs.with_untracked(Vec::len) {
            return;
        }
        p.docs.update(|ds| {
            ds.remove(i);
        });
        let n = p.docs.with_untracked(Vec::len);
        if i == at {
            let next = i.min(n.saturating_sub(1));
            match n {
                1.. => put(p, next),
                0 => {
                    p.doc.set(None);
                    p.doc_text.set(String::new());
                    p.doc_at.set(0);
                    p.doc_ask.set(false);
                    // The preview tab, if there's one, is what's left to show.
                    if p.doc_peek.with_untracked(Option::is_some) {
                        p.peek_on.set(true);
                    }
                }
            }
        } else if i < at {
            p.doc_at.set(at - 1);
        }
    }

    pub(in crate::app) fn close_editor(self, force: bool) {
        if !force && self.dirty() {
            self.confirm_discard.set(true);
            return;
        }
        self.leave_editor();
        self.confirm_discard.set(false);
        self.reload_all();
    }

    /// Closes the editor and lets go of its text (a file of up to a few MB, held twice).
    pub(in crate::app) fn leave_editor(self) {
        self.editor.set(None);
        self.text.set(String::new());
    }
}

/// Keeps the shown tab's file in its entry of `docs`, and its text if it has changes: unchanged,
/// it's made again from the saved text when shown.
fn stash(p: Pane) {
    let Some(ed) = p.doc.get_untracked() else {
        return;
    };
    let at = p.doc_at.get_untracked();
    let text = p
        .doc_text
        .try_update_untracked(std::mem::take)
        .unwrap_or_default();
    let text = (text != *ed.saved_text).then_some(text);
    p.docs.update(|ds| {
        if let Some(d) = ds.get_mut(at) {
            *d = Doc::Text { ed, text };
        }
    });
}

/// Shows tab `at`: a text file in the editor (its text moved there, not copied), or a kept file
/// as the preview shows it.
fn put(p: Pane, at: usize) {
    let taken = p.docs.try_update(|ds| {
        ds.get_mut(at).map(|d| match d {
            Doc::Text { ed, text } => (
                ed.path.clone(),
                Some((
                    ed.clone(),
                    text.take().unwrap_or_else(|| ed.saved_text.to_string()),
                )),
            ),
            Doc::File { entry, .. } => (entry.path.clone(), None),
        })
    });
    let Some(Some((path, text))) = taken else {
        return;
    };
    reveal(p, &path);
    match text {
        Some((ed, text)) => show(p, at, ed, text),
        None => {
            p.doc.set(None);
            p.doc_text.set(String::new());
            p.doc_at.set(at);
            p.doc_ask.set(false);
            p.doc_status.set((String::new(), ""));
        }
    }
}

/// Puts tab `at`'s file in the editor. Its text goes first: the editor reads it when it's made.
fn show(p: Pane, at: usize, ed: Editor, text: String) {
    p.doc_text.set(text);
    p.doc.set(Some(ed));
    p.doc_at.set(at);
    p.doc_ask.set(false);
    p.doc_status.set((String::new(), ""));
}

/// Opens the folders in the Editor view's tree down to `path`, so its row is shown.
fn reveal(p: Pane, path: &str) {
    let root = p.cwd.get_untracked();
    let mut dir = parent(path);
    let mut open = vec![];
    while dir.len() > root.len() && dir.starts_with(&root) {
        open.push(dir.clone());
        dir = parent(&dir);
    }
    if !open.is_empty() {
        p.tree_open.update(|o| o.extend(open));
    }
}
