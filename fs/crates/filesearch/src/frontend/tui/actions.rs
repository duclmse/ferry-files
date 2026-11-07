// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

use super::app::App;
use super::render::render;
use crate::api::ipc_protocol::{IpcRequest, IpcResponse};

pub async fn run(ipc_path: &Path) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
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
    term: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ipc_path: &Path,
) -> anyhow::Result<()> {
    let mut app = App::new(ipc_path);

    loop {
        term.draw(|f| render(f, &mut app))?;

        // Fire debounced search if needed.
        if let Some(last) = app.last_key_at {
            if app.search_pending && last.elapsed() >= DEBOUNCE {
                app.search_pending = false;
                app.last_key_at = None;
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
        KeyCode::Up => app.select_prev(),

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
            app.last_key_at = Some(Instant::now());
        }
        KeyCode::Backspace => {
            app.input.pop();
            app.search_pending = true;
            app.last_key_at = Some(Instant::now());
        }

        _ => {}
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// IPC helpers - all go through app.send() for connection reuse
// ---------------------------------------------------------------------------

async fn search(app: &mut App, query: String) {
    let t0 = Instant::now();
    match app
        .send(IpcRequest::Search {
            query,
            limit: app.limit,
            offset: 0,
        })
        .await
    {
        Ok(IpcResponse::Search(sr)) => {
            let took_ms = t0.elapsed().as_millis() as u64;
            app.load_hits(sr.hits, sr.total_hits, took_ms);
            fetch_stats(app).await;
        }
        Ok(_) => {}
        Err(e) => {
            app.status_msg = format!("search error: {e}");
        }
    }
}

async fn fetch_stats(app: &mut App) {
    if let Ok(IpcResponse::Stats(s)) = app.send(IpcRequest::Stats).await {
        app.load_stats(s.total_files, s.archive_entries_indexed);
    }
}

async fn open_file(app: &mut App, path: String) {
    match app.send(IpcRequest::Open { path }).await {
        Ok(IpcResponse::Open { opened: true, .. }) => {
            app.status_msg = "file opened".into();
        }
        Ok(IpcResponse::Open { opened: false, .. }) => {
            app.status_msg = "daemon could not open file".into();
        }
        Ok(_) => {}
        Err(e) => {
            app.status_msg = format!("open error: {e}");
        }
    }
}

async fn reindex(app: &mut App) {
    let _ = app.send(IpcRequest::Reindex { full: true }).await;
}
