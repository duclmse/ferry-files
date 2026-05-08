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
// TUI state
// ---------------------------------------------------------------------------

struct App {
    input:        String,
    mode:         Mode,
    hits:         Vec<SearchHitDto>,
    total_hits:   u64,
    took_ms:      Option<u64>,
    list_state:   ListState,
    status_msg:   String,
    last_key_at:  Option<Instant>,
    search_pending: bool,
    limit:        u32,
}

impl App {
    fn new() -> Self {
        Self {
            input:           String::new(),
            mode:            Mode::Exact,
            hits:            Vec::new(),
            total_hits:      0,
            took_ms:         None,
            list_state:      ListState::default(),
            status_msg:      String::new(),
            last_key_at:     None,
            search_pending:  false,
            limit:           100,
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

    fn select_next(&mut self) {
        if self.hits.is_empty() { return; }
        let i = self.list_state.selected().map(|i| (i + 1).min(self.hits.len() - 1)).unwrap_or(0);
        self.list_state.select(Some(i));
    }

    fn select_prev(&mut self) {
        if self.hits.is_empty() { return; }
        let i = self.list_state.selected().map(|i| i.saturating_sub(1)).unwrap_or(0);
        self.list_state.select(Some(i));
    }

    fn selected_hit(&self) -> Option<&SearchHitDto> {
        self.list_state.selected().and_then(|i| self.hits.get(i))
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

const DEBOUNCE: Duration = Duration::from_millis(100);
const POLL_TIMEOUT: Duration = Duration::from_millis(20);

async fn event_loop(
    term:     &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ipc_path: &Path,
) -> anyhow::Result<()> {
    let mut app = App::new();

    loop {
        term.draw(|f| render(f, &mut app))?;

        // Fire debounced search if needed.
        if let Some(last) = app.last_key_at {
            if app.search_pending && last.elapsed() >= DEBOUNCE {
                app.search_pending = false;
                app.last_key_at    = None;
                let q = app.full_query();
                if !q.is_empty() {
                    search(&mut app, ipc_path, q).await;
                } else {
                    app.hits        = Vec::new();
                    app.total_hits  = 0;
                    app.took_ms     = None;
                    app.list_state  = ListState::default();
                    app.status_msg  = String::new();
                }
            }
        }

        // Poll for terminal events (non-blocking with timeout).
        if !event::poll(POLL_TIMEOUT)? {
            continue;
        }

        if let Event::Key(key) = event::read()? {
            if handle_key(&mut app, key, ipc_path).await? {
                break; // exit requested
            }
        }
    }

    Ok(())
}

/// Returns `true` when the user requested exit.
async fn handle_key(app: &mut App, key: KeyEvent, ipc_path: &Path) -> anyhow::Result<bool> {
    match key.code {
        // Exit
        KeyCode::Esc => return Ok(true),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(true),

        // Navigate results
        KeyCode::Down => app.select_next(),
        KeyCode::Up   => app.select_prev(),

        // Open selected file
        KeyCode::Enter => {
            if let Some(hit) = app.selected_hit() {
                let path = hit.path.clone();
                open_file(app, ipc_path, path).await;
            }
        }

        // Cycle mode
        KeyCode::Tab => {
            app.mode           = app.mode.next();
            app.search_pending = true;
            app.last_key_at    = Some(Instant::now());
        }

        // Force re-index
        KeyCode::F(5) => {
            reindex(ipc_path).await;
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
// IPC helpers
// ---------------------------------------------------------------------------

async fn search(app: &mut App, ipc_path: &Path, query: String) {
    match send_request(ipc_path, IpcRequest::Search { query, limit: app.limit, offset: 0 }).await {
        Ok(IpcResponse::Search(sr)) => {
            app.hits       = sr.hits;
            app.total_hits = sr.total_hits;
            app.took_ms    = None; // IpcResponse doesn't carry took_ms
            let n = app.hits.len();
            app.status_msg = format!("{} result(s) (total: {})", n, app.total_hits);
            if !app.hits.is_empty() && app.list_state.selected().is_none() {
                app.list_state.select(Some(0));
            }
            if app.hits.is_empty() {
                app.list_state = ListState::default();
            }
        }
        Ok(_) => {}
        Err(e) => {
            app.status_msg = format!("search error: {e}");
        }
    }
}

async fn open_file(app: &mut App, ipc_path: &Path, path: String) {
    match send_request(ipc_path, IpcRequest::Open { path }).await {
        Ok(IpcResponse::Open { opened: true, .. }) => {
            app.status_msg = "file opened".into();
        }
        Ok(IpcResponse::Open { opened: false, .. }) => {
            app.status_msg = "daemon could not open file".into();
        }
        Ok(_)  => {}
        Err(e) => {
            app.status_msg = format!("open error: {e}");
        }
    }
}

async fn reindex(ipc_path: &Path) {
    let _ = send_request(ipc_path, IpcRequest::Reindex { full: true }).await;
}

async fn send_request(
    ipc_path: &Path,
    req:      IpcRequest,
) -> Result<IpcResponse, IpcClientError> {
    let mut client = IpcClient::connect(ipc_path).await?;
    client.request(req).await
}

// ---------------------------------------------------------------------------
// Rendering
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
    let mode_label = app.mode.label();
    let input_title = format!(" Search [{mode_label}] ");
    let search_block = Block::default()
        .borders(Borders::ALL)
        .title(input_title.as_str());
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
    let items: Vec<ListItem> = app.hits.iter().map(|h| {
        let zip_tag = if h.archive_path.is_some() {
            Span::styled(" [zip]", Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM))
        } else {
            Span::raw("")
        };
        let ext_str = h.extension.as_deref().unwrap_or("");
        let size_str = humanize_size(h.size_bytes);
        let line = Line::from(vec![
            Span::raw(h.path.clone()),
            Span::styled(
                format!("  {size_str}  {ext_str}"),
                Style::default().fg(Color::DarkGray),
            ),
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
