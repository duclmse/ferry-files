//! Undo: what the last file actions did, and doing the opposite. Each undo first checks that things
//! are still as the action left them; if not, it says what changed and does nothing.

use super::*;

/// How to undo one action.
#[derive(Clone)]
pub(super) enum Undo {
    /// Renamed items: (path now, name before).
    Renames { src: Src, items: Vec<(String, String)> },
    /// A new, empty file or folder.
    Create { src: Src, path: String, folder: bool },
    /// Deleted items, in the order they went: (path, was a folder, backup of a file).
    Delete { src: Src, items: Vec<(String, bool, Option<String>)> },
    /// What a copy made (files, or folders with everything in them).
    Copy { to: Src, made: Vec<String> },
    /// Moved items: (path now, folder they came from).
    Move { from: Src, to: Src, back: Vec<(String, String)> },
}

/// An action in the session's history.
#[derive(Clone)]
pub(super) struct Op {
    pub at: String,
    pub text: String,
    pub undo: Option<Undo>,
    pub undone: bool,
}

/// The file at `path` on `src`, if there is one.
async fn stat(src: Src, path: &str) -> Result<Option<Entry>, String> {
    let list = call::<Vec<Entry>>("list", json!({ "src": src.key(), "path": parent(path), "current": false })).await?;
    Ok(list.into_iter().find(|e| e.path == path))
}

impl Ctx {
    /// Notes an action that can be undone.
    pub(super) fn log_undo(self, what: impl Into<String>, undo: Undo) {
        self.push_op(what.into(), Some(undo));
    }

    pub(super) fn push_op(self, text: String, undo: Option<Undo>) {
        let d = js_sys::Date::new_0();
        let at = format!("{:02}:{:02}:{:02}", d.get_hours(), d.get_minutes(), d.get_seconds());
        self.history.update(|h| h.push(Op { at, text, undo, undone: false }));
    }

    /// The newest action that can still be undone (its index in `history`).
    pub(super) fn last_undo(self) -> Option<(usize, String)> {
        self.history.with_untracked(|h| {
            h.iter().enumerate().rev().find(|(_, o)| o.undo.is_some() && !o.undone).map(|(i, o)| (i, o.text.clone()))
        })
    }

    /// ⌘Z: undoes the newest action that can be, with the result in the active pane.
    pub(super) fn undo_last(self) {
        let p = self.act();
        match self.last_undo() {
            Some((i, _)) => self.undo(i, p),
            None => p.notice.set(Notice::Ok("Nothing to undo.".into())),
        }
    }

    /// Undoes action `i` of the history; says how it went in pane `p`.
    pub(super) fn undo(self, i: usize, p: Pane) {
        let Some(op) = self.history.with_untracked(|h| h.get(i).cloned()) else { return };
        let Some(u) = op.undo.clone() else { return };
        if op.undone {
            return;
        }
        // Not twice, even while this one is still going.
        self.history.update(|h| h[i].undone = true);
        spawn_local(async move {
            match self.run_undo(u).await {
                Ok(done) => {
                    self.push_op(format!("Undid: {}", op.text), None);
                    p.notice.set(Notice::Ok(done));
                }
                Err(e) => {
                    self.history.update(|h| h[i].undone = false);
                    p.error(format!("Couldn't undo “{}”: {e}", op.text));
                }
            }
        });
    }

    async fn run_undo(self, u: Undo) -> Result<String, String> {
        match u {
            Undo::Renames { src, items } => {
                for (now, _) in &items {
                    if stat(src, now).await?.is_none() {
                        return Err(format!("{} isn't there any more.", base(now)));
                    }
                }
                let (back, err) = batch::rename_all(src, items.iter().map(|(now, old)| (now.clone(), old.clone())).collect()).await;
                let mut dirs: Vec<String> = back.iter().map(|(now, _)| parent(now)).collect();
                dirs.sort();
                dirs.dedup();
                for d in dirs {
                    self.changed(src, &d);
                }
                if let Some(e) = err {
                    return Err(if back.is_empty() { e } else { format!("Renamed {} back, then: {e}", back.len()) });
                }
                Ok(match items.as_slice() {
                    [(_, old)] => format!("Renamed back to {old}."),
                    _ => format!("Renamed {} items back.", back.len()),
                })
            }
            Undo::Create { src, path, folder } => {
                match stat(src, &path).await? {
                    None => return Err(format!("{} isn't there any more.", base(&path))),
                    Some(e) if !folder && e.size > 0 => return Err(format!("{} has been written to since. Delete it yourself if you mean to.", base(&path))),
                    Some(_) => {}
                }
                call::<Option<String>>("remove", json!({ "src": src.key(), "path": path })).await?;
                self.changed(src, &parent(&path));
                Ok(format!("Removed {}.", base(&path)))
            }
            Undo::Delete { src, items } => {
                for (path, _, _) in &items {
                    if stat(src, path).await?.is_some() {
                        return Err(format!("Something called {} is there again.", base(path)));
                    }
                }
                // Folders first (outermost first), then the files back into them.
                let mut dirs: Vec<_> = items.iter().filter(|x| x.1).map(|x| x.0.clone()).collect();
                dirs.sort_by_key(|d| d.matches('/').count());
                for d in &dirs {
                    call::<()>("mkdir", json!({ "src": src.key(), "path": d })).await?;
                }
                for (path, _, backup) in items.iter().filter(|x| !x.1) {
                    let Some(file) = backup else { return Err(format!("There's no backup of {}.", base(path))) };
                    call::<WriteResult>("restore", json!({ "file": file, "src": src.key(), "path": path })).await?;
                }
                if let Some((path, _, _)) = items.first() {
                    self.changed(src, &parent(path));
                }
                Ok(match items.as_slice() {
                    [(path, _, _)] => format!("Put {} back.", base(path)),
                    _ => format!("Put {} items back.", items.len()),
                })
            }
            Undo::Copy { to, made } => {
                for path in &made {
                    if stat(to, path).await?.is_none() {
                        return Err(format!("{} isn't there any more.", base(path)));
                    }
                }
                for path in &made {
                    call::<u32>("remove_tree", json!({ "src": to.key(), "path": path })).await?;
                    self.changed(to, &parent(path));
                }
                Ok(format!("Removed the copies. Their files are in Backups on {}.", crate::os::the_computer()))
            }
            Undo::Move { from, to, back } => {
                for (now, _) in &back {
                    if stat(to, now).await?.is_none() {
                        return Err(format!("{} isn't there any more.", base(now)));
                    }
                }
                // One move back per folder they came from, through the transfer queue.
                let mut groups: Vec<(String, Vec<String>)> = vec![];
                for (now, dir) in back {
                    match groups.iter_mut().find(|g| g.0 == dir) {
                        Some(g) => g.1.push(now),
                        None => groups.push((dir, vec![now])),
                    }
                }
                let p = self.act();
                for (dir, items) in groups {
                    self.transfer(p, to, items, from, dir, true);
                }
                Ok("Moving them back.".into())
            }
        }
    }
}
