//! Everything (voidtools) servers: read-only drives on other computers, added in the sidebar.
//! They're connected like phones (so tabs, favourites, workspaces, search and copying out work
//! on them), again at launch, and looked at every so often for drives that came or went.

use super::*;

/// How often the Everything servers are looked at: offline ones connected, drives looked for.
pub(super) const EVERYTHING_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// "http://host:port" from what was typed, as the backend makes it.
pub(super) fn everything_base(server: &str) -> String {
    let s = server.trim().trim_end_matches('/');
    if s.starts_with("http://") || s.starts_with("https://") { s.to_string() } else { format!("http://{s}") }
}

/// The Everything server a connection is to, by address.
fn base_of(info: &Info) -> Option<&str> {
    info.usb.as_deref()?.strip_prefix("everything:")
}

impl Ctx {
    /// The connection to the Everything server at `base`, when there is one.
    pub(super) fn everything_src(self, base: &str) -> Option<Src> {
        self.phones.with(|ps| ps.iter().find(|i| base_of(i) == Some(base)).map(|i| Src::of(&i.id)))
    }

    /// Whether a phone (not an Everything server) is connected.
    pub(super) fn has_phone(self) -> bool {
        self.phones.with_untracked(|ps| ps.iter().any(|i| i.transport != "everything"))
    }

    /// Whether `src`'s files can only be read: an Everything server.
    pub(super) fn read_only(self, src: Src) -> bool {
        self.src_info(src).is_some_and(|i| i.transport == "everything")
    }

    /// Adds the Everything server at `server` and shows its first drive. Why not, when it can't.
    pub(super) async fn add_everything(self, server: String) -> Result<(), String> {
        let info = call::<Info>("everything_connect", json!({ "server": server })).await?;
        let base = base_of(&info).map(String::from).unwrap_or_else(|| everything_base(&server));
        if !self.everything.with_untracked(|s| s.contains(&base)) {
            self.set_everything(self.everything.get_untracked().into_iter().chain([base.clone()]).collect());
        }
        self.everything_errors.update(|e| {
            e.remove(&base);
        });
        self.connected(info, true);
        Ok(())
    }

    /// Shows all of `src`'s drives in the active pane with the search box open, to search them all.
    pub(super) fn search_all(self, src: Src) {
        let p = self.act();
        self.open_place(src, "/".into());
        self.open_search(p, false);
    }

    /// Takes the Everything server at `base` out of the sidebar, disconnecting it.
    pub(super) fn remove_everything(self, base: &str) {
        self.set_everything(self.everything.get_untracked().into_iter().filter(|s| s != base).collect());
        self.everything_errors.update(|e| {
            e.remove(base);
        });
        if let Some(src) = self.everything_src(base) {
            self.disconnect(src);
        }
    }

    /// Connects the Everything servers that aren't, and looks again at the drives of those that
    /// are. At launch and every `EVERYTHING_EVERY`.
    pub(super) fn check_everything(self) {
        for base in self.everything.get_untracked() {
            match self.everything_src(&base) {
                Some(src) => spawn_local(async move {
                    if call::<()>("refresh", json!({ "src": src.key() })).await.is_err() {
                        return;
                    }
                    if let Ok(Some(info)) = call::<Option<Info>>("info", json!({ "src": src.key() })).await {
                        // Only when the drives changed, so the sidebar isn't drawn again for nothing.
                        let changed = self.phones.with_untracked(|ps| ps.iter().any(|i| i.id == info.id && i.quick_dirs != info.quick_dirs));
                        if changed {
                            self.phones.update(|ps| {
                                if let Some(i) = ps.iter_mut().find(|i| i.id == info.id) {
                                    i.quick_dirs = info.quick_dirs;
                                }
                            });
                        }
                    }
                }),
                None => spawn_local(async move {
                    match call::<Info>("everything_connect", json!({ "server": base })).await {
                        Ok(info) => {
                            // Taken out of the sidebar while connecting.
                            if !self.everything.with_untracked(|s| s.contains(&base)) {
                                let _ = call::<()>("disconnect", json!({ "src": info.id })).await;
                                return;
                            }
                            self.everything_errors.update(|e| {
                                e.remove(&base);
                            });
                            self.connected(info, false);
                        }
                        // Connected meanwhile (a check already under way).
                        Err(e) if e.contains("already added") => {}
                        Err(e) => self.everything_errors.update(|m| {
                            m.insert(base, e);
                        }),
                    }
                }),
            }
        }
    }
}
