//! Interactive ratatui TUI (`fs tui`).
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
//!   Type - update query (debounced 100 ms → IPC search)
//!   ↑ / ↓ - navigate results
//!   Enter - open selected file via /open API
//!   Tab - cycle mode: exact → fuzzy: → regex: → content: → archive:
//!   Ctrl+C/Esc - exit
//!   F5 - force re-index

mod actions;
mod app;
mod render;
#[cfg(test)]
mod tests;

pub use actions::run;

// Only needed so `tests`' `use super::*;` can name these directly, exactly as
// when they were all defined inline in this file. `HitRow` is intentionally
// omitted: tests only ever go through `app.rows[..]`, never naming the type.
#[cfg(test)]
use app::{App, Mode};
#[cfg(test)]
use render::{humanize_count, humanize_size};
