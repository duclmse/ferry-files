//! Query latency of the memory store through the full query path.
//!
//! The index is synthetic but shaped like a real tree: nested directories,
//! mixed-case names and a spread of extensions. Queries with no match are the
//! worst case, since nothing stops the scan early.
//!
//! Run with:
//!   cargo bench -p fs-server --bench memory_search

use std::sync::Arc;

use criterion::{criterion_group, criterion_main, Criterion};
use tokio::runtime::Runtime;
use tokio::sync::RwLock;

use fs_server::api::archive::ArchiveRegistry;
use fs_server::api::rest::{self, AppState};
use fs_server::config::Config;
use fs_server::indexer::crawler::IndexMetrics;
use fs_server::scheduler::{ArchiveSemaphores, ClientRateLimiter, SchedulerMetrics};
use fs_server::store::memory_store::MemoryStore;
use fs_server::store::{tantivy_store::TantivyStore, FileRecord, FileStore};

const ENTRIES: usize = 2_000_000;
const FILES_PER_DIR: usize = 40;

const WORDS: &[&str] = &[
    "report", "Invoice", "photo", "main", "config", "README", "notes", "build", "Index", "module",
    "test", "backup", "data", "Draft", "image", "script", "summary", "client", "server", "util",
];
const EXTENSIONS: &[&str] = &[
    "rs", "txt", "pdf", "jpg", "json", "md", "html", "zip", "log", "c",
];

fn record(
    i: usize,
    path: String,
    name: String,
    extension: Option<String>,
    is_dir: bool,
) -> FileRecord {
    // Spread out so sorting has real work to do, with some repeats for dupes.
    let spread = (i as u64).wrapping_mul(7919) % 1_000_003;
    FileRecord {
        id: None,
        path,
        name,
        extension,
        size_bytes: spread,
        mtime: 1_700_000_000 + spread as i64,
        ctime: 1_700_000_000,
        is_dir,
        is_hidden: false,
        is_symlink: false,
        attrib_bitmask: 0,
        content_indexed: false,
        tantivy_id: None,
        indexed_at: 1_700_000_000,
        archive_path: None,
    }
}

fn populate(store: &MemoryStore) {
    let mut dir = String::new();
    for i in 0..ENTRIES {
        if i % (FILES_PER_DIR + 1) == 0 {
            let n = i / (FILES_PER_DIR + 1);
            let name = format!("{}_{}", WORDS[n % WORDS.len()], n);
            dir = format!(
                "/bench/{}/{}/{name}",
                WORDS[n / 7 % WORDS.len()],
                WORDS[n / 149 % WORDS.len()]
            );
            store.upsert_file(&record(i, dir.clone(), name, None, true));
            continue;
        }
        let ext = EXTENSIONS[i % EXTENSIONS.len()];
        let word = WORDS[i / 3 % WORDS.len()];
        let name = format!("{word}_{}_{}.{ext}", WORDS[i / 11 % WORDS.len()], i);
        store.upsert_file(&record(
            i,
            format!("{dir}/{name}"),
            name,
            Some(ext.to_owned()),
            false,
        ));
    }
}

fn config(dir: &std::path::Path) -> Arc<RwLock<Config>> {
    let dir = dir.to_string_lossy().replace('\\', "/");
    let toml =
        format!("watch_paths = [\"{dir}\"]\ndata_dir = \"{dir}/d\"\nlog_dir = \"{dir}/l\"\n\n[api]\nipc_path = \"{dir}/s\"\n");
    Arc::new(RwLock::new(toml::from_str(&toml).expect("valid config")))
}

fn bench_queries(c: &mut Criterion) {
    let rt: Runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let memory = MemoryStore::new(tmp.path()).expect("memory store");
    populate(&memory);
    let memory_handle = Arc::new(memory);
    let store = Arc::new(FileStore::Memory(Arc::clone(&memory_handle)));
    let tantivy = Arc::new(TantivyStore::new(&tmp.path().join("t")).expect("tantivy"));
    let cfg = config(tmp.path());

    let mut group = c.benchmark_group("memory_search_2m");
    group.sample_size(15);
    for (label, query) in [
        ("contains_no_match", "zzqx"),
        ("contains_common", "readme"),
        ("suffix_no_match", "*.zzq"),
        ("suffix_common", "*.pdf"),
        ("two_wildcards_no_match", "*report*zzq*"),
        ("whole_filename_no_match", "\"zzqx.txt\""),
        ("path_prefix_no_match", "path:/zzqx"),
        ("ext_no_match", "ext:zzq"),
        ("path_contains_no_match", "path: zzqx"),
        ("path_prefix_common", "path:/bench/report"),
        ("sort_name_common", "sort:name readme"),
        ("sort_size_common", "sort:size readme"),
        ("sort_size_all", "sort:size"),
        ("sort_date_no_match", "sort:dm zzqx"),
        ("dupe_size", "sizedupe: count:100"),
        // Every path shares the prefix, as every path on C: shares C:\.
        ("parent_shared_prefix", "parent:/bench"),
        ("parent_one_folder", "parent:/bench/report/report"),
        ("depth_no_match", "depth:99"),
    ] {
        group.bench_function(label, |b| {
            b.to_async(&rt).iter(|| {
                let (s, t, f) = (Arc::clone(&store), Arc::clone(&tantivy), Arc::clone(&cfg));
                async move {
                    fs_server::query::execute(query, s, t, f)
                        .await
                        .expect("query")
                }
            })
        });
    }
    group.finish();

    bench_api(c, &rt, &store, &tantivy, &cfg);
    bench_persistence(c, &memory_handle, tmp.path());
}

