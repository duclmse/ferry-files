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
//!   <binary> build memory 10000000 <dir> [names.txt]
//!   <binary> open  memory 10000000 <dir> [one query]
//! `<dir>` must be empty for `build`; use one per store. With `names.txt`,
//! files take real names from that list in turn instead of synthetic ones.
//!
//! `trigram synth <entries> | file <names.txt>  [block] [bits/key] [terms..]`
//! prototypes per-block trigram Bloom filters over names; see [`trigram`].

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
    "qwertyuiop",
    "mian",
    "invoice.pdf",
    "libsystem",
    "main.rs",
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

/// File names read from a list (one per line, as `find | awk -F/ '{print
/// $NF}'` gives), placed in `entry`'s folders. A name already in its folder
/// gets a `~i` suffix, so the entry count stays what was asked for.
struct RealNames {
    names: Vec<String>,
    in_dir: std::collections::HashSet<String>,
}

impl RealNames {
    fn load(path: &str) -> Self {
        let text = std::fs::read_to_string(path).expect("names file");
        let names: Vec<String> = text
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect();
        assert!(!names.is_empty(), "no names in {path}");
        Self {
            names,
            in_dir: Default::default(),
        }
    }

    fn entry(&mut self, i: usize, dir: &mut String) -> FileRecord {
        let synthetic = entry(i, dir);
        if synthetic.is_dir {
            self.in_dir.clear();
            return synthetic;
        }
        let mut name = self.names[i % self.names.len()].clone();
        if !self.in_dir.insert(name.clone()) {
            name = format!("{name}~{i}");
        }
        let ext = name
            .rsplit_once('.')
            .map(|(_, x)| x)
            .filter(|x| !x.is_empty());
        record(i, format!("{dir}/{name}"), name.clone(), ext, false)
    }
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

fn build(kind: &str, entries: usize, dir: &Path, names: Option<&str>) {
    let mut real = names.map(RealNames::load);
    let store = open(kind, dir);
    let start = Instant::now();
    let bulk = store.begin_bulk_load().expect("bulk load");
    let generation = store.begin_crawl_generation().expect("generation");
    let mut folder = String::new();
    let mut batch = Vec::with_capacity(BATCH);
    for i in 0..entries {
        let e = match &mut real {
            Some(r) => r.entry(i, &mut folder),
            None => entry(i, &mut folder),
        };
        batch.push((e, ROOT.to_owned()));
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
    if std::env::var_os("STORE_SCALE_WARM").is_some() {
        let t = Instant::now();
        store.warm_search();
        println!("warm: {:.2}s", t.elapsed().as_secs_f64());
    }

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
    if let Some(summary) = store.heap_summary() {
        println!("heap: {summary}");
    }
}

fn main() {
    // `cargo bench` passes `--bench`; everything else is positional.
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with("--"))
        .collect();
    if args.first().is_some_and(|a| a == "trigram") {
        trigram::run(&args[1..]);
        return;
    }
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
        "build" => build(kind, entries, &dir, only.first().map(String::as_str)),
        "open" => query(kind, entries, &dir, only.first().map(String::as_str)),
        other => panic!("phase must be build or open, not {other}"),
    }
}

/// Prototype of a block skip index: one Bloom filter per block of names,
/// holding the block's lowercased 3-byte windows. A substring search tests
/// the term's trigrams first and scans only blocks holding all of them.
///
/// Names sit in one arena, so the baseline scan is cheaper per entry than
/// the memory store's; compare the two columns here with each other, not
/// with the store's own times.
mod trigram {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    use rayon::prelude::*;

    struct Names {
        bytes: Vec<u8>,
        ends: Vec<usize>,
    }

    impl Names {
        fn len(&self) -> usize {
            self.ends.len()
        }
        fn get(&self, i: usize) -> &[u8] {
            let start = if i == 0 { 0 } else { self.ends[i - 1] };
            &self.bytes[start..self.ends[i]]
        }
        fn push(&mut self, name: &[u8]) {
            self.bytes.extend_from_slice(name);
            self.ends.push(self.bytes.len());
        }
    }

    struct Bloom {
        bits: Vec<u64>,
        nbits: u64,
        k: u32,
    }

    fn mix(mut x: u64) -> u64 {
        x ^= x >> 30;
        x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x ^= x >> 27;
        x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
        x ^ (x >> 31)
    }

