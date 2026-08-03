use std::collections::HashMap;

use crate::error::Result;
use super::matching::{dupe_key, like_match, meta_filter_matches, parent_dir};
use super::{DupeKind, FileRecord, MemoryStore, MetaFilters, SqlCondition};

impl MemoryStore {
    pub fn has_child(&self, parent_path: &str, name_pattern: &str) -> Result<bool> {
        let state = self.state.read().unwrap();
        Ok(state.records.iter().any(|r| {
            parent_dir(&r.path) == parent_path && like_match(&r.name, name_pattern)
        }))
    }

    /// Mirror of `SqliteStore::children_of`: direct (non-recursive) real-file
    /// children of `parent_path`.
    pub fn children_of(&self, parent_path: &str) -> Result<Vec<String>> {
        let state = self.state.read().unwrap();
        Ok(state.records.iter()
            .filter(|r| r.archive_path.is_none() && parent_dir(&r.path) == parent_path)
            .map(|r| r.path.clone())
            .collect())
    }

    pub fn list_archive_paths(&self) -> Result<Vec<String>> {
        let state = self.state.read().unwrap();
        Ok(state.records.iter()
            .filter(|r| r.archive_path.is_none() && !r.is_dir
                && r.extension.as_deref() == Some("zip"))
            .map(|r| r.path.clone())
            .collect())
    }

    pub fn search_meta(&self, filters: &MetaFilters, limit: usize) -> Result<Vec<FileRecord>> {
        let state = self.state.read().unwrap();
        Ok(state.records.iter()
            .filter(|r| meta_filter_matches(r, filters))
            .take(limit)
            .cloned()
            .collect())
    }

    /// Mirror of `SqliteStore::search_name`: `name LIKE pattern` + optional meta-filters.
    pub fn search_name(
        &self,
        name:    &str,
        filters: &MetaFilters,
        limit:   usize,
    ) -> Result<Vec<FileRecord>> {
        let state = self.state.read().unwrap();
        Ok(state.records.iter()
            .filter(|r| like_match(&r.name, name) && meta_filter_matches(r, filters))
            .take(limit)
            .cloned()
            .collect())
    }

    /// Mirror of `SqliteStore::search_extension`: exact extension match + optional meta-filters.
    pub fn search_extension(
        &self,
        ext:     &str,
        filters: &MetaFilters,
        limit:   usize,
    ) -> Result<Vec<FileRecord>> {
        let state = self.state.read().unwrap();
        Ok(state.records.iter()
            .filter(|r| r.extension.as_deref() == Some(ext) && meta_filter_matches(r, filters))
            .take(limit)
            .cloned()
            .collect())
    }

    /// Mirror of `SqliteStore::paths_under`: all paths recursively under `dir`.
    pub fn paths_under(&self, dir: &str) -> Result<Vec<String>> {
        let prefix = format!("{dir}/");
        let state = self.state.read().unwrap();
        Ok(state.records.iter()
            .filter(|r| r.path.starts_with(&prefix))
            .map(|r| r.path.clone())
            .collect())
    }

    pub fn search_conditions(&self, conds: &[SqlCondition], limit: usize) -> Result<Vec<FileRecord>> {
        if conds.is_empty() {
            return self.search_meta(&MetaFilters::default(), limit);
        }
        let state = self.state.read().unwrap();
        Ok(state.records.iter()
            .filter(|r| conds.iter().all(|c| c.matches(r)))
            .take(limit)
            .cloned()
            .collect())
    }

    pub fn search_by_paths(
        &self,
        paths: &[String],
        conds: &[SqlCondition],
        limit: usize,
    ) -> Result<Vec<FileRecord>> {
        let state = self.state.read().unwrap();
        let mut results = Vec::new();
        for path in paths.iter().take(limit * 4) {
            if let Some(&idx) = state.by_path.get(path) {
                let r = &state.records[idx];
                if conds.is_empty() || conds.iter().all(|c| c.matches(r)) {
                    results.push(r.clone());
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
        let state = self.state.read().unwrap();

        let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, r) in state.records.iter().enumerate() {
            if r.archive_path.is_some() { continue; }
            if !conds.is_empty() && !conds.iter().all(|c| c.matches(r)) { continue; }
            let key = dupe_key(r, kind);
            groups.entry(key).or_default().push(i);
        }

        let mut keys: Vec<&String> = groups.keys().collect();
        keys.sort();
        let mut result = Vec::new();
        'outer: for key in keys {
            let indices = &groups[key];
            if indices.len() <= 1 { continue; }
            for &idx in indices {
                result.push(state.records[idx].clone());
                if result.len() >= limit { break 'outer; }
            }
        }
        Ok(result)
    }
}
