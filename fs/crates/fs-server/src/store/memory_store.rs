//! In-memory file-metadata store with binary snapshot persistence.
//!
//! Layout follows `docs/plans/everything-borrowing.md` (phase 3): entries are
//! fixed-size records holding a parent entry number and the path's tail (the
//! last component, stored once in a shared string arena), so a full path is
//! rebuilt by walking the parent chain. Names are slices of the tail, and the
//! path index is a hash table of entry numbers. Snapshots (`index.bin`) hold
//! those arrays plus the key-value metadata, so a restart reloads the index
//! without rebuilding strings or re-crawling.
//!
//! This store is single-writer: all mutating methods take an exclusive lock.
//! Use the SQLite store when you need concurrent writers.

use std::sync::atomic::AtomicU64;
use std::sync::{Mutex, RwLock};

use super::{
    CmpOp, DupeKind, FileRecord, FuzzyKeep, FuzzyRank, MetaFilters, RecordView, Sort, SortKey,
    SqlCondition,
};

mod crud;
mod matching;
mod name_filter;
mod search;
mod snapshot;
mod sort;
mod state;
#[cfg(test)]
mod tests;

use state::IndexState;

// ---------------------------------------------------------------------------

pub struct MemoryStore {
    state: RwLock<IndexState>,
    /// `IndexState::revision` of the last save.
    saved_revision: AtomicU64,
    /// Saves share one temporary file, so they must not overlap.
    saving: Mutex<()>,
}

impl MemoryStore {
    // A poisoned lock means a writer panicked mid-call; every mutation on
    // IndexState leaves it structurally valid between statements, so keep
    // serving rather than turning one panic into a panic on every request.
    fn read_state(&self) -> std::sync::RwLockReadGuard<'_, IndexState> {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write_state(&self) -> std::sync::RwLockWriteGuard<'_, IndexState> {
        self.state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