/// The same query through the engine and through each real transport, so the
/// difference is what the transport and its serialization add.
fn bench_api(
    c: &mut Criterion,
    rt: &Runtime,
    store: &Arc<FileStore>,
    tantivy: &Arc<TantivyStore>,
    cfg: &Arc<RwLock<Config>>,
) {
    let state = AppState {
        sqlite: Arc::clone(store),
        tantivy: Arc::clone(tantivy),
        config: Arc::clone(cfg),
        metrics: IndexMetrics::new(),
        reindex_tx: tokio::sync::watch::channel(false).0,
        archive_registry: Arc::new(ArchiveRegistry::disabled()),
        archive_semaphores: ArchiveSemaphores::new(4),
        scheduler_metrics: SchedulerMetrics::new(),
        rate_limiter: Arc::new(ClientRateLimiter::new(1_000_000, 1_000_000)),
    };
    let addr = rt.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            axum::serve(listener, rest::router(state))
                .await
                .expect("serve");
        });
        addr
    });
    let http = reqwest::Client::new();

    let mut group = c.benchmark_group("transport_2m");
    group.sample_size(15);
    for (label, query) in [("no_match", "zzqx"), ("100_hits", "readme")] {
        group.bench_function(format!("engine_{label}"), |b| {
            b.to_async(rt).iter(|| {
                let (s, t, f) = (Arc::clone(store), Arc::clone(tantivy), Arc::clone(cfg));
                let q = format!("count:100 {query}");
                async move { fs_server::query::execute(&q, s, t, f).await.expect("query") }
            })
        });
        let url = format!("http://{addr}/search?limit=100&q={query}");
        group.bench_function(format!("rest_{label}"), |b| {
            b.to_async(rt).iter(|| {
                let (http, url) = (http.clone(), url.clone());
                async move {
                    let response = http.get(url).send().await.expect("request");
                    assert!(response.status().is_success());
                    response.bytes().await.expect("body")
                }
            })
        });
    }
    bench_ipc(&mut group, rt, store, tantivy, cfg);
    group.finish();
}

#[cfg(unix)]
fn bench_ipc(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    rt: &Runtime,
    store: &Arc<FileStore>,
    tantivy: &Arc<TantivyStore>,
    cfg: &Arc<RwLock<Config>>,
) {
    use fs_protocol::{ipc_client::IpcClient, ipc_protocol::IpcRequest};
    use tokio::sync::Mutex;

    let socket = rt.block_on(async { cfg.read().await.api.ipc_path.clone() });
    let client = rt.block_on(async {
        tokio::spawn(fs_server::api::ipc_server::run_ipc_server(
            Arc::clone(cfg),
            Arc::clone(store),
            Arc::clone(tantivy),
            IndexMetrics::new(),
            tokio::sync::watch::channel(false).0,
            Arc::new(Default::default()),
            false,
            tokio_util::sync::CancellationToken::new(),
        ));
        for _ in 0..200 {
            if let Ok(client) = IpcClient::connect(std::path::Path::new(&socket)).await {
                return Arc::new(Mutex::new(client));
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("IPC server did not start");
    });
    for (label, query) in [("no_match", "zzqx"), ("100_hits", "readme")] {
        group.bench_function(format!("ipc_{label}"), |b| {
            b.to_async(rt).iter(|| {
                let client = Arc::clone(&client);
                async move {
                    client
                        .lock()
                        .await
                        .request(IpcRequest::Search {
                            query: query.to_owned(),
                            limit: 100,
                            offset: 0,
                        })
                        .await
                        .expect("request")
                }
            })
        });
    }
}

#[cfg(not(unix))]
fn bench_ipc(
    _: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    _: &Runtime,
    _: &Arc<FileStore>,
    _: &Arc<TantivyStore>,
    _: &Arc<RwLock<Config>>,
) {
}

fn bench_persistence(c: &mut Criterion, memory: &MemoryStore, dir: &std::path::Path) {
    let mut group = c.benchmark_group("persistence_2m");
    group.sample_size(10);
    group.bench_function("save", |b| {
        b.iter(|| memory.flush_to_disk(dir).expect("save"))
    });
    group.bench_function("load", |b| b.iter(|| MemoryStore::new(dir).expect("load")));
    group.finish();

    let bytes = std::fs::metadata(dir.join("index.bin"))
        .expect("index.bin")
        .len();
    eprintln!(
        "index.bin: {} bytes ({:.1} MiB, {:.1} B/entry) for {ENTRIES} entries",
        bytes,
        bytes as f64 / (1024.0 * 1024.0),
        bytes as f64 / ENTRIES as f64
    );
}

criterion_group!(benches, bench_queries);
criterion_main!(benches);
