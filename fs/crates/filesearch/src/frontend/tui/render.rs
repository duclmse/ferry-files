// ---------------------------------------------------------------------------
// Rendering - zero heap allocations for stable frames
// ---------------------------------------------------------------------------

use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Frame,
};

use super::app::App;

pub(super) fn render(f: &mut Frame, app: &mut App) {
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
        // Borrows the pre-computed String - no allocation.
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
    // Borrows pre-computed strings - no clone, no format! per row per frame.
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

pub(super) fn humanize_size(bytes: u64) -> String {
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

pub(super) fn humanize_count(n: u64) -> String {
    if n < 1_000 {
        format!("{n}")
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}
