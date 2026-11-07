use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

mod archive_entries;
mod bench;
mod bulk;
mod crud;
mod schema;
mod search;
#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------

pub struct SqliteStore {
    pool: Pool<SqliteConnectionManager>,
    name_bounds: std::sync::Mutex<search::NameBounds>,
    /// Whether `idx_files_name` is configured; the name searches name it in
    /// `INDEXED BY` and need a plain scan without it.
    name_index: bool,
}
