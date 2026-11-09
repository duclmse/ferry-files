use std::collections::HashMap;
use std::sync::OnceLock;

use rayon::prelude::*;

use super::matching::{compile, dupe_key, meta_filter_matches, Compiled, Like};
use super::state::{EntryRef, IndexState};
use super::{DupeKind, FileRecord, FuzzyRank, MemoryStore, MetaFilters, RecordView, SqlCondition};
use crate::error::Result;

fn all_match(conds: &[Compiled<'_>], e: &EntryRef<'_>) -> bool {
    conds.iter().all(|c| c.matches(e))
}

/// Entries per scan unit: small enough that a query satisfied early does
/// little extra work.
const SCAN_BLOCK: usize = 1 << 15;

/// Scans run on their own pool. A scan holds the store's read lock while it
/// waits for its blocks, so sharing workers with jobs that take that lock
/// (post-filters on the global pool) could deadlock behind a waiting writer.
fn scan_pool() -> Option<&'static rayon::ThreadPool> {
    static POOL: OnceLock<Option<rayon::ThreadPool>> = OnceLock::new();
    POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .thread_name(|i| format!("memory-scan-{i}"))
            .build()
            .map_err(|e| tracing::warn!("memory store: scanning on one thread: {e}"))
            .ok()
    })
    .as_ref()
}

