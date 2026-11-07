//! Archive registry: tracks registered ZIPs, their `.zidx` sidecars, and
//! caches open `ZidxIndex`/`LocalFileSource` handles for the streaming API.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use dashmap::DashMap;

use crate::archive::{dzx_path_for, DzxIndex, LocalFileSource};
use crate::zidx::{zidx_path_for, ZidxIndex};

// ---------------------------------------------------------------------------
// Archive ID
// ---------------------------------------------------------------------------

/// Blake3 hex of zip_path string, first 24 chars (96-bit prefix).
/// Stable across restarts; collision-negligible for realistic archive counts.
pub fn archive_id(zip_path: &Path) -> String {
    let key = zip_path.to_string_lossy();
    let hex = blake3::hash(key.as_bytes()).to_hex();
    hex[..24].to_string()
}

// ---------------------------------------------------------------------------
// Registry internals
// ---------------------------------------------------------------------------

pub(super) struct RegistryEntry {
    pub(super) zip_path: PathBuf,
    pub(super) zidx_path: PathBuf,
    /// Weak validator for `ETag:` - derived from file mtime+size at register
    /// time, O(1) stat only.  Stale after rebuild but that also re-registers.
    pub(super) etag: String,
}

pub struct ArchiveSummary {
    pub id: String,
    pub zip_path: PathBuf,
}

// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// ArchiveRegistry
// ---------------------------------------------------------------------------

pub struct ArchiveRegistry {
    pub(super) entries: DashMap<String, RegistryEntry>,
    /// Per-shard concurrent cache - no global lock on the hot read path.
    /// `index_cache_size` from config is retained in the struct for future
    /// bounded-eviction support but not enforced here; ZidxIndex is mmap-backed
    /// and holds negligible memory beyond the OS page cache.
    index_cache: DashMap<String, Arc<ZidxIndex>>,
    /// Cached open file handle per archive - avoids an open()+fstat() syscall
    /// on every streaming request.  Evicted (along with index_cache) on rebuild.
    source_cache: DashMap<String, Arc<LocalFileSource>>,
    zidx_dir: PathBuf,
    /// `None` disables `.dzx`-backed Range reads (requests fall back to a full stream).
    dzx_dir: Option<PathBuf>,
    enabled: bool,
}

/// Compute a cheap ETag from file mtime + size - one stat syscall, O(1).
fn zidx_etag(path: &Path) -> String {
    use std::time::UNIX_EPOCH;
    std::fs::metadata(path)
        .ok()
        .and_then(|m| {
            let secs = m
                .modified()
                .ok()?
                .duration_since(UNIX_EPOCH)
                .ok()?
                .as_secs();
            Some(format!("{secs:016x}-{:016x}", m.len()))
        })
        .unwrap_or_else(|| "unknown".into())
}

impl ArchiveRegistry {
    pub fn new(zidx_dir: PathBuf, _cache_size: usize) -> Self {
        Self {
            entries: DashMap::new(),
            index_cache: DashMap::new(),
            source_cache: DashMap::new(),
            zidx_dir,
            dzx_dir: None,
            enabled: true,
        }
    }

    pub fn disabled() -> Self {
        Self {
            entries: DashMap::new(),
            index_cache: DashMap::new(),
            source_cache: DashMap::new(),
            zidx_dir: PathBuf::new(),
            dzx_dir: None,
            enabled: false,
        }
    }

