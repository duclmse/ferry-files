//! Both stores at a given size, one store and one phase per process, so each
//! process's peak memory is that phase's alone.
//!
//!   build: writes N synthetic entries through the crawl's write path (bulk
//!          load for SQLite) and saves them, as a first crawl does.
//!   open:  opens what `build` left, as a restart does, then times queries
//!          through the full query path.
//!
//! The tree is `memory_search`'s: nested folders of 40 files with
//! mixed-case names and ten extensions. Names are shorter than a real
//! volume's (C:\Windows averaged 80 B/entry in `index.bin` against 56 here).
//!
//! Build once, then run the binary directly so the OS can report its peak
//! memory (macOS `/usr/bin/time -l`, Linux `/usr/bin/time -v`):
//!   cargo bench -p fs-server --bench store_scale --no-run
//!   <binary> build memory 10000000 <dir>
//!   <binary> open  memory 10000000 <dir> [one query]
//! `<dir>` must be empty for `build`; use one per store.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::RwLock;

use fs_server::config::Config;
use fs_server::store::memory_store::MemoryStore;
use fs_server::store::sqlite_store::SqliteStore;
use fs_server::store::{tantivy_store::TantivyStore, FileRecord, FileStore};

const FILES_PER_DIR: usize = 40;
const BATCH: usize = 10_000;
const ROOT: &str = "/bench";
const RUNS: usize = 5;

const WORDS: &[&str] = &[
    "report", "Invoice", "photo", "main", "config", "README", "notes", "build", "Index", "module",
    "test", "backup", "data", "Draft", "image", "script", "summary", "client", "server", "util",
];
const EXTENSIONS: &[&str] = &[
    "rs", "txt", "pdf", "jpg", "json", "md", "html", "zip", "log", "c",
];

const QUERIES: &[&str] = &[
    "zzqx",
    "readme",
    "*.zzq",
    "*.pdf",
    "ext:zzq",
    "ext:pdf",
    "\"zzqx.txt\"",
    "path:/bench/report",
    "parent:/bench/report/report",
    "size:>999kb",
    "sort:size readme",
    "sizedupe: count:100",
    "fuzzy:reprot",
    "fuzzy:zzqx",
    "fuzzy:mian",
];

