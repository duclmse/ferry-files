//! Copying and moving between the panes, and pasting.

use super::*;

impl Ctx {
    /// "Copy to the Mac", "Move 3 items to <folder>" (when both panes are on the same side).
    pub(in crate::app) fn to_label(self, p: Pane, verb: &str, n: usize) -> String {
        let o = self.other(p);
        let (from, to) = (p.src.get_untracked(), o.src.get_untracked());
        let what = if n > 1 {
            format!(" {n} items")
        } else {
            String::new()
        };
        if from != to {
            format!(
                "{verb}{what} to {}",
                self.place(to).trim_start_matches("the ")
            )
        } else {
            format!("{verb}{what} to “{}”", base(&o.cwd.get_untracked()))
        }
    }

    /// Keeps the picked items for Paste, which copies them (moves them, when `moving`). Items on
    /// the computer go on the system clipboard too, for Finder.
    pub(in crate::app) fn copy_items(self, p: Pane, moving: bool) {
        let items: Vec<String> = p.picked().into_iter().map(|e| e.path).collect();
        if items.is_empty() {
            return;
        }
        if moving && !self.renamable(p) {
            return p.error("These can't be moved from here.");
        }
        let what = match items.as_slice() {
            [one] => format!("“{}”", base(one)),
            _ => format!("{} items", items.len()),
        };
        let src = p.src.get_untracked();
        // Finder gets the files; for a phone's, the clipboard gets their names instead, so what
        // Finder copied before isn't pasted in their place.
        let mac: Vec<String> = if src == Src::Mac {
            items.clone()
        } else {
            vec![]
        };
        let names = items.iter().map(|x| base(x)).collect::<Vec<_>>().join("\n");
        self.wrote.set_value(mac.clone());
        self.finder.set(vec![]);
        spawn_local(async move {
            let _ = call::<()>("clip_set", json!({ "paths": mac, "text": names })).await;
        });
        self.clip.set(Some(Clip {
            side: self.side(src),
            items,
            moving,
        }));
        let key = match self.key_label("paste") {
            k if k.is_empty() => String::new(),
            k => format!(" ({k})"),
        };
        let verb = if moving { "Cut" } else { "Copied" };
        let how = if moving { "move it" } else { "paste it" };
        p.notice.set(Notice::Ok(format!(
            "{verb} {what}: paste to {how} into another folder{key}."
        )));
    }

    /// Looks at the system clipboard for files copied in Finder since the app's last Copy, which
    /// Paste then pastes instead.
    pub(in crate::app) fn check_clipboard(self) {
        spawn_local(async move {
            let files = call::<Vec<String>>("clip_files", json!({}))
                .await
                .unwrap_or_default();
            let theirs = if files == self.wrote.get_value() {
                vec![]
            } else {
                files
            };
            if self.finder.get_untracked() != theirs {
                self.finder.set(theirs);
            }
        });
    }

    /// Whether Paste has something to paste.
    pub(in crate::app) fn can_paste(self) -> bool {
        !self.finder.with_untracked(Vec::is_empty) || self.clip.with_untracked(Option::is_some)
    }

    /// Paste, with what it pastes: "Paste “name”", "Paste 3 items", or "Move …" after Cut.
    pub(in crate::app) fn paste_label(self) -> String {
        let finder = self.finder.get_untracked();
        let (items, moving) = match self.clip.get_untracked() {
            _ if !finder.is_empty() => (finder, false),
            Some(c) => (c.items, c.moving),
            None => (vec![], false),
        };
        let verb = if moving { "Move" } else { "Paste" };
        match items.as_slice() {
            [one] => format!("{verb} “{}”", base(one)),
            [] => verb.into(),
            _ => format!("{verb} {} items", items.len()),
        }
    }

    /// Copies (or, after Cut, moves) what was copied into `dir` (None: the pane's folder): files
    /// copied in Finder since the app's last Copy, else what Copy kept.
    pub(in crate::app) fn paste(self, p: Pane, dir: Option<String>) {
        let dir = dir.unwrap_or_else(|| p.cwd.get_untracked());
        self.paste_to(p, p.src.get_untracked(), dir);
    }

    /// Paste into folder `to_dir` on `to_src` (a sidebar folder), with the work shown in pane `p`.
    pub(in crate::app) fn paste_to(self, p: Pane, to_src: Src, to_dir: String) {
        spawn_local(async move {
            let files = call::<Vec<String>>("clip_files", json!({}))
                .await
                .unwrap_or_default();
            let finder = if files == self.wrote.get_value() {
                vec![]
            } else {
                files
            };
            self.finder.set(finder.clone());
            let clip = match self.clip.get_untracked() {
                _ if !finder.is_empty() => Clip {
                    side: Side::Mac,
                    items: finder,
                    moving: false,
                },
                Some(c) => c,
                None => {
                    return p.error("Nothing to paste: pick files or folders and Copy them first.")
                }
            };
            let from = match self.src_of(&clip.side) {
                Ok(s) => s,
                Err(e) => return p.error(e),
            };
            let items = clip.items;
            if from == to_src {
                if items.iter().any(|x| {
                    *x == to_dir || to_dir.starts_with(&format!("{}/", x.trim_end_matches('/')))
                }) {
                    return p.error("A folder can't be pasted into itself.");
                }
                if items.iter().all(|x| parent(x) == to_dir) {
                    return p.error(
                        "That's already in this folder. Open another folder to paste it into.",
                    );
                }
            }
            // What was cut is moved once; after that it isn't where it was.
            if clip.moving {
                self.clip.set(None);
            }
            self.transfer(p, from, items, to_src, to_dir, clip.moving);
        });
    }
}
