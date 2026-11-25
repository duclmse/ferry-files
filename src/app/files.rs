//! Working on files: opening, download and upload, delete, rename, new file or folder.

use super::*;

impl Ctx {
    pub(super) fn open(self, p: Pane, e: Entry) {
        let src = p.src.get_untracked();
        match kind(&e) {
            Kind::Folder => self.navigate(p, e.path),
            // A book opens for editing in the preview.
            Kind::Epub => {
                p.loading.set(true);
                p.selected.set(Some(e.path.clone()));
                p.preview.set(true);
                let ep = self.ep;
                spawn_local(async move {
                    if let Err(err) = ep.open(&src.key(), self.place(src), e.path, p.id).await {
                        p.error(err);
                    }
                    p.loading.set(false);
                });
            }
            Kind::Text if p.view.get_untracked() == ViewMode::Code => self.open_doc(p, e.path),
            Kind::Text => self.open_text(p, e.path),
            Kind::Other => self.open_in_app(p, e.path),
        }
    }

    /// Shows the book at `path` on `src` in the active pane, picked, and opens it for editing in
    /// the preview.
    pub(super) fn edit_book(self, src: Src, path: String) {
        let p = self.act();
        let dir = parent(&path);
        if p.src.get_untracked() != src || p.cwd.get_untracked() != dir {
            self.navigate_select(p, src, dir, Some(path.clone()));
        } else {
            p.selected.set(Some(path.clone()));
        }
        p.preview.set(true);
        let ep = self.ep;
        spawn_local(async move {
            if let Err(err) = ep.open(&src.key(), self.place(src), path, p.id).await {
                p.error(err);
            }
        });
    }

    /// Shows the book `path` on `src` in the preview, its chapters to read.
    pub(super) fn read_book(self, src: Src, path: String) {
        let p = self.act();
        let dir = parent(&path);
        if p.src.get_untracked() != src || p.cwd.get_untracked() != dir {
            self.navigate_select(p, src, dir, Some(path.clone()));
        } else {
            p.selected.set(Some(path.clone()));
        }
        p.preview.set(true);
        if self.ep.host.get_untracked() == Some(p.id) && self.ep.holds(&src.key(), &path) {
            self.ep.close(false);
        }
        self.epub_read.set(true);
        store(preview::EPUB_READ_KEY, "1");
    }

    /// Opens a file in its default Mac app. A phone file is copied to the Mac first; that copy is
    /// what the app sees, so changes to it don't go back to the phone.
    pub(super) fn open_in_app(self, p: Pane, path: String) {
        self.open_in_app_from(p, p.src.get_untracked(), path);
    }

    /// As `open_in_app`, for a file on `src` rather than the pane's place.
    pub(super) fn open_in_app_from(self, p: Pane, src: Src, path: String) {
        p.loading.set(true);
        spawn_local(async move {
            match call::<String>("open_file", json!({ "src": src.key(), "path": path })).await {
                Ok(_) if src != Src::Mac => p.notice.set(Notice::Ok(format!(
                    "Opened a copy of {}. Changes to it stay on {}.",
                    base(&path),
                    crate::os::the_computer()
                ))),
                Ok(_) => {}
                Err(e) => p.error(e),
            }
            p.loading.set(false);
        });
    }

    pub(super) fn download(self, p: Pane, path: String) {
        let src = p.src.get_untracked();
        spawn_local(async move {
            match call::<Option<String>>("download", json!({ "src": src.key(), "path": path }))
                .await
            {
                Ok(Some(dest)) => p.notice.set(Notice::Ok(format!("Saved to {dest}"))),
                Ok(None) => {}
                Err(e) => p.error(e),
            }
        });
    }

    pub(super) fn upload(self, p: Pane) {
        let (src, dir) = (p.src.get_untracked(), p.cwd.get_untracked());
        self.busy.set(true);
        spawn_local(async move {
            match call::<Vec<String>>("upload", json!({ "src": src.key(), "dir": dir })).await {
                Ok(lines) if lines.is_empty() => {}
                Ok(lines) => {
                    p.notice.set(Notice::Ok(lines.join("\n")));
                    self.changed(src, &dir);
                }
                Err(e) => p.error(e),
            }
            self.busy.set(false);
        });
    }

    /// Deletes what's picked. On the computer that's to the Trash, straight away, as in Finder
    /// and File Explorer; `forever` (only there) and anywhere else it asks first.
    pub(super) fn ask_delete(self, p: Pane, forever: bool) {
        let src = p.src.get_untracked();
        if self.read_only(src) {
            p.error("Files on an Everything server can only be read, not deleted.");
            return;
        }
        if forever && src != Src::Mac {
            let pc = crate::os::the_computer();
            p.error(format!("Only files on {pc} can be deleted permanently. Here Delete always keeps a backup on {pc}."));
            return;
        }
        let items = p.picked();
        if items.is_empty() {
            return;
        }
        if src == Src::Mac && !forever {
            self.delete(p, items, false);
        } else {
            p.notice.set(Notice::ConfirmDelete(src, items, forever));
        }
    }

    /// Deletes each item in turn, stopping at the first error. `forever`: with no backup, folders
    /// with everything in them (only on the computer). Otherwise on the computer each goes to the
    /// Trash, or, on a disk with none, is deleted as on a phone: files backed up first, folders
    /// only when empty.
    pub(super) fn delete(self, p: Pane, items: Vec<Entry>, forever: bool) {
        self.delete_on(p, p.src.get_untracked(), items, forever);
    }

