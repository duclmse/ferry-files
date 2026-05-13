use crate::error::Result;
use crate::store::{DupeKind, FileRecord, FileStore, SqlCondition};

/// Find duplicate files grouped by `kind`.
/// Works with both SQLite and Memory backends via `FileStore`.
pub fn find_dupes(
    store:            &FileStore,
    kind:             DupeKind,
    extra_conditions: &[SqlCondition],
    limit:            usize,
) -> Result<Vec<FileRecord>> {
    store.find_dupes(kind, extra_conditions, limit)
}