    impl Bloom {
        fn new(keys: &[u32], bits_per_key: usize) -> Self {
            let nbits = (keys.len() * bits_per_key).max(64) as u64;
            let k = ((bits_per_key as f64 * std::f64::consts::LN_2).round() as u32).max(1);
            let mut bloom = Bloom {
                bits: vec![0; nbits.div_ceil(64) as usize],
                nbits,
                k,
            };
            for &key in keys {
                bloom.probes(key, |b, i| b.bits[i / 64] |= 1 << (i % 64));
            }
            bloom
        }
        fn probes(&mut self, key: u32, mut f: impl FnMut(&mut Self, usize)) {
            let h = mix(u64::from(key));
            let (h1, h2) = (h, mix(h ^ 0x9e37_79b9_7f4a_7c15) | 1);
            for i in 0..u64::from(self.k) {
                let x = h1.wrapping_add(i.wrapping_mul(h2));
                let bit = ((u128::from(x) * u128::from(self.nbits)) >> 64) as usize;
                f(self, bit);
            }
        }
        fn has(&self, key: u32) -> bool {
            let h = mix(u64::from(key));
            let (h1, h2) = (h, mix(h ^ 0x9e37_79b9_7f4a_7c15) | 1);
            (0..u64::from(self.k)).all(|i| {
                let x = h1.wrapping_add(i.wrapping_mul(h2));
                let bit = ((u128::from(x) * u128::from(self.nbits)) >> 64) as usize;
                self.bits[bit / 64] & (1 << (bit % 64)) != 0
            })
        }
        fn bytes(&self) -> usize {
            self.bits.len() * 8
        }
    }

    fn trigrams(name: &[u8], out: &mut Vec<u32>) {
        for w in name.windows(3) {
            let [a, b, c] = [w[0], w[1], w[2]].map(|x| u32::from(x.to_ascii_lowercase()));
            out.push(a | b << 8 | c << 16);
        }
    }

    /// The store's matcher, copied: ASCII case folding only.
    fn contains_ignore_case(text: &[u8], needle: &[u8]) -> bool {
        let Some(last) = text.len().checked_sub(needle.len()) else {
            return false;
        };
        let Some(&first) = needle.first() else {
            return true;
        };
        let (lower, upper) = (first.to_ascii_lowercase(), first.to_ascii_uppercase());
        memchr::memchr2_iter(lower, upper, &text[..=last])
            .any(|i| text[i..i + needle.len()].eq_ignore_ascii_case(needle))
    }

    /// The store's `scan_in_blocks` round structure: one block, then twice
    /// as many per round up to four per thread, stopping at `limit` hits.
    fn scan(total_blocks: usize, limit: usize, block: impl Fn(usize) -> usize + Sync) -> usize {
        let max_blocks = rayon::current_num_threads() * 4;
        let (mut hits, mut next, mut round) = (0, 0, 1);
        while next < total_blocks && hits < limit {
            let end = (next + round).min(total_blocks);
            hits += (next..end).into_par_iter().map(&block).sum::<usize>();
            next = end;
            round = (round * 2).min(max_blocks);
        }
        hits.min(limit)
    }

    fn synth(entries: usize) -> Names {
        let mut names = Names {
            bytes: Vec::new(),
            ends: Vec::with_capacity(entries),
        };
        let mut dir = String::new();
        for i in 0..entries {
            names.push(super::entry(i, &mut dir).name.as_bytes());
        }
        names
    }