    /// As `delete`, for items on `src` (a sidebar folder's side), reported in pane `p`. Panes
    /// inside what's deleted go to the folder it was in.
    pub(super) fn delete_on(self, p: Pane, src: Src, items: Vec<Entry>, forever: bool) {
        p.notice.set(Notice::None);
        let trash = crate::os::trash();
        spawn_local(async move {
            let (mut done, mut trashed, mut backups) = (0, 0, 0);
            let mut error = None;
            let mut gone = vec![];
            for e in &items {
                let args = json!({ "src": src.key(), "path": e.path });
                if forever {
                    match call::<()>("remove_forever", args).await {
                        Ok(()) => done += 1,
                        Err(err) => {
                            error = Some(err);
                            break;
                        }
                    }
                    continue;
                }
                let to_trash = src == Src::Mac;
                if to_trash {
                    match call::<bool>("move_to_trash", args.clone()).await {
                        Ok(true) => {
                            done += 1;
                            trashed += 1;
                            continue;
                        }
                        Ok(false) => {}
                        Err(err) => {
                            error = Some(err);
                            break;
                        }
                    }
                }
                match call::<Option<String>>("remove", args).await {
                    Ok(b) => {
                        done += 1;
                        backups += b.is_some() as usize;
                        gone.push((e.path.clone(), e.dir, b));
                    }
                    Err(err) => {
                        error = Some(if to_trash {
                            format!("{err} That disk has no {trash}.")
                        } else {
                            err
                        });
                        break;
                    }
                }
            }
            let one = items.len() == 1;
            let what = if one {
                items[0].name.clone()
            } else {
                format!("{done} items")
            };
            let place = self.place(src);
            let note = if backups > 0 {
                format!(" Backups are kept on {}.", crate::os::the_computer())
            } else {
                String::new()
            };
            // What went to the Trash comes back from there; what was backed up, with Undo.
            if forever && done > 0 {
                self.log(format!("Deleted {what} permanently from {place}"));
            } else if trashed > 0 {
                let t = if one {
                    what.clone()
                } else {
                    format!("{trashed} items")
                };
                self.log(format!("Moved {t} to the {trash} from {place}"));
            }
            let fell_back = gone.len();
            if !gone.is_empty() {
                let g = if one {
                    what.clone()
                } else {
                    format!("{fell_back} items")
                };
                self.log_undo(
                    format!("Deleted {g} from {place}"),
                    Undo::Delete { src, items: gone },
                );
            }
            let ok = if forever {
                format!("Deleted {what} permanently.")
            } else if src != Src::Mac {
                format!("Deleted {what}.{note}")
            } else if trashed == done {
                format!("Moved {what} to the {trash}.")
            } else if trashed == 0 {
                format!("Deleted {what}: that disk has no {trash}.{note}")
            } else {
                format!("Moved {trashed} to the {trash}; {fell_back} more had no {trash} on their disk and were deleted.{note}")
            };
            match error {
                None => p.notice.set(Notice::Ok(ok)),
                Some(err) if done == 0 => p.error(err),
                Some(err) => p.error(format!("Deleted {done} of {}, then: {err}", items.len())),
            }
            if let Some(e) = items.first() {
                self.changed(src, &parent(&e.path));
            }
            // A pane in what was deleted goes to the folder it was in.
            for q in self.panes {
                let cwd = q.cwd.get_untracked();
                if q.src.get_untracked() == src {
                    if let Some(e) = items[..done.min(items.len())]
                        .iter()
                        .find(|e| within(&cwd, &e.path))
                    {
                        self.navigate(q, parent(&e.path));
                    }
                }
            }
        });
    }

    pub(super) fn rename(self, p: Pane, e: Entry, name: String) {
        p.renaming.set(None);
        let name = name.trim().to_string();
        if name == e.name || name.is_empty() {
            return;
        }
        let src = p.src.get_untracked();
        spawn_local(async move {
            match call::<String>(
                "rename",
                json!({ "src": src.key(), "path": e.path, "name": name }),
            )
            .await
            {
                Ok(new) => {
                    self.log_undo(
                        format!("Renamed {} to {}", e.name, base(&new)),
                        Undo::Renames {
                            src,
                            items: vec![(new.clone(), e.name.clone())],
                        },
                    );
                    self.doc_moved(src, &e.path, &new);
                    p.select_only(Some(new));
                    self.changed(src, &parent(&e.path));
                }
                Err(err) => p.error(err),
            }
        });
    }

    pub(super) fn create(self, p: Pane, name: String, folder: bool) {
        let name = name.trim().to_string();
        if name.is_empty() || name.contains('/') {
            p.error("Enter a name without /.");
            return;
        }
        let src = p.src.get_untracked();
        let dir = p.cwd.get_untracked();
        let path = join(&dir, &name);
        spawn_local(async move {
            let result = if folder {
                call::<()>("mkdir", json!({ "src": src.key(), "path": path })).await
            } else {
                let args = json!({ "src": src.key(), "path": path, "text": "", "base_version": null, "create": true });
                call::<WriteResult>("write_text", args).await.map(|_| ())
            };
            match result {
                Ok(()) => {
                    p.notice.set(Notice::None);
                    self.log_undo(
                        format!("Created {}", path),
                        Undo::Create {
                            src,
                            path: path.clone(),
                            folder,
                        },
                    );
                    if folder {
                        self.changed(src, &dir)
                    } else if p.view.get_untracked() == ViewMode::Code {
                        self.changed(src, &dir);
                        self.open_doc(p, path)
                    } else {
                        self.open_text(p, path)
                    }
                }
                Err(e) => p.error(e),
            }
        });
    }
}
