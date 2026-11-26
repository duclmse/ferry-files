//! The app's shared state: how it starts, its settings, and what the menus act on.

use super::*;

impl Ctx {
    pub(super) fn new() -> Self {
        Ctx {
            phones: RwSignal::new(vec![]),
            mac: RwSignal::new(None),
            panes: [Pane::new(0), Pane::new(1)],
            active: RwSignal::new(0),
            split: RwSignal::new(stored(SPLIT_KEY).as_deref() == Some("1")),
            side: RwSignal::new(stored_flag(SIDE_KEY, true)),
            editor: RwSignal::new(None),
            text: RwSignal::new(String::new()),
            status: RwSignal::new((String::new(), "")),
            confirm_discard: RwSignal::new(false),
            finding: RwSignal::new(false),
            find_state: FindState {
                query: RwSignal::new(String::new()),
                with: RwSignal::new(String::new()),
                opts: RwSignal::new(find::FindOpts::default()),
                keep: RwSignal::new(false),
                replacing: RwSignal::new(false),
            },
            busy: RwSignal::new(false),
            ep: crate::epub::Ep::new(),
            menu: RwSignal::new(None),
            settings: RwSignal::new(false),
            devices: RwSignal::new(None),
            scanning: StoredValue::new(false),
            scan_again: StoredValue::new(false),
            looking: RwSignal::new(false),
            failed: StoredValue::new(None),
            connecting: RwSignal::new(None),
            awaiting: RwSignal::new(None),
            access_timer: StoredValue::new_local(None),
            conn_error: RwSignal::new(None),
            show_hidden: RwSignal::new(stored_flag(HIDDEN_KEY, false)),
            colour_code: RwSignal::new(stored_flag(COLOUR_KEY, true)),
            wrap_text: RwSignal::new(stored_flag(WRAP_KEY, true)),
            line_numbers: RwSignal::new(stored_flag(LINE_NUMBERS_KEY, true)),
            phone_thumbs: RwSignal::new(stored_flag(PHONE_THUMBS_KEY, true)),
            follow_folder: RwSignal::new(stored_flag(FOLLOW_FOLDER_KEY, false)),
            scripts: RwSignal::new(vec![]),
            script_places: RwSignal::new(
                stored(scripts::PLACES_KEY)
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_default(),
            ),
            script_folder: RwSignal::new(stored(scripts::FOLDER_KEY).unwrap_or_default()),
            script_error: RwSignal::new(None),
            script_keys: RwSignal::new(
                stored(scripts::KEYS_KEY)
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_default(),
            ),
            script_capturing: RwSignal::new(None),
            script_key_note: RwSignal::new(None),
            section_order: RwSignal::new(sidebar::section_order(
                &stored(SECTION_ORDER_KEY)
                    .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
                    .unwrap_or_default(),
            )),
            // Only sections there still are (Everything is now part of Servers).
            hidden_sections: RwSignal::new(
                stored(HIDDEN_SECTIONS_KEY)
                    .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|k| sidebar::SECTIONS.iter().any(|s| s.0 == k))
                    .collect(),
            ),
            conns: RwSignal::new(servers::stored_conns()),
            conn_errors: RwSignal::new(HashMap::new()),
            smb_mounts: RwSignal::new(vec![]),
            auto_connect: RwSignal::new(stored_flag(AUTO_CONNECT_KEY, true)),
            verify: RwSignal::new(stored_flag(VERIFY_KEY, false)),
            comparing: RwSignal::new(false),
            cmp_content: RwSignal::new(false),
            dupe_progress: RwSignal::new(None),
            tried: StoredValue::new(HashSet::new()),
            seen: StoredValue::new((HashSet::new(), 0)),
            jobs: RwSignal::new(vec![]),
            next_job: StoredValue::new(1),
            space: RwSignal::new(HashMap::new()),
            history: RwSignal::new(vec![]),
            panel: RwSignal::new(None),
            favs: RwSignal::new(
                stored(FAVS_KEY)
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_default(),
            ),
            side_renaming: RwSignal::new(None),
            side_changed: RwSignal::new((0, Src::Mac, String::new())),
            sync_pairs: RwSignal::new(
                stored(SYNC_PAIRS_KEY)
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_default(),
            ),
            workspaces: RwSignal::new(saved_workspaces()),
            drag: StoredValue::new(None),
            tab_drag: StoredValue::new(None),
            tab_drop: RwSignal::new(None),
            clip: RwSignal::new(None),
            wrote: StoredValue::new(vec![]),
            finder: RwSignal::new(vec![]),
            drop_at: RwSignal::new(None),
            side_w: stored_px(SIDE_W_KEY),
            pane_w: stored_px(PANE_W_KEY),
            theme: RwSignal::new(stored(THEME_KEY).unwrap_or_else(|| "auto".into())),
            epub_read: RwSignal::new(stored_flag(preview::EPUB_READ_KEY, false)),
            read_find: RwSignal::new((0, 0)),
            keymap: RwSignal::new(saved_keys()),
            capturing: RwSignal::new(None),
            settings_tab: RwSignal::new("general"),
            wifi: RwSignal::new(false),
        }
    }

    /// Settings, at the keyboard shortcuts.
    pub(super) fn show_shortcuts(self) {
        self.settings_tab.set("keys");
        self.settings.set(true);
    }

    /// Drops hidden files from a listing unless the settings say to show them.
    pub(super) fn visible(self, entries: Vec<Entry>) -> Vec<Entry> {
        if self.show_hidden.get_untracked() {
            entries
        } else {
            entries.into_iter().filter(|e| !e.is_hidden()).collect()
        }
    }

    pub(super) fn set_show_hidden(self, on: bool) {
        self.show_hidden.set(on);
        store(HIDDEN_KEY, if on { "1" } else { "0" });
        self.reload_all();
    }

    pub(super) fn set_colour_code(self, on: bool) {
        self.colour_code.set(on);
        store(COLOUR_KEY, if on { "1" } else { "0" });
    }

    pub(super) fn set_wrap_text(self, on: bool) {
        self.wrap_text.set(on);
        store(WRAP_KEY, if on { "1" } else { "0" });
    }

    pub(super) fn set_line_numbers(self, on: bool) {
        self.line_numbers.set(on);
        store(LINE_NUMBERS_KEY, if on { "1" } else { "0" });
    }

    pub(super) fn set_hidden_sections(self, keys: Vec<String>) {
        store(
            HIDDEN_SECTIONS_KEY,
            &serde_json::to_string(&keys).unwrap_or_default(),
        );
        self.hidden_sections.set(keys);
    }

    pub(super) fn set_section_order(self, order: Vec<&'static str>) {
        store(
            SECTION_ORDER_KEY,
            &serde_json::to_string(&order).unwrap_or_default(),
        );
        self.section_order.set(order);
    }

    pub(super) fn set_follow_folder(self, on: bool) {
        self.follow_folder.set(on);
        store(FOLLOW_FOLDER_KEY, if on { "1" } else { "0" });
    }

    pub(super) fn set_phone_thumbs(self, on: bool) {
        self.phone_thumbs.set(on);
        store(PHONE_THUMBS_KEY, if on { "1" } else { "0" });
    }

    pub(super) fn set_verify(self, on: bool) {
        self.verify.set(on);
        store(VERIFY_KEY, if on { "1" } else { "0" });
    }

    pub(super) fn set_auto_connect(self, on: bool) {
        self.auto_connect.set(on);
        store(AUTO_CONNECT_KEY, if on { "1" } else { "0" });
        if on {
            self.scan();
        }
    }

    pub(super) fn act(self) -> Pane {
        self.panes[self.active.get_untracked()]
    }

    pub(super) fn other(self, p: Pane) -> Pane {
        self.panes[1 - p.id]
    }

    /// The panes on screen.
    pub(super) fn shown(self) -> Vec<Pane> {
        if self.split.get_untracked() {
            self.panes.to_vec()
        } else {
            vec![self.panes[0]]
        }
    }

    pub(super) fn src_info(self, src: Src) -> Option<Info> {
        match src {
            Src::Mac => self.mac.get(),
            Src::Dev(_) => {
                let id = src.key();
                self.phones
                    .with(|ps| ps.iter().find(|i| i.id == id).cloned())
            }
        }
    }

    /// For sentences: "Saved to the Mac." or "Saved to Pixel 7."
    pub(super) fn place(self, src: Src) -> String {
        match src {
            Src::Mac => crate::os::the_computer().into(),
            Src::Dev(_) => self
                .src_info(src)
                .map_or_else(|| "the device".into(), |i| i.label),
        }
    }

    pub(super) fn dirty(&self) -> bool {
        self.full_slot().dirty()
    }

    /// Where the full-window editor keeps its file.
    pub(super) fn full_slot(self) -> Slot {
        Slot {
            editor: self.editor,
            text: self.text,
            ask: self.confirm_discard,
            status: self.status,
            find: self.finding,
            pane: None,
        }
    }
}