/// The first `limit` matching entry numbers, in entry order. That order is
/// what makes `offset` paging stable, so blocks are joined in order, and the
/// number of blocks per round grows so early matches stay cheap.
fn matching_entries(
    state: &IndexState,
    limit: usize,
    pred: impl Fn(&EntryRef<'_>) -> bool + Sync,
) -> Vec<u32> {
    let scan_block = |first: usize| -> Vec<u32> {
        state
            .iter_range(first..(first + SCAN_BLOCK).min(state.slots()))
            .filter(|e| pred(e))
            .take(limit)
            .map(|e| e.index())
            .collect()
    };
    let pool = scan_pool();
    let max_blocks = pool.map_or(1, |p| p.current_num_threads() * 4);
    let mut hits: Vec<u32> = Vec::new();
    let mut next = 0;
    let mut blocks = 1;
    while next < state.slots() && hits.len() < limit {
        let starts: Vec<usize> = (next..state.slots())
            .step_by(SCAN_BLOCK)
            .take(blocks)
            .collect();
        next += blocks * SCAN_BLOCK;
        let found: Vec<Vec<u32>> = match pool {
            Some(pool) if starts.len() > 1 => {
                pool.install(|| starts.par_iter().map(|&s| scan_block(s)).collect())
            }
            _ => starts.iter().map(|&s| scan_block(s)).collect(),
        };
        hits.extend(found.into_iter().flatten());
        blocks = (blocks * 2).min(max_blocks);
    }
    hits.truncate(limit);
    hits
}

impl MemoryStore {
    /// Scan under one read lock; only matching entries become `FileRecord`s.
    fn collect_where(
        &self,
        limit: usize,
        pred: impl Fn(&EntryRef<'_>) -> bool + Sync,
    ) -> Vec<FileRecord> {
        let state = self.read_state();
        matching_entries(&state, limit, pred)
            .into_iter()
            .map(|i| state.view(i).to_record())
            .collect()
    }

    pub fn has_child(&self, parent_path: &str, name_pattern: &str) -> Result<bool> {
        let state = self.read_state();
        let like = Like::new(name_pattern);
        let found = state
            .iter()
            .any(|e| like.matches(e.name_str()) && e.under(parent_path) == Some(true));
        Ok(found)
    }

    /// Mirror of `SqliteStore::children_of`: direct (non-recursive) real-file
    /// children of `parent_path`.
    pub fn children_of(&self, parent_path: &str) -> Result<Vec<String>> {
        let state = self.read_state();
        Ok(state
            .iter()
            .filter(|e| !e.is_archive() && e.under(parent_path) == Some(true))
            .map(|e| e.path_string())
            .collect())
    }

    pub fn list_archive_paths(&self) -> Result<Vec<String>> {
        let state = self.read_state();
        Ok(state
            .iter()
            .filter(|e| !e.is_archive() && !e.is_dir() && e.ext_str() == Some("zip"))
            .map(|e| e.path_string())
            .collect())
    }

    pub fn search_meta(&self, filters: &MetaFilters, limit: usize) -> Result<Vec<FileRecord>> {
        Ok(self.collect_where(limit, |e| meta_filter_matches(e, filters)))
    }

    /// Mirror of `SqliteStore::search_name`: `name LIKE pattern` + optional meta-filters.
    pub fn search_name(
        &self,
        name: &str,
        filters: &MetaFilters,
        limit: usize,
    ) -> Result<Vec<FileRecord>> {
        let like = Like::new(name);
        Ok(self.collect_where(limit, |e| {
            like.matches(e.name_str()) && meta_filter_matches(e, filters)
        }))
    }

    /// Mirror of `SqliteStore::search_extension`: exact extension match + optional meta-filters.
    pub fn search_extension(
        &self,
        ext: &str,
        filters: &MetaFilters,
        limit: usize,
    ) -> Result<Vec<FileRecord>> {
        Ok(self.collect_where(limit, |e| {
            e.ext_str() == Some(ext) && meta_filter_matches(e, filters)
        }))
    }

    /// Mirror of `SqliteStore::paths_under`: all paths recursively under `dir`.
    pub fn paths_under(&self, dir: &str) -> Result<Vec<String>> {
        let state = self.read_state();
        Ok(state
            .iter()
            .filter(|e| e.under(dir).is_some())
            .map(|e| e.path_string())
            .collect())
    }

    pub fn search_conditions(
        &self,
        conds: &[SqlCondition],
        limit: usize,
    ) -> Result<Vec<FileRecord>> {
        let compiled = compile(conds);
        Ok(self.collect_where(limit, |e| all_match(&compiled, e)))
    }

    /// Ranks borrowed names under the read lock; only the winners are copied
    /// out.
    pub fn fuzzy_search(
        &self,
        conds: &[SqlCondition],
        cap: usize,
        rank: &FuzzyRank<'_>,
    ) -> Result<Vec<FileRecord>> {
        let state = self.read_state();
        let compiled = compile(conds);
        let candidates: Vec<EntryRef<'_>> =
            matching_entries(&state, cap, |e| all_match(&compiled, e))
                .into_iter()
                .map(|i| state.view(i))
                .collect();
        let names: Vec<&str> = candidates.iter().map(|e| e.name_str()).collect();
        Ok(rank(&names)
            .into_iter()
            .map(|i| candidates[i].to_record())
            .collect())
    }

    pub fn search_by_paths(
        &self,
        paths: &[String],
        conds: &[SqlCondition],
        limit: usize,
    ) -> Result<Vec<FileRecord>> {
        let state = self.read_state();
        let compiled = compile(conds);
        let mut results = Vec::new();
        for path in paths.iter().take(limit * 4) {
            if let Some(i) = state.find(path) {
                let e = state.view(i);
                if all_match(&compiled, &e) {
                    results.push(e.to_record());
                }
            }
            if results.len() >= limit {
                break;
            }
        }
        Ok(results)
    }

    pub fn find_dupes(
        &self,
        kind: DupeKind,
        conds: &[SqlCondition],
        limit: usize,
    ) -> Result<Vec<FileRecord>> {
        let state = self.read_state();
        let compiled = compile(conds);

        let mut groups: HashMap<String, Vec<EntryRef<'_>>> = HashMap::new();
        for e in state.iter() {
            if e.is_archive() || !all_match(&compiled, &e) {
                continue;
            }
            groups.entry(dupe_key(&e, kind)).or_default().push(e);
        }

        let mut keys: Vec<&String> = groups.keys().collect();
        keys.sort();
        let mut result = Vec::new();
        'outer: for key in keys {
            let entries = &groups[key];
            if entries.len() <= 1 {
                continue;
            }
            for e in entries {
                result.push(e.to_record());
                if result.len() >= limit {
                    break 'outer;
                }
            }
        }
        Ok(result)
    }
}
