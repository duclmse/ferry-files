use std::path::Path;
use std::sync::atomic::Ordering;

use crate::api::archive::ArchiveRegistry;
use crate::error::Result;
use crate::indexer::archive::{ArchiveCrawler, archive_entry_to_record};
use crate::store::{
    FileRecord,
    FileStore,
    tantivy_store::TantivyStore,
};

use super::dzx_sidecars::build_dzx_sidecars;
use super::metrics::IndexMetrics;

// ---------------------------------------------------------------------------
// crawl_archive - wraps all inner upserts in a single SQLite transaction
// ---------------------------------------------------------------------------

/// Sidecar-index build config, grouped to keep `crawl_archive_sync`'s
/// argument count clippy-clean.
pub(super) struct SidecarConfig<'a> {
    pub(super) zidx_dir: Option<&'a Path>,
    pub(super) registry: Option<&'a ArchiveRegistry>,
    /// `(dzx_dir, dzx_min_entry_bytes, dzx_max_concurrent_builds)`
    pub(super) dzx:      Option<(&'a Path, u64, usize)>,
}

/// `tag` is `Some((watch_root, generation))` when called from a full crawl
/// walk - it stamps the inserted archive-entry rows with the current
/// generation so `remove_stale_for_root` doesn't sweep them away right after
/// this same crawl inserts them. It is `None` for event-driven archive
/// (re)indexing (rename, ArchiveModified), which happens outside any crawl
/// generation.
pub(super) fn crawl_archive_sync(
    archive_path: &Path,
    crawler:      &ArchiveCrawler<'_>,
    store:        &FileStore,
    tantivy:      &TantivyStore,
    metrics:      &IndexMetrics,
    sidecars:     SidecarConfig<'_>,
    tag:          Option<(&str, i64)>,
) -> Result<()> {
    let archive_str = archive_path.to_str().unwrap_or_default();
    store.remove_archive_entries(archive_str)?;

    let mut records_batch: Vec<FileRecord>         = Vec::new();
    let mut content_batch: Vec<(FileRecord, String)> = Vec::new();

    for entry_result in crawler.crawl(archive_path, 0)? {
        let entry = match entry_result {
            Ok(e)  => e,
            Err(e) => {
                tracing::warn!("archive entry error in {}: {e}", archive_path.display());
                continue;
            }
        };
        let record = archive_entry_to_record(&entry);
        if let Some(content) = entry.content {
            let mut r = record.clone();
            r.content_indexed = true;
            content_batch.push((r, content));
        }
        records_batch.push(record);
        metrics.archive_entries_indexed.fetch_add(1, Ordering::Relaxed);
    }

    store.upsert_archive_batch(&records_batch, tag)?;

    for (record, content) in &content_batch {
        if let Err(e) = tantivy.upsert(record, Some(content)) {
            tracing::warn!("tantivy upsert error: {e}");
        }
        metrics.content_indexed.fetch_add(1, Ordering::Relaxed);
    }

    metrics.archives_crawled.fetch_add(1, Ordering::Relaxed);

    // Build .zidx sidecar when archive-service indexing is enabled.
    if let Some(zidx_dir) = sidecars.zidx_dir {
        let idx_path = crate::zidx::zidx_path_for(zidx_dir, archive_path);
        if let Err(e) = crate::zidx::ZidxBuilder::build(archive_path, &idx_path) {
            tracing::warn!("zidx build failed for {}: {e}", archive_path.display());
        } else {
            if let Some(reg) = sidecars.registry {
                reg.register(archive_path.to_path_buf());
            }
            if let Some((dzx_dir, min_entry_bytes, max_concurrent_builds)) = sidecars.dzx {
                if min_entry_bytes > 0 {
                    build_dzx_sidecars(archive_path, &idx_path, dzx_dir, min_entry_bytes, max_concurrent_builds);
                }
            }
        }
    }

    Ok(())
}
