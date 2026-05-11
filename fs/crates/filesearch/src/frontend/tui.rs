//! Interactive ratatui TUI (`filesearch tui`).
//!
//! Layout:
//!   ┌──────────────────────────────────────────┐
//!   │ 🔍  [search input                      ] │
//!   ├──────────────────────────────────────────┤
//!   │ (scrollable results list)                │
//!   ├──────────────────────────────────────────┤
//!   │ N results  •  Xms  •  mode  •  M idx     │
//!   └──────────────────────────────────────────┘
//!
//! Keybindings:
//!   Type       — update query (debounced 100 ms → IPC search)
//!   ↑ / ↓     — navigate results
//!   Enter      — open selected file via /open API
//!   Tab        — cycle mode: exact → fuzzy: → regex: → content: → archive:
//!   Ctrl+C/Esc — exit
//!   F5         — force re-index

use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame, Terminal,
};

use crate::api::ipc_client::{IpcClient, IpcClientError};
use crate::api::ipc_protocol::{IpcRequest, IpcResponse, SearchHitDto};

// ---------------------------------------------------------------------------
// Search modes
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Exact,
    Fuzzy,
    Regex,
    Content,
    Archive,
}

impl Mode {
    fn prefix(self) -> &'static str {
        match self {
            Mode::Exact   => "",
            Mode::Fuzzy   => "fuzzy:",
            Mode::Regex   => "regex:",
            Mode::Content => "content:",
            Mode::Archive => "archive:",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Mode::Exact   => "exact",
            Mode::Fuzzy   => "fuzzy",
            Mode::Regex   => "regex",
            Mode::Content => "content",
            Mode::Archive => "archive",
        }
    }

    fn next(self) -> Mode {
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
// Pre-computed row — built once when hits arrive, read many times per frame
// ---------------------------------------------------------------------------

struct HitRow {
    path:       String,
    // Pre-formatted "  1.2KB  rs" — avoids format! in every render frame.
    meta:       String,
    is_archive: bool,
}

// ---------------------------------------------------------------------------
// TUI state
// ---------------------------------------------------------------------------

struct App {
    input:          String,
    mode:           Mode,
    // Rendered representation of each hit — separate from the raw DTO so
    // render only borrows strings without allocation.
    rows:           Vec<HitRow>,
    total_hits:     u64,
    list_state:     ListState,
    status_msg:     String,
    last_key_at:    Option<Instant>,
    search_pending: bool,
    limit:          u32,
    // Cached title string; updated only when mode changes (not every frame).
    mode_title:     String,
    // Last search latency (IPC round-trip, client-side).
    last_took_ms:   Option<u64>,
    // Latest index stats for the status bar footer (3.2M idx  4k zip).
    stats_total_files: u64,
    stats_zip_entries: u64,
    // Reused IPC connection; reconnected lazily on transport errors.
    ipc_path:       std::path::PathBuf,
    ipc:            Option<IpcClient>,
}

impl App {
    fn new(ipc_path: &Path) -> Self {
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

    fn full_query(&self) -> String {
        let prefix = self.mode.prefix();
        if prefix.is_empty() {
            self.input.clone()
        } else if self.input.is_empty() {
            String::new()
        } else {
            format!("{prefix}{}", self.input)
        }
    }

    fn cycle_mode(&mut self) {
        self.mode       = self.mode.next();
        // Update cache in one place so render never calls format!.
        self.mode_title = format!(" Search [{}] ", self.mode.label());
        self.search_pending = true;
        self.last_key_at    = Some(Instant::now());
    }

    fn select_next(&mut self) {
        if self.rows.is_empty() { return; }
        let i = self.list_state.selected().map(|i| (i + 1).min(self.rows.len() - 1)).unwrap_or(0);
        self.list_state.select(Some(i));
    }

    fn select_prev(&mut self) {
        if self.rows.is_empty() { return; }
        let i = self.list_state.selected().map(|i| i.saturating_sub(1)).unwrap_or(0);
        self.list_state.select(Some(i));
    }

    /// Returns the path of the currently selected row (borrows then copies) so
    /// the caller can drop the shared borrow before taking `&mut App` again.
    fn selected_path(&self) -> Option<String> {
        self.list_state.selected()
            .and_then(|i| self.rows.get(i))
            .map(|r| r.path.clone())
    }

    /// Send a request over the persistent connection, reconnecting once on
    /// transport-layer failures (socket closed, broken pipe, etc.).
    async fn send(&mut self, req: IpcRequest) -> Result<IpcResponse, IpcClientError> {
        // Lazily connect.
        if self.ipc.is_none() {
            self.ipc = Some(IpcClient::connect(&self.ipc_path).await?);
        }
        let client = self.ipc.as_mut().unwrap();
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
    fn load_hits(&mut self, hits: Vec<SearchHitDto>, total: u64, took_ms: u64) {
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
    fn load_stats(&mut self, total_files: u64, zip_entries: u64) {
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

    fn clear_hits(&mut self) {
        self.rows.clear();
        self.total_hits   = 0;
        self.last_took_ms = None;
        self.list_state   = ListState::default();
        self.status_msg.clear();
    }

    /// Reset stats counters (used in tests).
    #[cfg(test)]
    fn reset_stats(&mut self) {
        self.stats_total_files = 0;
        self.stats_zip_entries = 0;
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub async fn run(ipc_path: &Path) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend  = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;

    let result = event_loop(&mut term, ipc_path).await;

    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;

    result
}

// ---------------------------------------------------------------------------
// Event loop
// ---------------------------------------------------------------------------

const DEBOUNCE:     Duration = Duration::from_millis(100);
const POLL_TIMEOUT: Duration = Duration::from_millis(20);

async fn event_loop(
    term:     &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ipc_path: &Path,
) -> anyhow::Result<()> {
    let mut app = App::new(ipc_path);

    loop {
        term.draw(|f| render(f, &mut app))?;

        // Fire debounced search if needed.
        if let Some(last) = app.last_key_at {
            if app.search_pending && last.elapsed() >= DEBOUNCE {
                app.search_pending = false;
                app.last_key_at    = None;
                let q = app.full_query();
                if !q.is_empty() {
                    search(&mut app, q).await;
                } else {
                    app.clear_hits();
                }
            }
        }

        // Poll for terminal events (non-blocking with timeout).
        if !event::poll(POLL_TIMEOUT)? {
            continue;
        }

        if let Event::Key(key) = event::read()? {
            if handle_key(&mut app, key).await? {
                break; // exit requested
            }
        }
    }

    Ok(())
}

/// Returns `true` when the user requested exit.
async fn handle_key(app: &mut App, key: KeyEvent) -> anyhow::Result<bool> {
    match key.code {
        // Exit
        KeyCode::Esc => return Ok(true),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(true),

        // Navigate results
        KeyCode::Down => app.select_next(),
        KeyCode::Up   => app.select_prev(),

        // Open selected file
        KeyCode::Enter => {
            // Clone path string to release the shared borrow before calling send().
            if let Some(path) = app.selected_path() {
                open_file(app, path).await;
            }
        }

        // Cycle mode
        KeyCode::Tab => app.cycle_mode(),

        // Force re-index
        KeyCode::F(5) => {
            reindex(app).await;
            app.status_msg = "re-index requested".into();
        }

        // Text input
        KeyCode::Char(c) => {
            app.input.push(c);
            app.search_pending = true;
            app.last_key_at    = Some(Instant::now());
        }
        KeyCode::Backspace => {
            app.input.pop();
            app.search_pending = true;
            app.last_key_at    = Some(Instant::now());
        }

        _ => {}
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// IPC helpers — all go through app.send() for connection reuse
// ---------------------------------------------------------------------------

async fn search(app: &mut App, query: String) {
    let t0 = Instant::now();
    match app.send(IpcRequest::Search { query, limit: app.limit, offset: 0 }).await {
        Ok(IpcResponse::Search(sr)) => {
            let took_ms = t0.elapsed().as_millis() as u64;
            app.load_hits(sr.hits, sr.total_hits, took_ms);
            fetch_stats(app).await;
        }
        Ok(_) => {}
        Err(e) => { app.status_msg = format!("search error: {e}"); }
    }
}

async fn fetch_stats(app: &mut App) {
    if let Ok(IpcResponse::Stats(s)) = app.send(IpcRequest::Stats).await {
        app.load_stats(s.total_files, s.archive_entries_indexed);
    }
}

async fn open_file(app: &mut App, path: String) {
    match app.send(IpcRequest::Open { path }).await {
        Ok(IpcResponse::Open { opened: true,  .. }) => { app.status_msg = "file opened".into(); }
        Ok(IpcResponse::Open { opened: false, .. }) => { app.status_msg = "daemon could not open file".into(); }
        Ok(_)  => {}
        Err(e) => { app.status_msg = format!("open error: {e}"); }
    }
}

async fn reindex(app: &mut App) {
    let _ = app.send(IpcRequest::Reindex { full: true }).await;
}

// ---------------------------------------------------------------------------
// Rendering — zero heap allocations for stable frames
// ---------------------------------------------------------------------------

fn render(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // search box
            Constraint::Min(0),     // results
            Constraint::Length(1),  // status bar
        ])
        .split(f.size());

    // ── Search input ──────────────────────────────────────────────────────
    let search_block = Block::default()
        .borders(Borders::ALL)
        // Borrows the pre-computed String — no allocation.
        .title(app.mode_title.as_str());
    let input_para = Paragraph::new(app.input.as_str())
        .block(search_block)
        .style(Style::default().fg(Color::White));
    f.render_widget(input_para, chunks[0]);

    // Cursor at end of input.
    let cursor_x = chunks[0].x + 1 + app.input.len() as u16;
    let cursor_y = chunks[0].y + 1;
    f.set_cursor(
        cursor_x.min(chunks[0].x + chunks[0].width.saturating_sub(2)),
        cursor_y,
    );

    // ── Results list ─────────────────────────────────────────────────────
    // Borrows pre-computed strings — no clone, no format! per row per frame.
    let items: Vec<ListItem> = app.rows.iter().map(|row| {
        let zip_tag = if row.is_archive {
            Span::styled(" [zip]", Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM))
        } else {
            Span::raw("")
        };
        let line = Line::from(vec![
            Span::raw(row.path.as_str()),
            Span::styled(row.meta.as_str(), Style::default().fg(Color::DarkGray)),
            zip_tag,
        ]);
        ListItem::new(line)
    }).collect();

    let results_block = Block::default()
        .borders(Borders::ALL)
        .title(" Results ");
    let list = List::new(items)
        .block(results_block)
        .highlight_style(Style::default().bg(Color::Blue).add_modifier(Modifier::BOLD))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, chunks[1], &mut app.list_state);

    // ── Status bar ────────────────────────────────────────────────────────
    let status = Paragraph::new(app.status_msg.as_str())
        .style(Style::default().fg(Color::DarkGray));
    f.render_widget(status, chunks[2]);
}

fn humanize_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1}GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

fn humanize_count(n: u64) -> String {
    if n < 1_000 {
        format!("{n}")
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ipc_protocol::SearchHitDto;

    fn make_app() -> App {
        App::new(std::path::Path::new("/tmp/test.sock"))
    }

    fn make_hit(path: &str, archive_path: Option<&str>) -> SearchHitDto {
        SearchHitDto {
            path:         path.into(),
            name:         path.rsplit('/').next().unwrap_or(path).into(),
            extension:    Some("rs".into()),
            size_bytes:   1024,
            mtime:        0,
            is_dir:       false,
            archive_path: archive_path.map(str::to_string),
            snippet:      None,
        }
    }

    // ── Mode ─────────────────────────────────────────────────────────────────

    #[test]
    fn mode_prefix_all_variants() {
        assert_eq!(Mode::Exact.prefix(),   "");
        assert_eq!(Mode::Fuzzy.prefix(),   "fuzzy:");
        assert_eq!(Mode::Regex.prefix(),   "regex:");
        assert_eq!(Mode::Content.prefix(), "content:");
        assert_eq!(Mode::Archive.prefix(), "archive:");
    }

    #[test]
    fn mode_label_all_variants() {
        assert_eq!(Mode::Exact.label(),   "exact");
        assert_eq!(Mode::Fuzzy.label(),   "fuzzy");
        assert_eq!(Mode::Regex.label(),   "regex");
        assert_eq!(Mode::Content.label(), "content");
        assert_eq!(Mode::Archive.label(), "archive");
    }

    #[test]
    fn mode_next_cycles_through_all_and_wraps() {
        let cycle = [Mode::Exact, Mode::Fuzzy, Mode::Regex, Mode::Content, Mode::Archive, Mode::Exact];
        for pair in cycle.windows(2) {
            assert_eq!(pair[0].next(), pair[1]);
        }
    }

    // ── App::full_query ───────────────────────────────────────────────────────

    #[test]
    fn full_query_exact_mode_returns_raw_input() {
        let mut app = make_app();
        app.input = "main.rs".into();
        assert_eq!(app.full_query(), "main.rs");
    }

    #[test]
    fn full_query_fuzzy_mode_prepends_prefix() {
        let mut app = make_app();
        app.mode  = Mode::Fuzzy;
        app.input = "mian".into();
        assert_eq!(app.full_query(), "fuzzy:mian");
    }

    #[test]
    fn full_query_non_exact_mode_with_empty_input_returns_empty() {
        let mut app = make_app();
        app.mode  = Mode::Content;
        app.input = String::new();
        assert_eq!(app.full_query(), "");
    }

    // ── Navigation ───────────────────────────────────────────────────────────

    #[test]
    fn select_next_clamps_at_last_row() {
        let mut app = make_app();
        app.load_hits(vec![make_hit("/a", None), make_hit("/b", None)], 2, 5);
        app.select_next();
        app.select_next();
        app.select_next(); // past end
        assert_eq!(app.list_state.selected(), Some(1)); // clamped at index 1
    }

    #[test]
    fn select_next_empty_rows_is_noop() {
        let mut app = make_app();
        app.select_next();
        assert_eq!(app.list_state.selected(), None);
    }

    #[test]
    fn select_prev_clamps_at_zero() {
        let mut app = make_app();
        app.load_hits(vec![make_hit("/a", None), make_hit("/b", None)], 2, 5);
        app.select_prev(); // already at 0 (auto-selected on load)
        app.select_prev();
        assert_eq!(app.list_state.selected(), Some(0));
    }

    #[test]
    fn selected_path_none_when_nothing_selected() {
        let app = make_app();
        assert_eq!(app.selected_path(), None);
    }

    #[test]
    fn selected_path_returns_path_of_selected_row() {
        let mut app = make_app();
        app.load_hits(vec![make_hit("/first.rs", None), make_hit("/second.rs", None)], 2, 5);
        app.list_state.select(Some(1));
        assert_eq!(app.selected_path(), Some("/second.rs".into()));
    }

    // ── load_hits / clear_hits ───────────────────────────────────────────────

    #[test]
    fn load_hits_populates_rows_and_auto_selects() {
        let mut app = make_app();
        app.load_hits(vec![make_hit("/a.rs", None), make_hit("/b.rs", None)], 42, 7);
        assert_eq!(app.rows.len(), 2);
        assert_eq!(app.total_hits, 42);
        assert_eq!(app.last_took_ms, Some(7));
        assert_eq!(app.list_state.selected(), Some(0)); // auto-selected
        assert!(app.status_msg.contains("2 results"));
        assert!(app.status_msg.contains("7ms"));
        assert!(app.status_msg.contains("exact"));
    }

    #[test]
    fn load_hits_empty_clears_selection() {
        let mut app = make_app();
        app.load_hits(vec![make_hit("/a.rs", None)], 1, 5);
        app.load_hits(vec![], 0, 3);
        assert!(app.rows.is_empty());
        assert_eq!(app.list_state.selected(), None);
    }

    #[test]
    fn load_hits_marks_archive_entries() {
        let mut app = make_app();
        app.load_hits(vec![
            make_hit("/archive.zip!/main.rs", Some("/archive.zip")),
            make_hit("/real.rs", None),
        ], 2, 1);
        assert!(app.rows[0].is_archive);
        assert!(!app.rows[1].is_archive);
    }

    #[test]
    fn clear_hits_resets_all_state() {
        let mut app = make_app();
        app.load_hits(vec![make_hit("/a.rs", None)], 1, 5);
        app.clear_hits();
        assert!(app.rows.is_empty());
        assert_eq!(app.total_hits, 0);
        assert_eq!(app.last_took_ms, None);
        assert_eq!(app.list_state.selected(), None);
        assert!(app.status_msg.is_empty());
    }

    // ── humanize_size ─────────────────────────────────────────────────────────

    #[test]
    fn humanize_size_boundaries() {
        assert_eq!(humanize_size(0),           "0B");
        assert_eq!(humanize_size(1023),        "1023B");
        assert_eq!(humanize_size(1024),        "1.0KB");
        assert_eq!(humanize_size(1024 * 1024 - 1), "1024.0KB");
        assert_eq!(humanize_size(1024 * 1024), "1.0MB");
        assert_eq!(humanize_size(1024 * 1024 * 1024), "1.0GB");
    }

    // ── cycle_mode ────────────────────────────────────────────────────────────

    #[test]
    fn cycle_mode_updates_title_and_sets_pending() {
        let mut app = make_app();
        assert_eq!(app.mode, Mode::Exact);
        app.cycle_mode();
        assert_eq!(app.mode, Mode::Fuzzy);
        assert!(app.mode_title.contains("fuzzy"));
        assert!(app.search_pending);
        assert!(app.last_key_at.is_some());
    }

    // ── humanize_count ────────────────────────────────────────────────────────

    #[test]
    fn humanize_count_below_thousand() {
        assert_eq!(humanize_count(0),   "0");
        assert_eq!(humanize_count(1),   "1");
        assert_eq!(humanize_count(999), "999");
    }

    #[test]
    fn humanize_count_thousands() {
        assert_eq!(humanize_count(1_000),   "1.0k");
        assert_eq!(humanize_count(1_500),   "1.5k");
        assert_eq!(humanize_count(999_999), "1000.0k");
    }

    #[test]
    fn humanize_count_millions() {
        assert_eq!(humanize_count(1_000_000), "1.0M");
        assert_eq!(humanize_count(3_200_000), "3.2M");
    }

    // ── load_stats / rebuild_status ───────────────────────────────────────────

    #[test]
    fn load_stats_updates_counters_and_rebuilds_status() {
        let mut app = make_app();
        app.load_hits(vec![make_hit("/a.rs", None)], 1, 10);
        app.load_stats(3_200_000, 4_000);
        assert_eq!(app.stats_total_files, 3_200_000);
        assert_eq!(app.stats_zip_entries, 4_000);
        assert!(app.status_msg.contains("3.2M idx"));
        assert!(app.status_msg.contains("4.0k zip"));
    }

    #[test]
    fn rebuild_status_shows_result_count_took_and_mode() {
        let mut app = make_app();
        app.load_hits(vec![make_hit("/a.rs", None), make_hit("/b.rs", None)], 99, 42);
        assert!(app.status_msg.contains("2 results"));
        assert!(app.status_msg.contains("total: 99"));
        assert!(app.status_msg.contains("42ms"));
        assert!(app.status_msg.contains("exact"));
    }

    #[test]
    fn stats_persist_across_clear_hits() {
        let mut app = make_app();
        app.load_stats(5_000, 200);
        app.load_hits(vec![make_hit("/a.rs", None)], 1, 5);
        app.clear_hits();
        // Stats counters survive clear_hits (they're daemon-side facts)
        assert_eq!(app.stats_total_files, 5_000);
        assert_eq!(app.stats_zip_entries, 200);
    }

    #[test]
    fn reset_stats_zeroes_counters() {
        let mut app = make_app();
        app.load_stats(1_000, 50);
        app.reset_stats();
        assert_eq!(app.stats_total_files, 0);
        assert_eq!(app.stats_zip_entries, 0);
    }
}
