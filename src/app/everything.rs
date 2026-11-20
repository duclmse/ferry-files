//! Everything (voidtools) servers: read-only drives on other computers, added in Settings >
//! Connections (see `servers.rs`). They're connected like phones, so tabs, favourites,
//! workspaces, search and copying out work on them.

use super::*;

/// How often the connections are looked at: offline servers connected, drives looked for.
pub(super) const EVERYTHING_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

impl Ctx {
    /// Whether a phone (not a server) is connected.
    pub(super) fn has_phone(self) -> bool {
        self.phones.with_untracked(|ps| ps.iter().any(Info::is_phone))
    }

    /// Whether `src`'s files can only be read: an Everything server.
    pub(super) fn read_only(self, src: Src) -> bool {
        self.src_info(src).is_some_and(|i| i.transport == "everything")
    }

    /// Shows all of `src`'s drives in the active pane with the search box open, to search them all.
    pub(super) fn search_all(self, src: Src) {
        let p = self.act();
        self.open_place(src, "/".into());
        self.open_search(p, false);
    }
}
