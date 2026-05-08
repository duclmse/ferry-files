//! Interactive ratatui TUI (`filesearch tui`).
//!
//! Layout:
//!   ┌──────────────────────────────────────────┐
//!   │ 🔍  [search input                      ]  │
//!   ├──────────────────────────────────────────┤
//!   │ (scrollable results list)                 │
//!   ├──────────────────────────────────────────┤
//!   │ N results  •  Xms  •  mode  •  M idx      │
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

#[derive(Clone, Copy, PartialEq, Eq)]
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
    fn load_hits(&mut self, hits: Vec<SearchHitDto>, total: u64) {
        self.rows.clear();
        self.rows.extend(hits.into_iter().map(|h| {
            let ext  = h.extension.as_deref().unwrap_or("");
            let meta = format!("  {}  {ext}", humanize_size(h.size_bytes));
            HitRow { path: h.path, meta, is_archive: h.archive_path.is_some() }
        }));
        self.total_hits = total;
        let n = self.rows.len();
        self.status_msg = format!("{n} result(s) (total: {total})");
        if !self.rows.is_empty() && self.list_state.selected().is_none() {
            self.list_state.select(Some(0));
        }
        if self.rows.is_empty() {
            self.list_state = ListState::default();
        }
    }

    fn clear_hits(&mut self) {
        self.rows.clear();
        self.total_hits = 0;
        self.list_state = ListState::default();
        self.status_msg.clear();
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
    match app.send(IpcRequest::Search { query, limit: app.limit, offset: 0 }).await {
        Ok(IpcResponse::Search(sr)) => app.load_hits(sr.hits, sr.total_hits),
        Ok(_) => {}
        Err(e) => { app.status_msg = format!("search error: {e}"); }
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
