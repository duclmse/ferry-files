use crate::error::Result;
use crate::store::{DupeKind, FileRecord, SqlCondition, sqlite_store::SqliteStore};

/// Find duplicate files grouped by `kind`.
/// Delegates to the two-pass SQL query in `SqliteStore::find_dupes`.
pub fn find_dupes(
    sqlite:           &SqliteStore,
    kind:             DupeKind,
    extra_conditions: &[SqlCondition],
    limit:            usize,
) -> Result<Vec<FileRecord>> {
    sqlite.find_dupes(kind, extra_conditions, limit)
}