    fn from_file(path: &str) -> Names {
        let text = std::fs::read(path).expect("names file");
        let mut names = Names {
            bytes: Vec::with_capacity(text.len()),
            ends: Vec::new(),
        };
        for line in text.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
            names.push(line);
        }
        names
    }

    fn median(mut t: Vec<f64>) -> f64 {
        t.sort_by(f64::total_cmp);
        t[t.len() / 2]
    }

    pub fn run(args: &[String]) {
        let (names, label) = match args {
            [kind, n, ..] if kind == "synth" => {
                let n: usize = n.replace('_', "").parse().expect("entries");
                (synth(n), format!("synthetic {n}"))
            }
            [kind, path, ..] if kind == "file" => (from_file(path), path.clone()),
            _ => {
                panic!("usage: trigram synth <entries> | file <names.txt> [block] [bits] [terms..]")
            }
        };
        let block: usize = args.get(2).map_or(1 << 15, |b| b.parse().expect("block"));
        let bits: usize = args.get(3).map_or(10, |b| b.parse().expect("bits"));
        let total = names.len();
        let blocks = total.div_ceil(block);
        let name_bytes = names.bytes.len();
        println!(
            "{label}: {total} names, {:.1} B/name, {blocks} blocks of {block}, {bits} bits/key",
            name_bytes as f64 / total as f64
        );

        let t0 = Instant::now();
        let filters: Vec<(Bloom, usize)> = (0..blocks)
            .into_par_iter()
            .map(|b| {
                let mut keys = Vec::new();
                for i in b * block..((b + 1) * block).min(total) {
                    trigrams(names.get(i), &mut keys);
                }
                keys.sort_unstable();
                keys.dedup();
                (Bloom::new(&keys, bits), keys.len())
            })
            .collect();
        let built = t0.elapsed();
        let filter_bytes: usize = filters.iter().map(|(f, _)| f.bytes()).sum();
        let distinct: Vec<usize> = filters.iter().map(|&(_, d)| d).collect();
        println!(
            "build {:.0} ms; filters {:.1} MiB ({:.1} B/name, {:.0}% of name bytes); distinct trigrams/block avg {} max {}",
            built.as_secs_f64() * 1000.0,
            filter_bytes as f64 / 1_048_576.0,
            filter_bytes as f64 / total as f64,
            100.0 * filter_bytes as f64 / name_bytes as f64,
            distinct.iter().sum::<usize>() / blocks,
            distinct.iter().max().copied().unwrap_or(0),
        );

        // False positives: random keys, checked against the exact sets.
        let mut fp = (0usize, 0usize);
        let mut x = 0x2545_f491_4f6c_dd1du64;
        for b in (0..blocks).step_by((blocks / 50).max(1)) {
            let mut keys = Vec::new();
            for i in b * block..((b + 1) * block).min(total) {
                trigrams(names.get(i), &mut keys);
            }
            keys.sort_unstable();
            keys.dedup();
            for _ in 0..20_000 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let key = (x as u32) & 0x00ff_ffff;
                if keys.binary_search(&key).is_err() {
                    fp.1 += 1;
                    fp.0 += usize::from(filters[b].0.has(key));
                }
            }
        }
        println!(
            "false positives per absent trigram: {:.2}%",
            100.0 * fp.0 as f64 / fp.1 as f64
        );

        let mut terms: Vec<String> = ["zzqx", "zzqx.txt", "qwertyuiop", "readme", "mian", "report"]
            .map(str::to_owned)
            .to_vec();
        // Names from the list itself, so rare present terms come from the data.
        for at in [total / 3, total * 2 / 3] {
            terms.push(String::from_utf8_lossy(names.get(at)).into_owned());
        }
        terms.extend(args.iter().skip(4).cloned());

        println!(
            "{:<34} {:>7} {:>10} {:>10} {:>13}",
            "term (limit 100)", "hits", "scan ms", "skip ms", "blocks read"
        );
        for term in &terms {
            let needle = term.as_bytes();
            let mut wanted = Vec::new();
            trigrams(needle, &mut wanted);
            wanted.sort_unstable();
            wanted.dedup();
            let block_scan = |b: usize| {
                (b * block..((b + 1) * block).min(total))
                    .filter(|&i| contains_ignore_case(names.get(i), needle))
                    .take(100)
                    .count()
            };
            let (mut plain, mut skip, mut hits) = (Vec::new(), Vec::new(), (0, 0));
            let read = AtomicUsize::new(0);
            for _ in 0..7 {
                let t = Instant::now();
                hits.0 = scan(blocks, 100, block_scan);
                plain.push(t.elapsed().as_secs_f64() * 1000.0);
                read.store(0, Ordering::Relaxed);
                let t = Instant::now();
                hits.1 = scan(blocks, 100, |b| {
                    if wanted.iter().all(|&k| filters[b].0.has(k)) {
                        read.fetch_add(1, Ordering::Relaxed);
                        block_scan(b)
                    } else {
                        0
                    }
                });
                skip.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            assert_eq!(hits.0, hits.1, "skipping lost hits for {term}");
            let shown: String = term.chars().take(34).collect();
            println!(
                "{:<34} {:>7} {:>10.2} {:>10.2} {:>6}/{:<6}",
                shown,
                hits.0,
                median(plain),
                median(skip),
                read.load(Ordering::Relaxed),
                blocks
            );
        }
    }
}
