use std::path::Path;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// `.dzx` build throttle - process-wide, sized from the first archive crawled.
// `dzx_max_concurrent_builds` is read once, so a config change applies at restart.
// ---------------------------------------------------------------------------

fn dzx_build_semaphore(permits: usize) -> Arc<tokio::sync::Semaphore> {
    static SEM: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    Arc::clone(SEM.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(permits.max(1)))))
}

/// Build `.dzx` sidecars for every deflate entry in `zidx_path` whose
/// uncompressed size is at least `min_entry_bytes`. Runs after the `.zidx`
/// build so entry metadata (offsets, sizes) is already on disk.
pub(super) fn build_dzx_sidecars(
    archive_path: &Path,
    zidx_path: &Path,
    dzx_dir: &Path,
    min_entry_bytes: u64,
    max_concurrent_builds: usize,
) {
    let idx = match crate::zidx::ZidxIndex::open(zidx_path) {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(
                "dzx: reopen .zidx failed for {}: {e}",
                archive_path.display()
            );
            return;
        }
    };
    let source = match crate::archive::LocalFileSource::new(archive_path.to_path_buf()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                "dzx: open archive failed for {}: {e}",
                archive_path.display()
            );
            return;
        }
    };
    if let Err(e) = std::fs::create_dir_all(dzx_dir) {
        tracing::warn!("dzx: create_dir_all {} failed: {e}", dzx_dir.display());
        return;
    }

    let sema = dzx_build_semaphore(max_concurrent_builds);
    let handle = tokio::runtime::Handle::current();
    let builder = crate::archive::DzxBuilder::new(crate::archive::DEFAULT_BLOCK_SIZE);

    for id in 0..idx.node_count {
        let Ok(node) = idx.node(id) else { continue };
        if node.kind != 0 || node.compression != 8 || node.uncompressed_size < min_entry_bytes {
            continue;
        }
        let out_path =
            crate::archive::dzx_path_for(dzx_dir, archive_path, node.local_header_offset);
        if out_path.exists() {
            continue;
        }
        let meta = crate::archive::EntryMeta {
            compression: node.compression,
            compressed_size: node.compressed_size,
            uncompressed_size: node.uncompressed_size,
            local_header_offset: node.local_header_offset,
        };
        let Ok(_permit) = handle.block_on(sema.acquire()) else {
            continue;
        };
        if let Err(e) = handle.block_on(builder.build(&source, &meta, &out_path)) {
            tracing::warn!(
                "dzx build failed for {} node {id}: {e}",
                archive_path.display()
            );
        }
    }
}
