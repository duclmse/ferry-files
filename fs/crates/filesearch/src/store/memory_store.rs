//! In-memory file-metadata store with binary snapshot persistence.
//!
//! Designed after the approach in `docs/design.md`: records live in a flat
//! `Vec<FileRecord>` plus a `HashMap` path-index for O(1) lookups. Snapshots
//! are written as a small binary file (`index.bin`) on clean shutdown and
//! reloaded on the next start — near-zero load latency via sequential read.
//!
//! This store is single-writer: all mutating methods take an exclusive lock.
//! Use the SQLite store when you need concurrent writers.

use std::sync::RwLock;

use super::{CmpOp, DupeKind, FileRecord, MetaFilters, SqlCondition};

mod crud;
mod matching;
mod search;
mod state;
#[cfg(test)]
mod tests;

use state::IndexState;

// ---------------------------------------------------------------------------

pub struct MemoryStore {
    state: RwLock<IndexState>,
}
