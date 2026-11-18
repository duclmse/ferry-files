//! Working on files: opening, download and upload, delete, rename, new file or folder.

use super::*;

impl Ctx {
    pub(super) fn open(self, p: Pane, e: Entry) {
        let src = p.src.get_untracked();
        match kind(&e) {
            Kind::Folder => self.navigate(p, e.path),
            Kind::Epub => {
                p.loading.set(true);
                let ep = self.ep;
                spawn_local(async move {
                    if let Err(err) = ep.open(&src.key(), self.place(src), e.path).await {
                        p.error(err);
                    }
                    p.loading.set(false);
                });
            }
            Kind::Text => self.open_text(p, e.path),
            Kind::Other => self.open_in_app(p, e.path),
        }
    }

    /// Opens a file in its default Mac app. A phone file is copied to the Mac first; that copy is
    /// what the app sees, so changes to it don't go back to the phone.
    pub(super) fn open_in_app(self, p: Pane, path: String) {
        let src = p.src.get_untracked();
        p.loading.set(true);
        spawn_local(async move {
            match call::<String>("open_file", json!({ "src": src.key(), "path": path })).await {
                Ok(_) if src != Src::Mac => {
                    p.notice.set(Notice::Ok(format!("Opened a copy of {}. Changes to it stay on {}.", base(&path), crate::os::the_computer())))
                }
                Ok(_) => {}
                Err(e) => p.error(e),
            }
            p.loading.set(false);
        });
    }

    pub(super) fn download(self, p: Pane, path: String) {
        let src = p.src.get_untracked();
        spawn_local(async move {
            match call::<Option<String>>("download", json!({ "src": src.key(), "path": path })).await {
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

    pub(super) fn ask_delete(self, p: Pane) {
        let items = p.picked();
        if !items.is_empty() {
            p.notice.set(Notice::ConfirmDelete(items));
        }
    }

    /// Deletes each item in turn (folders only when empty), stopping at the first error.
    pub(super) fn delete(self, p: Pane, items: Vec<Entry>) {
        let src = p.src.get_untracked();
        p.notice.set(Notice::None);
        spawn_local(async move {
            let mut done = 0;
            let mut backups = 0;
            let mut error = None;
            for e in &items {
                match call::<Option<String>>("remove", json!({ "src": src.key(), "path": e.path })).await {
                    Ok(b) => {
                        done += 1;
                        backups += b.is_some() as usize;
                    }
                    Err(err) => {
                        error = Some(err);
                        break;
                    }
                }
            }
            let what = if items.len() == 1 { items[0].name.clone() } else { format!("{done} items") };
            let note = if backups > 0 { format!(" Backups are kept on {}.", crate::os::the_computer()) } else { String::new() };
            if done > 0 {
                self.log(format!("Deleted {what} from {}", self.place(src)));
            }
            match error {
                None => p.notice.set(Notice::Ok(format!("Deleted {what}.{note}"))),
                Some(err) if done == 0 => p.error(err),
                Some(err) => p.error(format!("Deleted {done} of {}, then: {err}", items.len())),
            }
            if let Some(e) = items.first() {
                self.changed(src, &parent(&e.path));
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
            match call::<String>("rename", json!({ "src": src.key(), "path": e.path, "name": name })).await {
                Ok(new) => {
                    self.log(format!("Renamed {} to {}", e.name, base(&new)));
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
                    self.log(format!("Created {}", path));
                    if folder { self.changed(src, &dir) } else { self.open_text(p, path) }
                }
                Err(e) => p.error(e),
            }
        });
    }
}
