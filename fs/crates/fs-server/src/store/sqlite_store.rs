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
    /// One connection for every write. SQLite takes one writer at a time
    /// anyway; queueing here hands over at once, where writers contending
    /// inside SQLite sleep in its busy handler for up to 100 ms per retry.
    writer: Pool<SqliteConnectionManager>,
    name_bounds: std::sync::Mutex<search::NameBounds>,
    /// Whether `idx_files_name` is configured; the name searches name it in
    /// `INDEXED BY` and need a plain scan without it.
    name_index: bool,
    /// Whether `idx_files_extension` is configured; without it the `*.ext`
    /// shortcut would scan the table on top of the regular search.
    ext_index: bool,
}
