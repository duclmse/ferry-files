use std::collections::HashMap;

use crate::error::Result;
use crate::store::is_under;
use super::matching::{dupe_key, like_match, meta_filter_matches, parent_dir};
use super::state::EntryRef;
use super::{DupeKind, FileRecord, FuzzyRank, MemoryStore, MetaFilters, RecordView, SqlCondition};

fn all_match(conds: &[SqlCondition], e: &EntryRef<'_>) -> bool {
    conds.iter().all(|c| c.matches(e))
}

impl MemoryStore {
    /// Scan under one read lock; only matching entries become `FileRecord`s.
    fn collect_where(
        &self,
        limit: usize,
        pred:  impl Fn(&EntryRef<'_>) -> bool,
    ) -> Vec<FileRecord> {
        let state = self.read_state();
        state.iter()
            .filter(|e| pred(e))
            .take(limit)
            .map(|e| e.to_record())
            .collect()
    }

    pub fn has_child(&self, parent_path: &str, name_pattern: &str) -> Result<bool> {
        let state = self.read_state();
        let found = state.iter().any(|e| {
            parent_dir(e.path_str()) == parent_path && like_match(e.name_str(), name_pattern)
        });
        Ok(found)
    }

    /// Mirror of `SqliteStore::children_of`: direct (non-recursive) real-file
    /// children of `parent_path`.
    pub fn children_of(&self, parent_path: &str) -> Result<Vec<String>> {
        let state = self.read_state();
        Ok(state.iter()
            .filter(|e| e.archive_str().is_none() && parent_dir(e.path_str()) == parent_path)
            .map(|e| e.path_str().to_owned())
            .collect())
    }

    pub fn list_archive_paths(&self) -> Result<Vec<String>> {
        let state = self.read_state();
        Ok(state.iter()
            .filter(|e| e.archive_str().is_none() && !e.is_dir() && e.ext_str() == Some("zip"))
            .map(|e| e.path_str().to_owned())
            .collect())
    }

    pub fn search_meta(&self, filters: &MetaFilters, limit: usize) -> Result<Vec<FileRecord>> {
        Ok(self.collect_where(limit, |e| meta_filter_matches(e, filters)))
    }

    /// Mirror of `SqliteStore::search_name`: `name LIKE pattern` + optional meta-filters.
    pub fn search_name(
        &self,
        name:    &str,
        filters: &MetaFilters,
        limit:   usize,
    ) -> Result<Vec<FileRecord>> {
        Ok(self.collect_where(limit, |e| {
            like_match(e.name_str(), name) && meta_filter_matches(e, filters)
        }))
    }

    /// Mirror of `SqliteStore::search_extension`: exact extension match + optional meta-filters.
    pub fn search_extension(
        &self,
        ext:     &str,
        filters: &MetaFilters,
        limit:   usize,
    ) -> Result<Vec<FileRecord>> {
        Ok(self.collect_where(limit, |e| {
            e.ext_str() == Some(ext) && meta_filter_matches(e, filters)
        }))
    }

    /// Mirror of `SqliteStore::paths_under`: all paths recursively under `dir`.
    pub fn paths_under(&self, dir: &str) -> Result<Vec<String>> {
        let state = self.read_state();
        Ok(state.iter()
            .filter(|e| is_under(e.path_str(), dir))
            .map(|e| e.path_str().to_owned())
            .collect())
    }

    pub fn search_conditions(&self, conds: &[SqlCondition], limit: usize) -> Result<Vec<FileRecord>> {
        Ok(self.collect_where(limit, |e| all_match(conds, e)))
    }

    /// Ranks borrowed names under the read lock; only the winners are copied
    /// out.
    pub fn fuzzy_search(
        &self,
        conds: &[SqlCondition],
        cap:   usize,
        rank:  &FuzzyRank<'_>,
    ) -> Result<Vec<FileRecord>> {
        let state = self.read_state();
        let candidates: Vec<EntryRef<'_>> = state.iter()
            .filter(|e| all_match(conds, e))
            .take(cap)
            .collect();
        let names: Vec<&str> = candidates.iter().map(|e| e.name_str()).collect();
        Ok(rank(&names).into_iter().map(|i| candidates[i].to_record()).collect())
    }

    pub fn search_by_paths(
        &self,
        paths: &[String],
        conds: &[SqlCondition],
        limit: usize,
    ) -> Result<Vec<FileRecord>> {
        let state = self.read_state();
        let mut results = Vec::new();
        for path in paths.iter().take(limit * 4) {
            if let Some(i) = state.find(path) {
                let e = state.view(i);
                if all_match(conds, &e) {
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
        kind:  DupeKind,
        conds: &[SqlCondition],
        limit: usize,
    ) -> Result<Vec<FileRecord>> {
        let state = self.read_state();

        let mut groups: HashMap<String, Vec<EntryRef<'_>>> = HashMap::new();
        for e in state.iter() {
            if e.archive_str().is_some() || !all_match(conds, &e) { continue; }
            groups.entry(dupe_key(&e, kind)).or_default().push(e);
        }

        let mut keys: Vec<&String> = groups.keys().collect();
        keys.sort();
        let mut result = Vec::new();
        'outer: for key in keys {
            let entries = &groups[key];
            if entries.len() <= 1 { continue; }
            for e in entries {
                result.push(e.to_record());
                if result.len() >= limit { break 'outer; }
            }
        }
        Ok(result)
    }
}
