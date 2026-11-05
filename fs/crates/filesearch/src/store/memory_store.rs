//! In-memory file-metadata store with binary snapshot persistence.
//!
//! Designed after the approach in `docs/research/in-memory-index.md`: records live in a flat
//! `Vec<FileRecord>` plus a `HashMap` path-index for O(1) lookups. Snapshots
//! are written as a small binary file (`index.bin`) on clean shutdown and
//! reloaded on the next start - near-zero load latency via sequential read.
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

impl MemoryStore {
    // A poisoned lock means a writer panicked mid-call; every mutation on
    // IndexState leaves it structurally valid between statements, so keep
    // serving rather than turning one panic into a panic on every request.
    fn read_state(&self) -> std::sync::RwLockReadGuard<'_, IndexState> {
        self.state.read().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write_state(&self) -> std::sync::RwLockWriteGuard<'_, IndexState> {
        self.state.write().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
