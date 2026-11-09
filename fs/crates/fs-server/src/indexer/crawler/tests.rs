use super::*;
use std::path::Path;
use tempfile::TempDir;

fn make_store(dir: &TempDir) -> Arc<FileStore> {
    use crate::store::sqlite_store::SqliteStore;
    Arc::new(FileStore::Sqlite(Arc::new(
        SqliteStore::new(&dir.path().join("test.db")).unwrap(),
    )))
}

fn make_tantivy(_dir: &TempDir) -> Arc<TantivyStore> {
    Arc::new(TantivyStore::new(Path::new(":memory:")).unwrap())
}

fn make_config(watch: &Path) -> Arc<RwLock<Config>> {
    use crate::config::*;
    Arc::new(RwLock::new(Config {
        watch_paths: vec![watch.to_path_buf()],
        data_dir: watch.join("data"),
        log_dir: watch.join("logs"),
        extensions_dir: None,
        source_path: None,
        exclusions: ExclusionConfig::default(),
        content: ContentConfig {
            extension_limits: [("txt".to_string(), 65536)].into(),
        },
        api: ApiConfig::default(),
        search: SearchConfig::default(),
        indexer: IndexerConfig::default(),
        archive_index: ArchiveIndexConfig::default(),
        macros: MacroConfig::default(),
        archive_service: ArchiveServiceConfig::default(),
        store: StoreConfig::default(),
    }))
}

#[tokio::test]
async fn full_crawl_indexes_all_files() {
    let dir = TempDir::new().unwrap();
    for i in 0..5 {
        std::fs::write(dir.path().join(format!("{i}.txt")), b"hello").unwrap();
    }
    let store = make_store(&dir);
    let tantivy = make_tantivy(&dir);
    let metrics = IndexMetrics::new();
    let config = make_config(dir.path());
    let token = CancellationToken::new();

    let registry = Arc::new(ArchiveRegistry::disabled());
    full_crawl(config, store.clone(), tantivy, metrics, registry, token)
        .await
        .unwrap();

    let total = store.total_files().unwrap();
    assert!(total >= 5, "expected ≥5 files, got {total}");
}

#[tokio::test]
async fn excluded_pattern_not_indexed() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("skip.log"), b"should be excluded").unwrap();
    std::fs::write(dir.path().join("keep.txt"), b"should be indexed").unwrap();

    use crate::config::*;
    let config = Arc::new(RwLock::new(Config {
        watch_paths: vec![dir.path().to_path_buf()],
        data_dir: dir.path().join("data"),
        log_dir: dir.path().join("logs"),
        extensions_dir: None,
        source_path: None,
        exclusions: ExclusionConfig {
            path_patterns: vec!["**/*.log".into()],
            exclude_hidden: false,
            max_content_size_bytes: 0,
            skip_network_content: false,
        },
        content: ContentConfig {
            extension_limits: Default::default(),
        },
        api: ApiConfig::default(),
        search: SearchConfig::default(),
        indexer: IndexerConfig::default(),
        archive_index: ArchiveIndexConfig::default(),
        macros: MacroConfig::default(),
        archive_service: ArchiveServiceConfig::default(),
        store: StoreConfig::default(),
    }));

    let tmpdir2 = TempDir::new().unwrap();
    let store = make_store(&tmpdir2);
    let tantivy = make_tantivy(&tmpdir2);
    let metrics = IndexMetrics::new();
    let registry = Arc::new(ArchiveRegistry::disabled());
    let token = CancellationToken::new();

    full_crawl(config, store.clone(), tantivy, metrics, registry, token)
        .await
        .unwrap();

    let skip_path = dir.path().join("skip.log").to_string_lossy().into_owned();
    assert!(
        store.get_by_path(&skip_path).unwrap().is_none(),
        "excluded file should not be indexed"
    );
}

#[tokio::test]
async fn delete_event_removes_record() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("victim.txt");
    std::fs::write(&file, b"bye").unwrap();

    let tmpdir2 = TempDir::new().unwrap();
    let store = make_store(&tmpdir2);
    let tantivy = make_tantivy(&tmpdir2);
    let metrics = IndexMetrics::new();
    let config = make_config(dir.path());

    // Insert the record manually
    let meta = std::fs::metadata(&file).unwrap();
    let record = build_file_record(&file, &meta, false, unix_now());
    store.upsert_file(&record).unwrap();

    // Now fire Delete event
    let registry = Arc::new(ArchiveRegistry::disabled());
    process_event(
        IndexEvent::Deleted(file.clone()),
        config,
        store.clone(),
        tantivy,
        metrics,
        registry,
    )
    .await;

    let path_str = file.to_string_lossy().into_owned();
    assert!(store.get_by_path(&path_str).unwrap().is_none());
}
