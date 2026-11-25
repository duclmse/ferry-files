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
        file_lists: Default::default(),
        watcher: Default::default(),
        watch: Vec::new(),
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

/// A kept folder (a watch path the crawled root took in) is not walked
/// again: its rows survive the sweep as they were, in both stores.
#[tokio::test]
async fn a_kept_folder_is_tagged_not_walked() {
    use crate::store::memory_store::MemoryStore;
    let dir = TempDir::new().unwrap();
    let root = crate::config::without_verbatim(dir.path().canonicalize().unwrap()).join("r");
    let child = root.join("child");
    std::fs::create_dir_all(&child).unwrap();
    std::fs::write(child.join("known.txt"), b"x").unwrap();
    let data = dir.path().join("mem");
    std::fs::create_dir(&data).unwrap();
    let stores = [
        make_store(&dir),
        Arc::new(FileStore::Memory(Arc::new(
            MemoryStore::new(&data).unwrap(),
        ))),
    ];
    for store in stores {
        let crawl = |scope: Scope| {
            crawl_roots(
                make_config(&root),
                scope,
                store.clone(),
                make_tantivy(&dir),
                IndexMetrics::new(),
                Arc::new(ArchiveRegistry::disabled()),
                CancellationToken::new(),
            )
        };
        crawl(Scope::default()).await.unwrap();
        std::fs::write(child.join("unseen.txt"), b"x").unwrap();
        std::fs::write(root.join("beside.txt"), b"x").unwrap();
        crawl(Scope {
            roots: Some(vec![root.clone()]),
            keep: vec![child.clone()],
        })
        .await
        .unwrap();
        let has = |p: &Path| store.get_by_path(&p.to_string_lossy()).unwrap().is_some();
        assert!(has(&child.join("known.txt")), "kept through the sweep");
        assert!(!has(&child.join("unseen.txt")), "not walked");
        assert!(has(&root.join("beside.txt")), "the rest is walked");
        std::fs::remove_file(child.join("unseen.txt")).unwrap();
        std::fs::remove_file(root.join("beside.txt")).unwrap();
    }
}

