// ---------------------------------------------------------------------------
// Search modes
// ---------------------------------------------------------------------------

use std::path::Path;
use std::time::Instant;

use ratatui::widgets::ListState;

use super::render::{humanize_count, humanize_size};
use crate::api::ipc_client::{IpcClient, IpcClientError};
use crate::api::ipc_protocol::{IpcRequest, IpcResponse, SearchHitDto};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    Exact,
    Fuzzy,
    Regex,
    Content,
    Archive,
}

impl Mode {
    pub(super) fn prefix(self) -> &'static str {
        match self {
            Mode::Exact   => "",
            Mode::Fuzzy   => "fuzzy:",
            Mode::Regex   => "regex:",
            Mode::Content => "content:",
            Mode::Archive => "archive:",
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Mode::Exact   => "exact",
            Mode::Fuzzy   => "fuzzy",
            Mode::Regex   => "regex",
            Mode::Content => "content",
            Mode::Archive => "archive",
        }
    }

    pub(super) fn next(self) -> Mode {
        match self {
            Mode::Exact   => Mode::Fuzzy,
            Mode::Fuzzy   => Mode::Regex,
            Mode::Regex   => Mode::Content,
            Mode::Content => Mode::Archive,
            Mode::Archive => Mode::Exact,
        }
    }
}

// ---------------------------------------------------------------------------
// Pre-computed row - built once when hits arrive, read many times per frame
// ---------------------------------------------------------------------------

pub(super) struct HitRow {
    pub(super) path:       String,
    // Pre-formatted "  1.2KB  rs" - avoids format! in every render frame.
    pub(super) meta:       String,
    pub(super) is_archive: bool,
}

// ---------------------------------------------------------------------------
// TUI state
// ---------------------------------------------------------------------------

pub(super) struct App {
    pub(super) input:          String,
    pub(super) mode:           Mode,
    // Rendered representation of each hit - separate from the raw DTO so
    // render only borrows strings without allocation.
    pub(super) rows:           Vec<HitRow>,
    pub(super) total_hits:     u64,
    pub(super) list_state:     ListState,
    pub(super) status_msg:     String,
    pub(super) last_key_at:    Option<Instant>,
    pub(super) search_pending: bool,
    pub(super) limit:          u32,
    // Cached title string; updated only when mode changes (not every frame).
    pub(super) mode_title:     String,
    // Last search latency (IPC round-trip, client-side).
    pub(super) last_took_ms:   Option<u64>,
    // Latest index stats for the status bar footer (3.2M idx  4k zip).
    pub(super) stats_total_files: u64,
    pub(super) stats_zip_entries: u64,
    // Reused IPC connection; reconnected lazily on transport errors.
    pub(super) ipc_path:       std::path::PathBuf,
    pub(super) ipc:            Option<IpcClient>,
}

impl App {
    pub(super) fn new(ipc_path: &Path) -> Self {
        Self {
            input:          String::new(),
            mode:           Mode::Exact,
            rows:           Vec::new(),
            total_hits:     0,
            list_state:     ListState::default(),
            status_msg:     String::new(),
            last_key_at:    None,
            search_pending: false,
            limit:          100,
            mode_title:     format!(" Search [{}] ", Mode::Exact.label()),
            last_took_ms:   None,
            stats_total_files: 0,
            stats_zip_entries: 0,
            ipc_path:       ipc_path.to_path_buf(),
            ipc:            None,
        }
    }

    pub(super) fn full_query(&self) -> String {
        let prefix = self.mode.prefix();
        if prefix.is_empty() {
            self.input.clone()
        } else if self.input.is_empty() {
            String::new()
        } else {
            format!("{prefix}{}", self.input)
        }
    }

    pub(super) fn cycle_mode(&mut self) {
        self.mode       = self.mode.next();
        // Update cache in one place so render never calls format!.
        self.mode_title = format!(" Search [{}] ", self.mode.label());
        self.search_pending = true;
        self.last_key_at    = Some(Instant::now());
    }

    pub(super) fn select_next(&mut self) {
        if self.rows.is_empty() { return; }
        let i = self.list_state.selected().map(|i| (i + 1).min(self.rows.len() - 1)).unwrap_or(0);
        self.list_state.select(Some(i));
    }

    pub(super) fn select_prev(&mut self) {
        if self.rows.is_empty() { return; }
        let i = self.list_state.selected().map(|i| i.saturating_sub(1)).unwrap_or(0);
        self.list_state.select(Some(i));
    }

    /// Returns the path of the currently selected row (borrows then copies) so
    /// the caller can drop the shared borrow before taking `&mut App` again.
    pub(super) fn selected_path(&self) -> Option<String> {
        self.list_state.selected()
            .and_then(|i| self.rows.get(i))
            .map(|r| r.path.clone())
    }

    /// Send a request over the persistent connection, reconnecting once on
    /// transport-layer failures (socket closed, broken pipe, etc.).
    pub(super) async fn send(&mut self, req: IpcRequest) -> Result<IpcResponse, IpcClientError> {
        let client = match self.ipc.as_mut() {
            Some(c) => c,
            None    => self.ipc.insert(IpcClient::connect(&self.ipc_path).await?),
        };
        match client.request(req).await {
            Err(e) if e.is_connection_error() => {
                // Drop the stale connection; the next call will reconnect.
                self.ipc = None;
                Err(e)
            }
            other => other,
        }
    }

    /// Load new hits, pre-computing per-row display strings.
    pub(super) fn load_hits(&mut self, hits: Vec<SearchHitDto>, total: u64, took_ms: u64) {
        self.rows.clear();
        self.rows.extend(hits.into_iter().map(|h| {
            let ext  = h.extension.as_deref().unwrap_or("");
            let meta = format!("  {}  {ext}", humanize_size(h.size_bytes));
            HitRow { path: h.path, meta, is_archive: h.archive_path.is_some() }
        }));
        self.total_hits   = total;
        self.last_took_ms = Some(took_ms);
        self.rebuild_status();
        if !self.rows.is_empty() && self.list_state.selected().is_none() {
            self.list_state.select(Some(0));
        }
        if self.rows.is_empty() {
            self.list_state = ListState::default();
        }
    }

    /// Update stats counters from a Stats IPC response.
    pub(super) fn load_stats(&mut self, total_files: u64, zip_entries: u64) {
        self.stats_total_files = total_files;
        self.stats_zip_entries = zip_entries;
        self.rebuild_status();
    }

    /// Rebuild the status bar string from current state.
    fn rebuild_status(&mut self) {
        let n       = self.rows.len();
        let total   = self.total_hits;
        let took_ms = self.last_took_ms.unwrap_or(0);
        let mode    = self.mode.label();
        let idx     = humanize_count(self.stats_total_files);
        let zip     = humanize_count(self.stats_zip_entries);
        self.status_msg = format!(
            "{n} results (total: {total})  •  {took_ms}ms  •  {mode}  •  {idx} idx  {zip} zip"
        );
    }

    pub(super) fn clear_hits(&mut self) {
        self.rows.clear();
        self.total_hits   = 0;
        self.last_took_ms = None;
        self.list_state   = ListState::default();
        self.status_msg.clear();
    }

    /// Reset stats counters (used in tests).
    #[cfg(test)]
    pub(super) fn reset_stats(&mut self) {
        self.stats_total_files = 0;
        self.stats_zip_entries = 0;
    }
}
