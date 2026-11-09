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

use fs_server::config::Config;
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

fn record(path: String, name: String, extension: Option<String>, is_dir: bool) -> FileRecord {
    FileRecord {
        id: None,
        path,
        name,
        extension,
        size_bytes: 4096,
        mtime: 1_700_000_000,
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
            store.upsert_file(&record(dir.clone(), name, None, true));
            continue;
        }
        let ext = EXTENSIONS[i % EXTENSIONS.len()];
        let word = WORDS[i / 3 % WORDS.len()];
        let name = format!("{word}_{}_{}.{ext}", WORDS[i / 11 % WORDS.len()], i);
        store.upsert_file(&record(
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
        format!("watch_paths = [\"{dir}\"]\ndata_dir = \"{dir}/d\"\nlog_dir = \"{dir}/l\"\n");
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
    let store = Arc::new(FileStore::Memory(Arc::new(memory)));
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
}

criterion_group!(benches, bench_queries);
criterion_main!(benches);
