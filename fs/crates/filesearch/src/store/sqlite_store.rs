use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

mod schema;
mod crud;
mod search;
mod archive_entries;
mod bench;
mod bulk;
#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------

pub struct SqliteStore {
    pool:        Pool<SqliteConnectionManager>,
    name_bounds: std::sync::Mutex<search::NameBounds>,
}