    pub fn with_dzx_dir(mut self, dzx_dir: PathBuf) -> Self {
        self.dzx_dir = Some(dzx_dir);
        self
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Register a ZIP and its .zidx sidecar. Called after ZidxBuilder::build() succeeds.
    pub fn register(&self, zip_path: PathBuf) {
        if !self.enabled {
            return;
        }
        let id = archive_id(&zip_path);
        let zidx_path = zidx_path_for(&self.zidx_dir, &zip_path);
        let etag = zidx_etag(&zidx_path);
        // Invalidate caches so the next request re-opens the freshly rebuilt file.
        self.index_cache.remove(&id);
        self.source_cache.remove(&id);
        self.entries.insert(
            id,
            RegistryEntry {
                zip_path,
                zidx_path,
                etag,
            },
        );
    }

    /// Remove a ZIP from the registry. Called when the ZIP is deleted.
    pub fn unregister(&self, zip_path: &Path) {
        if !self.enabled {
            return;
        }
        let id = archive_id(zip_path);
        self.entries.remove(&id);
        self.index_cache.remove(&id);
        self.source_cache.remove(&id);
    }

    /// Open (or return a cached) ZidxIndex for the given archive ID.
    /// Returns None if the ID is unknown or the .zidx file is missing/corrupt.
    pub fn index(&self, id: &str) -> Option<Arc<ZidxIndex>> {
        if !self.enabled {
            return None;
        }
        // Fast path: warm cache - DashMap shard lock, no blocking I/O.
        if let Some(idx) = self.index_cache.get(id) {
            return Some(Arc::clone(&*idx));
        }
        // Verify the archive is registered before opening the file.
        let zidx_path = self.entries.get(id)?.zidx_path.clone();
        // Open the file outside any lock.
        match ZidxIndex::open(&zidx_path) {
            Ok(idx) => {
                let idx = Arc::new(idx);
                // or_insert avoids overwriting a concurrent winner.
                let stored = self
                    .index_cache
                    .entry(id.to_string())
                    .or_insert_with(|| Arc::clone(&idx));
                Some(Arc::clone(&*stored))
            }
            Err(e) => {
                tracing::warn!("failed to open .zidx for archive {id}: {e}");
                None
            }
        }
    }

    /// Resolve an archive ID to its ZIP path.
    pub fn zip_path(&self, id: &str) -> Option<PathBuf> {
        if !self.enabled {
            return None;
        }
        self.entries.get(id).map(|e| e.zip_path.clone())
    }

    /// Return a cached (or freshly opened) `LocalFileSource` for `id`.
    /// Returns `None` if the archive is unknown or the file cannot be opened.
    pub fn source(&self, id: &str) -> Option<Arc<LocalFileSource>> {
        if !self.enabled {
            return None;
        }
        if let Some(src) = self.source_cache.get(id) {
            return Some(Arc::clone(&*src));
        }
        let zip_path = self.entries.get(id)?.zip_path.clone();
        match LocalFileSource::new(zip_path) {
            Ok(src) => {
                let src = Arc::new(src);
                let stored = self
                    .source_cache
                    .entry(id.to_string())
                    .or_insert_with(|| Arc::clone(&src));
                Some(Arc::clone(&*stored))
            }
            Err(e) => {
                tracing::warn!("failed to open source for archive {id}: {e}");
                None
            }
        }
    }

    /// Open the `.dzx` sidecar for the entry at `local_header_offset`, if one
    /// was built. Not cached: `DzxIndex::open` is a single mmap, and sidecars
    /// can appear after registration (they are built after the `.zidx`).
    pub fn dzx(&self, id: &str, local_header_offset: u64) -> Option<DzxIndex> {
        if !self.enabled {
            return None;
        }
        let dzx_dir = self.dzx_dir.as_ref()?;
        let zip_path = self.entries.get(id)?.zip_path.clone();
        let path = dzx_path_for(dzx_dir, &zip_path, local_header_offset);
        if !path.exists() {
            return None;
        }
        match DzxIndex::open(&path) {
            Ok(dzx) => Some(dzx),
            Err(e) => {
                tracing::warn!("failed to open .dzx {}: {e}", path.display());
                None
            }
        }
    }

    /// List all registered archives.
    pub fn list(&self) -> Vec<ArchiveSummary> {
        if !self.enabled {
            return vec![];
        }
        self.entries
            .iter()
            .map(|e| ArchiveSummary {
                id: e.key().clone(),
                zip_path: e.value().zip_path.clone(),
            })
            .collect()
    }
}