/// The daemon's own folders under a watch path stay out of the index: its
/// writes there would otherwise come back as events forever.
#[tokio::test]
async fn the_data_and_log_folders_are_not_crawled() {
    let dir = TempDir::new().unwrap();
    let root = crate::config::without_verbatim(dir.path().canonicalize().unwrap());
    std::fs::create_dir_all(root.join("data")).unwrap();
    std::fs::create_dir_all(root.join("logs")).unwrap();
    std::fs::write(root.join("data").join("index.bin"), b"x").unwrap();
    std::fs::write(root.join("logs").join("fs.log"), b"x").unwrap();
    std::fs::write(root.join("kept.txt"), b"x").unwrap();
    let store = make_store(&dir);
    let registry = Arc::new(ArchiveRegistry::disabled());
    full_crawl(
        make_config(&root),
        store.clone(),
        make_tantivy(&dir),
        IndexMetrics::new(),
        registry,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let has = |p: &Path| store.get_by_path(&p.to_string_lossy()).unwrap().is_some();
    assert!(has(&root.join("kept.txt")));
    assert!(!has(&root.join("data")));
    assert!(!has(&root.join("data").join("index.bin")));
    assert!(!has(&root.join("logs").join("fs.log")));
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
        file_lists: Default::default(),
        watcher: Default::default(),
        watch: Vec::new(),
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

fn stores(dir: &TempDir) -> Stores {
    Stores {
        sqlite: make_store(dir),
        tantivy: make_tantivy(dir),
        metrics: Arc::new(IndexMetrics::default()),
        registry: Arc::new(ArchiveRegistry::disabled()),
    }
}

#[tokio::test]
async fn a_batch_keeps_each_paths_last_event() {
    let data = TempDir::new().unwrap();
    let watch = TempDir::new().unwrap();
    let cfg = make_config(watch.path()).read().await.clone();
    let stores = stores(&data);
    let kept = watch.path().join("kept.txt");
    let gone = watch.path().join("gone.txt");
    std::fs::write(&kept, b"x").unwrap();

    // gone.txt was created and then deleted; kept.txt deleted, then back.
    std::fs::write(&gone, b"x").unwrap();
    let mut events = vec![IndexEvent::Created(gone.clone())];
    std::fs::remove_file(&gone).unwrap();
    events.extend([
        IndexEvent::Deleted(gone.clone()),
        IndexEvent::Deleted(kept.clone()),
        IndexEvent::Created(kept.clone()),
    ]);
    apply_file_events(events, &cfg, &stores);

    let path = |p: &Path| p.to_string_lossy().into_owned();
    assert!(stores.sqlite.get_by_path(&path(&kept)).unwrap().is_some());
    assert!(stores.sqlite.get_by_path(&path(&gone)).unwrap().is_none());
    assert_eq!(stores.metrics.events_processed.load(Ordering::Relaxed), 4);
}

#[tokio::test]
async fn a_deleted_folder_takes_its_subtree_and_later_events_still_apply() {
    let data = TempDir::new().unwrap();
    let watch = TempDir::new().unwrap();
    let cfg = make_config(watch.path()).read().await.clone();
    let stores = stores(&data);
    let folder = watch.path().join("f");
    std::fs::create_dir(&folder).unwrap();
    std::fs::write(folder.join("a.txt"), b"x").unwrap();
    // Unseen folder: its contents come from a walk.
    apply_file_events(vec![IndexEvent::Created(folder.clone())], &cfg, &stores);
    let child = folder.join("a.txt").to_string_lossy().into_owned();
    assert!(stores.sqlite.get_by_path(&child).unwrap().is_some());

    std::fs::remove_dir_all(&folder).unwrap();
    let other = watch.path().join("other.txt");
    std::fs::write(&other, b"x").unwrap();
    apply_file_events(
        vec![
            IndexEvent::Deleted(folder.clone()),
            IndexEvent::Created(other.clone()),
        ],
        &cfg,
        &stores,
    );
    assert!(stores.sqlite.get_by_path(&child).unwrap().is_none());
    let other = other.to_string_lossy().into_owned();
    assert!(stores.sqlite.get_by_path(&other).unwrap().is_some());
}

#[tokio::test]
async fn a_crawl_records_how_many_entries_each_root_had() {
    let data = TempDir::new().unwrap();
    let watch = TempDir::new().unwrap();
    for i in 0..3 {
        std::fs::write(watch.path().join(format!("{i}.txt")), b"x").unwrap();
    }
    let store = make_store(&data);
    let config = make_config(watch.path());
    let root = config.read().await.watch_paths[0]
        .to_string_lossy()
        .into_owned();
    assert_eq!(last_entries(&store, &root), 0);
    full_crawl(
        config,
        store.clone(),
        make_tantivy(&data),
        IndexMetrics::new(),
        Arc::new(ArchiveRegistry::disabled()),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    // The root folder and its three files.
    assert_eq!(last_entries(&store, &root), 4);
}

#[test]
fn excluded_paths_drop_out_of_watcher_events() {
    use crate::config::ExclusionConfig;
    let excl = ExclusionEngine::new(&ExclusionConfig {
        path_patterns: vec!["**/data/**".into()],
        ..Default::default()
    })
    .unwrap();
    let p = |s: &str| std::path::PathBuf::from(s);
    let events = without_excluded(
        vec![
            IndexEvent::Modified(p("/w/data/files.db-wal")),
            IndexEvent::Created(p("/w/a.txt")),
            IndexEvent::Renamed {
                from: p("/w/b.txt"),
                to: p("/w/data/b.txt"),
            },
            IndexEvent::Renamed {
                from: p("/w/data/c.txt"),
                to: p("/w/c.txt"),
            },
            IndexEvent::Deleted(p("/w/data/seg.idx")),
            IndexEvent::Created(p("/elsewhere/d.txt")),
        ],
        &excl,
        &[p("/w")],
    );
    let shown: Vec<String> = events.iter().map(|e| format!("{e:?}")).collect();
    assert_eq!(
        shown,
        [
            r#"Created("/w/a.txt")"#,
            r#"Deleted("/w/b.txt")"#,
            r#"Created("/w/c.txt")"#
        ]
    );
}

#[tokio::test]
async fn an_unseen_folder_under_an_unindexed_parent_is_not_walked() {
    let data = TempDir::new().unwrap();
    let watch = TempDir::new().unwrap();
    let cfg = make_config(watch.path()).read().await.clone();
    let stores = stores(&data);
    // An alias of an indexed folder looks like this: its parent is unknown.
    let deep = watch.path().join("unknown").join("alias");
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::write(deep.join("f.txt"), b"x").unwrap();
    apply_file_events(vec![IndexEvent::Created(deep.clone())], &cfg, &stores);
    let child = deep.join("f.txt").to_string_lossy().into_owned();
    assert!(stores.sqlite.get_by_path(&child).unwrap().is_none());
}

#[cfg(not(target_os = "macos"))]
#[test]
fn a_folder_marked_swept_is_not_swept_again_soon() {
    let parent = "/mark-swept-test/a";
    mark_swept([parent]);
    assert!(!sweep_due(parent));
    assert!(sweep_due("/mark-swept-test/b"));
}

// A live event for one entry sweeps its vanished siblings. A vanished
// directory must take its subtree with it: catch-up compares a directory's
// children with the index, so an orphaned row under a missing parent row is
// never found again.
#[tokio::test]
async fn sibling_sweep_removes_a_vanished_directory_with_its_contents() {
    let dir = TempDir::new().unwrap();
    let gone = dir.path().join("gone");
    std::fs::create_dir(&gone).unwrap();
    let child = gone.join("child.txt");
    std::fs::write(&child, b"x").unwrap();
    let fresh = dir.path().join("fresh.txt");
    std::fs::write(&fresh, b"y").unwrap();

    let tmpdir2 = TempDir::new().unwrap();
    let store = make_store(&tmpdir2);
    for p in [&gone, &child] {
        let meta = std::fs::metadata(p).unwrap();
        store
            .upsert_file(&build_file_record(p, &meta, false, unix_now()))
            .unwrap();
    }
    std::fs::remove_dir_all(&gone).unwrap();

    process_event(
        IndexEvent::Modified(fresh.clone()),
        make_config(dir.path()),
        store.clone(),
        make_tantivy(&tmpdir2),
        IndexMetrics::new(),
        Arc::new(ArchiveRegistry::disabled()),
    )
    .await;

    assert!(store
        .get_by_path(&fresh.to_string_lossy())
        .unwrap()
        .is_some());
    assert!(store
        .get_by_path(&gone.to_string_lossy())
        .unwrap()
        .is_none());
    assert!(
        store
            .get_by_path(&child.to_string_lossy())
            .unwrap()
            .is_none(),
        "the vanished directory's child was left behind"
    );
}