fn record(i: usize, path: String, name: String, ext: Option<&str>, is_dir: bool) -> FileRecord {
    // Spread out so sorting has real work to do, with some repeats for dupes.
    let spread = (i as u64).wrapping_mul(7919) % 1_000_003;
    FileRecord {
        id: None,
        path,
        name,
        extension: ext.map(str::to_owned),
        size_bytes: if is_dir { 0 } else { spread },
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

/// Entry `i` of the tree, given the folder the previous entries are in.
fn entry(i: usize, dir: &mut String) -> FileRecord {
    if i.is_multiple_of(FILES_PER_DIR + 1) {
        let n = i / (FILES_PER_DIR + 1);
        let name = format!("{}_{}", WORDS[n % WORDS.len()], n);
        *dir = format!(
            "{ROOT}/{}/{}/{name}",
            WORDS[n / 7 % WORDS.len()],
            WORDS[n / 149 % WORDS.len()]
        );
        return record(i, dir.clone(), name, None, true);
    }
    let ext = EXTENSIONS[i % EXTENSIONS.len()];
    let word = WORDS[i / 3 % WORDS.len()];
    let name = format!("{word}_{}_{}.{ext}", WORDS[i / 11 % WORDS.len()], i);
    record(i, format!("{dir}/{name}"), name, Some(ext), false)
}

fn open(kind: &str, dir: &Path) -> FileStore {
    match kind {
        "memory" => FileStore::Memory(Arc::new(MemoryStore::new(dir).expect("memory store"))),
        "sqlite" => FileStore::Sqlite(Arc::new(
            SqliteStore::new(&dir.join("index.db")).expect("sqlite store"),
        )),
        other => panic!("store must be memory or sqlite, not {other}"),
    }
}

/// Resident set in MiB, from `ps`; `None` where there is none.
fn rss_mib() -> Option<f64> {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    let kib: f64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    Some(kib / 1024.0)
}

fn report_rss(when: &str) {
    match rss_mib() {
        Some(m) => println!("rss {when}: {m:.0} MiB"),
        None => println!("rss {when}: unknown"),
    }
}

fn dir_size(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.metadata().ok().filter(|m| m.is_file()).map(|m| m.len()))
        .sum()
}

fn build(kind: &str, entries: usize, dir: &Path) {
    let store = open(kind, dir);
    let start = Instant::now();
    let bulk = store.begin_bulk_load().expect("bulk load");
    let generation = store.begin_crawl_generation().expect("generation");
    let mut folder = String::new();
    let mut batch = Vec::with_capacity(BATCH);
    for i in 0..entries {
        batch.push((entry(i, &mut folder), ROOT.to_owned()));
        if batch.len() == BATCH {
            store.batch_upsert_crawl(&batch, generation).expect("write");
            batch.clear();
        }
    }
    store.batch_upsert_crawl(&batch, generation).expect("write");
    let written = start.elapsed();
    if bulk {
        store.end_bulk_load().expect("rebuild indexes");
    }
    store
        .remove_stale_for_root(ROOT, generation)
        .expect("stale");
    let built = start.elapsed();
    report_rss("after build");
    let save = Instant::now();
    store.flush_to_disk(dir).expect("save");
    let saved = save.elapsed();
    println!(
        "build {kind} {entries}: write {:.1}s, indexes {:.1}s, save {:.2}s",
        written.as_secs_f64(),
        (built - written).as_secs_f64(),
        saved.as_secs_f64()
    );
    let bytes = dir_size(dir);
    println!(
        "on disk: {:.1} MiB ({:.0} B/entry)",
        bytes as f64 / 1_048_576.0,
        bytes as f64 / entries as f64
    );
}

fn config(dir: &Path) -> Arc<RwLock<Config>> {
    let dir = dir.to_string_lossy().replace('\\', "/");
    let toml = format!(
        "watch_paths = [\"{ROOT}\"]\ndata_dir = \"{dir}/d\"\nlog_dir = \"{dir}/l\"\n\n[api]\nipc_path = \"{dir}/s\"\n"
    );
    Arc::new(RwLock::new(toml::from_str(&toml).expect("valid config")))
}

fn query(kind: &str, entries: usize, dir: &Path, only: Option<&str>) {
    report_rss("before open");
    let start = Instant::now();
    let store = Arc::new(open(kind, dir));
    let opened = start.elapsed();
    let (files, dirs) = store.entry_counts().expect("counts");
    println!(
        "open {kind}: {:.2}s, {} entries",
        opened.as_secs_f64(),
        files + dirs
    );
    assert_eq!(files + dirs, entries as u64, "built with another size");
    report_rss("after open");

    let scratch = tempfile::TempDir::new().expect("tempdir");
    let tantivy = Arc::new(TantivyStore::new(&scratch.path().join("t")).expect("tantivy"));
    let cfg = config(scratch.path());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    println!(
        "{:<30} {:>9} {:>9} {:>8}",
        "query", "median ms", "max ms", "hits"
    );
    for q in QUERIES.iter().filter(|q| only.is_none_or(|o| o == **q)) {
        let mut times = Vec::with_capacity(RUNS);
        let mut hits = 0;
        for _ in 0..RUNS {
            let (s, t, c) = (Arc::clone(&store), Arc::clone(&tantivy), Arc::clone(&cfg));
            let t0 = Instant::now();
            let result = rt
                .block_on(fs_server::query::execute(q, s, t, c))
                .expect("query");
            times.push(t0.elapsed().as_secs_f64() * 1000.0);
            hits = result.hits.len();
        }
        times.sort_by(f64::total_cmp);
        println!(
            "{:<30} {:>9.1} {:>9.1} {:>8}",
            q,
            times[RUNS / 2],
            times[RUNS - 1],
            hits
        );
    }
    report_rss("after queries");
}

fn main() {
    // `cargo bench` passes `--bench`; everything else is positional.
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with("--"))
        .collect();
    let [phase, kind, entries, dir, only @ ..] = &args[..] else {
        if args.is_empty() {
            println!("store_scale: give build|open memory|sqlite <entries> <dir> [query]; skipped");
            return;
        }
        panic!("usage: store_scale build|open memory|sqlite <entries> <dir> [query]");
    };
    let entries: usize = entries
        .replace('_', "")
        .parse()
        .expect("entries is a number");
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir).expect("dir");
    match phase.as_str() {
        "build" => build(kind, entries, &dir),
        "open" => query(kind, entries, &dir, only.first().map(String::as_str)),
        other => panic!("phase must be build or open, not {other}"),
    }
}
