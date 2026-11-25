use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::{AppError, Result};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// Folders to crawl and watch. None, or an empty list, watches nothing;
    /// paths added while the daemon runs are crawled and watched then.
    /// `load` adds the paths of `[[watch]]` tables, so this is every root.
    #[serde(default)]
    pub watch_paths: Vec<PathBuf>,

    /// `[[watch]]` tables: watch paths with settings of their own.
    #[serde(default)]
    pub watch: Vec<RootConfig>,
    pub data_dir: PathBuf,
    pub log_dir: PathBuf,

    /// Folder of extension executables. Unset disables extensions.
    #[serde(default)]
    pub extensions_dir: Option<PathBuf>,

    /// File this config was read from.
    #[serde(skip)]
    pub source_path: Option<PathBuf>,

    #[serde(default)]
    pub exclusions: ExclusionConfig,

    #[serde(default)]
    pub content: ContentConfig,

    #[serde(default)]
    pub api: ApiConfig,

    #[serde(default)]
    pub search: SearchConfig,

    #[serde(default)]
    pub indexer: IndexerConfig,

    #[serde(default)]
    pub archive_index: ArchiveIndexConfig,

    #[serde(default)]
    pub macros: MacroConfig,

    #[serde(default)]
    pub archive_service: ArchiveServiceConfig,

    #[serde(default)]
    pub store: StoreConfig,

    #[serde(default)]
    pub file_lists: FileListsConfig,

    #[serde(default)]
    pub watcher: WatcherConfig,
}

/// A `[[watch]]` table: one watch path and how it is followed. Inside another
/// watch path, its `content` and `exclude` apply to that folder, and its
/// mode is ignored.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct RootConfig {
    pub path: PathBuf,
    #[serde(default)]
    pub mode: RootMode,
    /// `mode = "poll"`: seconds between scans.
    #[serde(default = "default_poll_secs")]
    pub poll_secs: u64,
    /// Full-text content for files under it, by `[content]`'s limits.
    #[serde(default = "default_true")]
    pub content: bool,
    /// Globs skipped under this path only, on top of `[exclusions]`.
    #[serde(default)]
    pub exclude: Vec<String>,
}

fn default_poll_secs() -> u64 {
    900
}

/// How a watch path's changes reach the index.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RootMode {
    /// The live watcher (`[watcher]`).
    #[default]
    Watch,
    /// A timestamp scan every `poll_secs`: for network shares, where change
    /// notifications are missing or unreliable, at the cost of a scan.
    Poll,
}

impl Config {
    /// The `[[watch]]` settings closest above `path`: its watch path's, or
    /// a folder's inside it.
    pub fn root_config(&self, path: &Path) -> Option<&RootConfig> {
        self.watch
            .iter()
            .filter(|r| path.starts_with(&r.path))
            .max_by_key(|r| r.path.as_os_str().len())
    }

    /// Whether files under `path` get full-text content.
    pub fn content_under(&self, path: &Path) -> bool {
        self.root_config(path).is_none_or(|r| r.content)
    }

    /// The watch paths that are polled, with their intervals.
    pub fn polled(&self) -> Vec<(PathBuf, u64)> {
        self.watch
            .iter()
            .filter(|r| r.mode == RootMode::Poll && self.watch_paths.contains(&r.path))
            .map(|r| (r.path.clone(), r.poll_secs))
            .collect()
    }
}

/// How live changes reach the index (`watcher/live/`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WatcherConfig {
    #[serde(default)]
    pub backend: WatcherBackend,
    /// How long changes are collected before they are applied: a path
    /// changed several times inside the window is applied once.
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
    /// Windows `native`: the change buffer of each watch root, in KiB.
    /// Network shares accept at most 64.
    #[serde(default = "default_watch_buffer_kib")]
    pub buffer_kib: u32,
}

fn default_debounce_ms() -> u64 {
    500
}

fn default_watch_buffer_kib() -> u32 {
    1024
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            backend: WatcherBackend::default(),
            debounce_ms: default_debounce_ms(),
            buffer_kib: default_watch_buffer_kib(),
        }
    }
}

/// `watcher.backend`. Every backend runs the timestamp scan when it learns
/// that changes were lost.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WatcherBackend {
    /// The `notify` crate on every platform. On Windows it loses changes
    /// in a burst without saying so (`watcher/live.rs`).
    Notify,
    /// Windows: ReadDirectoryChangesW with a large buffer that reports
    /// overflow. Elsewhere the same as `notify`.
    #[default]
    Native,
    /// Windows: the NTFS USN journal for every root on a volume whose
    /// journal can be read (administrator rights), `native` for the rest.
    /// Elsewhere the same as `notify`.
    Usn,
}

/// Everything file lists (`.efu`) whose entries are searchable alongside
/// the crawled ones: catalogues of offline drives, other machines, discs.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FileListsConfig {
    /// Absolute paths of `.efu` files. Rows under a watch path are left out,
    /// since the crawl knows those files first-hand.
    #[serde(default)]
    pub paths: Vec<PathBuf>,
    /// Seconds between checks for a changed, added or removed list.
    #[serde(default = "default_file_list_poll")]
    pub poll_secs: u64,
}

fn default_file_list_poll() -> u64 {
    30
}

impl Default for FileListsConfig {
    fn default() -> Self {
        Self {
            paths: Vec::new(),
            poll_secs: default_file_list_poll(),
        }
    }
}

/// Which backend persists the file-metadata index.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StoreMode {
    /// The memory store, unless the data dir holds only a SQLite index from
    /// before this was the default; that one stays, so an upgrade does not
    /// re-crawl.
    #[default]
    Auto,
    /// SQLite WAL database: least resident memory, slower scans (substring,
    /// `parent:`, `depth:`, `dupe:`), about 12x the disk.
    Sqlite,
    /// Packed entries with parent pointers and a shared name buffer, saved
    /// to `index.bin` on shutdown and periodically; single-writer only.
    Memory,
}

impl StoreMode {
    /// `Auto` resolved against what `data_dir` already holds.
    pub fn resolve(self, data_dir: &Path) -> StoreMode {
        match self {
            StoreMode::Auto
                if data_dir.join("index.db").exists() && !data_dir.join("index.bin").exists() =>
            {
                StoreMode::Sqlite
            }
            StoreMode::Auto => StoreMode::Memory,
            mode => mode,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StoreConfig {
    #[serde(default)]
    pub mode: StoreMode,
    /// Optional SQLite secondary indexes (sqlite mode only). Each one speeds
    /// up queries on its column and costs disk space and crawl/write time;
    /// the parent and archive indexes are always kept. Changing the list
    /// takes effect on restart: missing indexes are built, unlisted ones
    /// dropped.
    #[serde(default = "default_sqlite_indexes")]
    pub indexes: Vec<SqliteIndex>,
    /// Memory mode: seconds between saves of `index.bin` while files are
    /// changing, so a crash costs at most this much catch-up rather than a
    /// full crawl. 0 saves only on clean shutdown.
    #[serde(default = "default_snapshot_interval")]
    pub snapshot_interval_secs: u64,
}

fn default_snapshot_interval() -> u64 {
    300
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            mode: StoreMode::default(),
            indexes: default_sqlite_indexes(),
            snapshot_interval_secs: default_snapshot_interval(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SqliteIndex {
    Name,
    Extension,
    Mtime,
    Size,
    Attrib,
}

impl SqliteIndex {
    pub const ALL: [SqliteIndex; 5] = [
        Self::Name,
        Self::Extension,
        Self::Mtime,
        Self::Size,
        Self::Attrib,
    ];
}

fn default_sqlite_indexes() -> Vec<SqliteIndex> {
    SqliteIndex::ALL.to_vec()
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ExclusionConfig {
    #[serde(default)]
    pub path_patterns: Vec<String>,

    #[serde(default)]
    pub exclude_hidden: bool,

    #[serde(default)]
    pub max_content_size_bytes: u64,

    #[serde(default)]
    pub skip_network_content: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ContentConfig {
    /// Extension → max bytes. `"*"` is the fallback for unlisted extensions.
    #[serde(default)]
    pub extension_limits: HashMap<String, u64>,
}

impl ContentConfig {
    /// Every limit lowered to at most `cap` bytes; `0` leaves them as set.
    pub fn capped(&self, cap: u64) -> ContentConfig {
        if cap == 0 {
            return self.clone();
        }
        ContentConfig {
            extension_limits: self
                .extension_limits
                .iter()
                .map(|(ext, &limit)| (ext.clone(), limit.min(cap)))
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    /// Only REST routes that change files need the token.
    #[default]
    Writes,
    /// Every REST route except `/health`, and every gRPC call, need the token.
    All,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApiConfig {
    #[serde(default = "default_rest_port")]
    pub rest_port: u16,

    /// Address REST listens on. Loopback unless the operator opts in.
    #[serde(default = "default_rest_bind")]
    pub rest_bind: std::net::IpAddr,

    /// Which REST and gRPC calls need the bearer token.
    #[serde(default)]
    pub auth_mode: AuthMode,

    /// BLAKE3 hex digest of the bearer token. Unset disables the routes that
    /// change files. Only a digest is kept because `/fs/read` can serve this
    /// file back to anyone who reaches the API.
    #[serde(default, alias = "rest_file_token_hash")]
    pub auth_token_hash: Option<String>,

    #[serde(default = "default_grpc_port")]
    pub grpc_port: u16,

    /// Address gRPC listens on. Loopback unless the operator opts in.
    #[serde(default = "default_grpc_bind")]
    pub grpc_bind: std::net::IpAddr,

    #[serde(default = "default_ipc_path")]
    pub ipc_path: PathBuf,

    #[serde(default = "default_cors_origins")]
    pub rest_cors_origins: Vec<String>,

    /// Enable the REST HTTP API (default: true).
    #[serde(default = "default_true")]
    pub rest_enabled: bool,
    /// Enable the gRPC API (default: true).

    #[serde(default = "default_true")]
    pub grpc_enabled: bool,

    /// Enable the Named-Pipe / Unix-socket IPC API (default: true).
    #[serde(default = "default_true")]
    pub ipc_enabled: bool,

    /// Unix: members of this group may change a system service's watch
    /// paths over IPC, as root may. On Windows an elevated administrator
    /// may.
    #[serde(default)]
    pub admin_group: Option<String>,
}

fn default_rest_port() -> u16 {
    7700
}

fn default_rest_bind() -> std::net::IpAddr {
    std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
}

fn default_grpc_bind() -> std::net::IpAddr {
    std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
}

/// Creates a random token and the digest to store in the config.
pub fn generate_file_token() -> (String, String) {
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let hash = blake3::hash(token.as_bytes()).to_hex().to_string();
    (token, hash)
}

fn default_grpc_port() -> u16 {
    7701
}

fn default_ipc_path() -> PathBuf {
    fs_protocol::default_ipc_path()
}

// Empty on purpose: `/fs/read` needs no token, so allowing any origin would let
// any website read local files through the browser.
fn default_cors_origins() -> Vec<String> {
    Vec::new()
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            rest_port: default_rest_port(),
            rest_bind: default_rest_bind(),
            auth_mode: AuthMode::default(),
            auth_token_hash: None,
            grpc_port: default_grpc_port(),
            grpc_bind: default_grpc_bind(),
            ipc_path: default_ipc_path(),
            rest_cors_origins: default_cors_origins(),
            rest_enabled: true,
            grpc_enabled: true,
            ipc_enabled: true,
            admin_group: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SearchConfig {
    #[serde(default = "default_default_limit")]
    pub default_limit: usize,

    #[serde(default = "default_max_limit")]
    pub max_limit: usize,

    #[serde(default = "default_fuzzy_threshold")]
    pub fuzzy_threshold: f32,

    /// Maximum number of candidate rows to score during a `fuzzy:` query.
    /// Caps memory usage for broad fuzzy searches; 0 means no cap.
    #[serde(default = "default_max_fuzzy_candidates")]
    pub max_fuzzy_candidates: usize,

    /// Maximum rows fetched for a query whose filters run after the fetch
    /// (`regex:`, `child:`, `startwith:`, OR/NOT over those, ...). 0 means no cap.
    #[serde(default = "default_max_filter_candidates")]
    pub max_filter_candidates: usize,
}

fn default_default_limit() -> usize {
    100
}

fn default_max_limit() -> usize {
    10_000
}

fn default_fuzzy_threshold() -> f32 {
    0.7
}

fn default_max_fuzzy_candidates() -> usize {
    50_000
}

fn default_max_filter_candidates() -> usize {
    200_000
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            default_limit: default_default_limit(),
            max_limit: default_max_limit(),
            fuzzy_threshold: default_fuzzy_threshold(),
            max_fuzzy_candidates: default_max_fuzzy_candidates(),
            max_filter_candidates: default_max_filter_candidates(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IndexerConfig {
    #[serde(default = "default_crawl_threads")]
    pub crawl_threads: usize,

    #[serde(default = "default_threads")]
    pub content_threads: usize,

    #[serde(default = "default_commit_interval")]
    pub commit_interval_secs: u64,

    #[serde(default = "default_event_batch_size")]
    pub event_batch_size: usize,

    /// Windows: build a full crawl from the NTFS file table rather than by
    /// walking directories. Needs administrator rights; any failure falls
    /// back to the walk. See [`MftMode`].
    #[serde(default)]
    pub use_mft: MftMode,
}

/// `indexer.use_mft`: `true`, `false` or `"auto"`.
///
/// Reading the table costs about the same however much of the volume a root
/// covers (10 s for `C:` on a CI runner), while the walk costs per entry:
/// over all of `C:` the table took 33 s against 650 s, over System32 9.9 s
/// against 2.2 s. So `auto` reads it for a root that is a whole volume, and
/// for any root on a drive whose table this crawl has already read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MftMode {
    #[default]
    Auto,
    On,
    Off,
}

impl Serialize for MftMode {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            MftMode::Auto => s.serialize_str("auto"),
            MftMode::On => s.serialize_bool(true),
            MftMode::Off => s.serialize_bool(false),
        }
    }
}

impl<'de> Deserialize<'de> for MftMode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Flag(bool),
            Word(String),
        }
        match Raw::deserialize(d)? {
            Raw::Flag(true) => Ok(MftMode::On),
            Raw::Flag(false) => Ok(MftMode::Off),
            Raw::Word(w) if w.eq_ignore_ascii_case("auto") => Ok(MftMode::Auto),
            Raw::Word(w) => Err(serde::de::Error::custom(format!(
                "indexer.use_mft must be true, false or \"auto\", not {w:?}"
            ))),
        }
    }
}

/// The walk waits on the file system more than the CPU: one thread per
/// core, capped as the `ignore` crate caps its own default. Half that made
/// a walk over 83k entries 1.1 s on one thread against 0.25 s on eight.
fn default_crawl_threads() -> usize {
    std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(12)
}

/// Half the cores, at least one: on one core the old order gave 0, which
/// validation rejects.
fn default_threads() -> usize {
    (std::thread::available_parallelism().map_or(4, |n| n.get()) / 2).max(1)
}
fn default_commit_interval() -> u64 {
    5
}
fn default_event_batch_size() -> usize {
    100
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            crawl_threads: default_crawl_threads(),
            content_threads: default_threads(),
            commit_interval_secs: default_commit_interval(),
            event_batch_size: default_event_batch_size(),
            use_mft: MftMode::Auto,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ArchiveIndexConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_archive_formats")]
    pub formats: Vec<String>,
    #[serde(default)]
    pub inner_extension_limits: HashMap<String, u64>,
    #[serde(default = "default_max_entries")]
    pub max_entries_per_archive: usize,
    #[serde(default)]
    pub max_nesting_depth: u8,
    #[serde(default)]
    pub passwords: Vec<String>,
}

fn default_true() -> bool {
    true
}

fn default_archive_formats() -> Vec<String> {
    vec!["zip".into()]
}

fn default_max_entries() -> usize {
    10_000
}

impl Default for ArchiveIndexConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            formats: default_archive_formats(),
            inner_extension_limits: HashMap::new(),
            max_entries_per_archive: default_max_entries(),
            max_nesting_depth: 0,
            passwords: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct MacroConfig {
    #[serde(flatten)]
    pub entries: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ArchiveServiceConfig {
    /// Enable archive service routes and .zidx sidecar generation.
    #[serde(default)]
    pub enabled: bool,
    /// Root directory for .zidx sidecar files.
    /// Defaults to `{data_dir}/zidx` when absent.
    #[serde(default)]
    pub zidx_dir: Option<PathBuf>,
    /// Max ZidxIndex instances to hold open in the LRU cache.
    #[serde(default = "default_index_cache_size")]
    pub index_cache_size: usize,
    /// Root directory for `.dzx` sidecar files.
    /// Defaults to `{data_dir}/dzx` when absent.
    #[serde(default)]
    pub dzx_dir: Option<PathBuf>,
    /// Minimum uncompressed entry size (bytes) a deflate entry must have to
    /// get a `.dzx` sidecar built for it. `0` disables `.dzx` generation.
    #[serde(default = "default_dzx_min_entry_bytes")]
    pub dzx_min_entry_bytes: u64,
    /// Max concurrent `.dzx` builds, process-wide (throttles CPU/IO).
    #[serde(default = "default_dzx_max_concurrent_builds")]
    pub dzx_max_concurrent_builds: usize,
    /// Scheduler and multi-tenant isolation config.
    #[serde(default)]
    pub scheduler: crate::scheduler::SchedulerConfig,
}

fn default_index_cache_size() -> usize {
    256
}
fn default_dzx_min_entry_bytes() -> u64 {
    10 * 1024 * 1024
}
fn default_dzx_max_concurrent_builds() -> usize {
    2
}

// ---------------------------------------------------------------------------

/// Load config from a TOML file.
pub fn load(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path).map_err(AppError::Io)?;
    from_text(&text, path)
}

/// The config `text`, read from `path`, as `load` gives it.
pub fn from_text(text: &str, path: &Path) -> Result<Config> {
    let mut cfg: Config = toml::from_str(text).map_err(|e| AppError::Config(e.to_string()))?;

    // Canonicalize watch roots so the crawler's on-disk paths and the fs
    // watcher's reported event paths agree byte-for-byte. On macOS, FSEvents
    // resolves symlinked components (e.g. `/var` -> `/private/var`) in every
    // event path; if watch_paths were left un-resolved, a file created right
    // after startup could race the initial crawl (which discovers it under
    // the un-resolved root) against the watcher (whose event carries the
    // resolved root), producing two distinct `path` strings - and thus two
    // rows - for the same file. Left as-is if the path doesn't exist yet;
    // `validate()` reports that separately.
    let resolve = |p: &mut PathBuf| {
        if let Ok(canon) = p.canonicalize() {
            *p = if cfg!(windows) {
                without_verbatim(canon)
            } else {
                canon
            };
        }
    };
    cfg.watch_paths.iter_mut().for_each(resolve);
    cfg.watch.iter_mut().for_each(|r| resolve(&mut r.path));
    // A table inside another watch path is not one itself: its settings
    // apply to that part of it (`root_config`). Such a table is what a
    // watch path with settings becomes when a parent takes it in.
    for r in &cfg.watch {
        if !cfg.watch_paths.iter().any(|p| r.path.starts_with(p)) {
            cfg.watch_paths.push(r.path.clone());
        }
    }

    cfg.source_path = Some(path.to_path_buf());
    Ok(cfg)
}

/// `canonicalize` on Windows returns `\\?\C:\x`. Kept, that prefix made the
/// USN code open a volume named `\\.\\\?\C:`, so journal replay never ran,
/// and it ended up in every indexed path. Drive and UNC paths read the same
/// without it as long as they stay under MAX_PATH, as these roots do.
pub fn without_verbatim(p: PathBuf) -> PathBuf {
    let Some(s) = p.to_str() else { return p };
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match s.strip_prefix(r"\\?\") {
        Some(rest) if rest.len() < 260 && rest.as_bytes().get(1) == Some(&b':') => {
            PathBuf::from(rest)
        }
        _ => p,
    }
}

/// Validate config. Returns a list of error strings; empty means valid.
/// `sockaddr_un.sun_path` minus the trailing NUL (104 bytes on macOS/BSD,
/// 108 on Linux; the smaller limit is used so configs stay portable).
#[cfg(unix)]
const MAX_UNIX_SOCKET_PATH: usize = 103;

/// Each level buffers a nested ZIP in memory (up to
/// `indexer::archive::MAX_NESTED_ARCHIVE_BYTES`), so depth is capped to keep
/// worst-case memory and ZIP-bomb fan-out bounded.
const MAX_NESTING_DEPTH: u8 = 2;

pub fn validate(cfg: &Config) -> Vec<String> {
    let mut errors = Vec::new();

    if let Some(hash) = &cfg.api.auth_token_hash {
        if blake3::Hash::from_hex(hash).is_err() {
            errors.push("api.auth_token_hash must be a 64-character hex digest".into());
        }
    }
    if cfg.api.auth_mode == AuthMode::All && cfg.api.auth_token_hash.is_none() {
        errors.push("api.auth_mode = \"all\" requires api.auth_token_hash".into());
    }

    // A missing watch path is not an error: it may be a drive that is not
    // plugged in, or a share that is not mounted. It is offline until it
    // appears (`missing_watch_paths`).
    for p in &cfg.watch_paths {
        if p.exists() && !p.is_dir() {
            errors.push(format!("watch_path is not a directory: {}", p.display()));
        }
    }

    for r in &cfg.watch {
        if r.mode == RootMode::Poll && r.poll_secs == 0 {
            errors.push(format!(
                "[[watch]] {}: poll_secs must be at least 1",
                r.path.display()
            ));
        }
        for g in &r.exclude {
            if let Err(e) = globset::Glob::new(&g.replace('\\', "/")) {
                errors.push(format!(
                    "[[watch]] {}: exclude {g:?}: {e}",
                    r.path.display()
                ));
            }
        }
    }

    // A missing list is not an error: it may sit on a drive that is not
    // mounted yet, and its entries stay until it comes back.
    for p in &cfg.file_lists.paths {
        if !p.is_absolute() {
            errors.push(format!(
                "file_lists.paths must be absolute: {}",
                p.display()
            ));
        }
    }
    if cfg.file_lists.poll_secs == 0 {
        errors.push("file_lists.poll_secs must be at least 1".into());
    }

    if cfg.api.rest_enabled && cfg.api.grpc_enabled && cfg.api.rest_port == cfg.api.grpc_port {
        errors.push(format!(
            "rest_port and grpc_port must differ (both are {})",
            cfg.api.rest_port
        ));
    }

    // bind() on an over-long Unix socket path fails at runtime, and the IPC
    // server only logs it, leaving the daemon up with no CLI/TUI access.
    #[cfg(unix)]
    if cfg.api.ipc_enabled {
        let len = cfg.api.ipc_path.as_os_str().len();
        if len > MAX_UNIX_SOCKET_PATH {
            errors.push(format!(
                "ipc_path is {len} bytes; Unix socket paths must be at most {MAX_UNIX_SOCKET_PATH}: {}",
                cfg.api.ipc_path.display()
            ));
        }
    }

    if !(0.0..=1.0).contains(&cfg.search.fuzzy_threshold) {
        errors.push(format!(
            "fuzzy_threshold must be 0.0–1.0 (got {})",
            cfg.search.fuzzy_threshold
        ));
    }

    if cfg.indexer.crawl_threads == 0 {
        errors.push("crawl_threads must be > 0".into());
    }
    if cfg.indexer.content_threads == 0 {
        errors.push("content_threads must be > 0".into());
    }

    let supported_formats = ["zip"];
    for fmt in &cfg.archive_index.formats {
        if !supported_formats.contains(&fmt.as_str()) {
            errors.push(format!(
                "archive_index.formats contains unsupported format \"{}\" (supported: zip)",
                fmt
            ));
        }
    }

    if cfg.archive_index.max_nesting_depth > MAX_NESTING_DEPTH {
        errors.push(format!(
            "archive_index.max_nesting_depth={} exceeds the maximum of {MAX_NESTING_DEPTH}",
            cfg.archive_index.max_nesting_depth
        ));
    } else if cfg.archive_index.max_nesting_depth > 1 {
        tracing::warn!(
            "archive_index.max_nesting_depth={} - nested ZIP indexing can be slow",
            cfg.archive_index.max_nesting_depth
        );
    }

    errors
}

/// Watch paths that do not exist now: offline until they appear.
pub fn missing_watch_paths(cfg: &Config) -> Vec<&Path> {
    cfg.watch_paths
        .iter()
        .filter(|p| !p.exists())
        .map(PathBuf::as_path)
        .collect()
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apis_bind_to_loopback_by_default() {
        let api = ApiConfig::default();
        assert!(api.rest_bind.is_loopback());
        assert!(api.grpc_bind.is_loopback());
    }

    #[test]
    fn no_cors_origin_is_allowed_by_default() {
        assert!(ApiConfig::default().rest_cors_origins.is_empty());
    }

    #[test]
    fn generated_token_matches_its_digest() {
        let (token, hash) = generate_file_token();
        assert_eq!(token.len(), 64);
        let expected = blake3::Hash::from_hex(&hash).unwrap();
        assert_eq!(blake3::hash(token.as_bytes()), expected);
        assert_ne!(token, generate_file_token().0);
    }
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_config(content: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    fn minimal_config_toml(tmp_dir: &Path) -> String {
        // Use forward slashes: TOML quoted strings treat \ as an escape
        // character, so Windows backslash paths must be normalised.
        let dir = tmp_dir.to_string_lossy().replace('\\', "/");
        format!(
            r#"
watch_paths = ["{dir}"]
data_dir    = "{dir}/data"
log_dir     = "{dir}/logs"
"#
        )
    }

    #[test]
    fn watch_tables_join_the_watch_paths_with_their_settings() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::create_dir_all(a.join("deep")).unwrap();
        std::fs::create_dir(&b).unwrap();
        let fwd = |p: &Path| p.to_string_lossy().replace('\\', "/");
        let f = write_config(&format!(
            "watch_paths = [\"{a}\"]\ndata_dir = \"{d}\"\nlog_dir = \"{d}\"\n\n\
             [[watch]]\npath = \"{b}\"\nmode = \"poll\"\npoll_secs = 60\ncontent = false\n\
             exclude = [\"**/@eaDir/**\"]\n\n[[watch]]\npath = \"{a}\"\n",
            a = fwd(&a),
            b = fwd(&b),
            d = fwd(&dir.path().join("d")),
        ));
        let cfg = load(f.path()).unwrap();
        assert!(validate(&cfg).is_empty(), "{:?}", validate(&cfg));
        let (a, b) = (&cfg.watch_paths[0], &cfg.watch_paths[1]);
        assert_eq!(cfg.watch_paths.len(), 2, "a listed twice counts once");
        assert_eq!(cfg.polled(), [(b.clone(), 60)]);
        assert!(!cfg.content_under(&b.join("x.txt")));
        assert!(cfg.content_under(&a.join("deep").join("x.txt")));
        assert_eq!(
            cfg.root_config(&b.join("x")).unwrap().exclude,
            ["**/@eaDir/**"]
        );

        let bad = write_config(&format!(
            "data_dir = \"{d}\"\nlog_dir = \"{d}\"\n[[watch]]\npath = \"{b}\"\nmode = \"poll\"\npoll_secs = 0\n",
            b = fwd(b),
            d = fwd(&dir.path().join("d")),
        ));
        assert_eq!(validate(&load(bad.path()).unwrap()).len(), 1);

        // A table inside a watch path sets a folder's settings, and is no
        // watch path of its own.
        let nested = write_config(&format!(
            "watch_paths = [\"{a}\"]\ndata_dir = \"{d}\"\nlog_dir = \"{d}\"\n\
             [[watch]]\npath = \"{deep}\"\nmode = \"poll\"\ncontent = false\n",
            a = fwd(a),
            deep = fwd(&a.join("deep")),
            d = fwd(&dir.path().join("d")),
        ));
        let cfg = load(nested.path()).unwrap();
        assert_eq!(cfg.watch_paths.len(), 1);
        assert!(cfg.polled().is_empty());
        assert!(!cfg.content_under(&cfg.watch_paths[0].join("deep").join("x.txt")));
        assert!(cfg.content_under(&cfg.watch_paths[0].join("x.txt")));
    }

    #[test]
    fn a_missing_watch_path_is_offline_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("not-plugged-in");
        let file = dir.path().join("a-file");
        std::fs::write(&file, b"x").unwrap();
        let fwd = |p: &Path| p.to_string_lossy().replace('\\', "/");
        let f = write_config(&format!(
            "watch_paths = [\"{}\", \"{}\"]\ndata_dir = \"{}\"\nlog_dir = \"{}\"\n",
            fwd(&missing),
            fwd(&file),
            fwd(&dir.path().join("d")),
            fwd(&dir.path().join("l")),
        ));
        let cfg = load(f.path()).unwrap();
        let errors = validate(&cfg);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("not a directory"), "{errors:?}");
        assert_eq!(missing_watch_paths(&cfg), [missing.as_path()]);
    }

    #[test]
    fn load_valid_config() {
        let dir = tempfile::tempdir().unwrap();
        let f = write_config(&minimal_config_toml(dir.path()));
        let cfg = load(f.path()).unwrap();
        assert_eq!(cfg.watch_paths.len(), 1);
        assert_eq!(cfg.api.rest_port, 7700);
        assert_eq!(cfg.api.grpc_port, 7701);
        assert!((cfg.search.fuzzy_threshold - 0.7).abs() < f32::EPSILON);
    }

    #[test]
    fn use_mft_takes_a_flag_or_auto() {
        let dir = tempfile::tempdir().unwrap();
        let base = minimal_config_toml(dir.path());
        let parse = |extra: &str| load(write_config(&format!("{base}{extra}")).path());
        assert_eq!(parse("").unwrap().indexer.use_mft, MftMode::Auto);
        for (value, mode) in [
            ("true", MftMode::On),
            ("false", MftMode::Off),
            ("\"auto\"", MftMode::Auto),
            ("\"AUTO\"", MftMode::Auto),
        ] {
            let cfg = parse(&format!("[indexer]\nuse_mft = {value}\n")).unwrap();
            assert_eq!(cfg.indexer.use_mft, mode, "{value}");
        }
        assert!(parse("[indexer]\nuse_mft = \"sometimes\"\n").is_err());
    }

    #[test]
    fn store_indexes_default_to_all_and_reject_unknown_names() {
        let dir = tempfile::tempdir().unwrap();
        let base = minimal_config_toml(dir.path());
        let cfg = load(write_config(&base).path()).unwrap();
        assert_eq!(cfg.store.indexes, SqliteIndex::ALL);
        let cfg =
            load(write_config(&format!("{base}[store]\nmode = \"sqlite\"\n")).path()).unwrap();
        assert_eq!(cfg.store.indexes, SqliteIndex::ALL);
        let some = format!("{base}[store]\nindexes = [\"name\", \"mtime\"]\n");
        let cfg = load(write_config(&some).path()).unwrap();
        assert_eq!(cfg.store.indexes, [SqliteIndex::Name, SqliteIndex::Mtime]);
        let bad = format!("{base}[store]\nindexes = [\"path\"]\n");
        assert!(matches!(
            load(write_config(&bad).path()),
            Err(AppError::Config(_))
        ));
    }

    #[test]
    fn no_watch_paths_watches_nothing() {
        let f = write_config("data_dir = \"/tmp\"\nlog_dir = \"/tmp\"");
        let cfg = load(f.path()).unwrap();
        assert!(cfg.watch_paths.is_empty());
        assert!(validate(&cfg).is_empty(), "{:?}", validate(&cfg));
    }

    #[test]
    fn auth_mode_all_requires_a_token_hash() {
        let dir = tempfile::tempdir().unwrap();
        let base = minimal_config_toml(dir.path());
        let f = write_config(&format!("{base}\n[api]\nauth_mode = \"all\""));
        let errs = validate(&load(f.path()).unwrap());
        assert!(errs.iter().any(|e| e.contains("auth_token_hash")));
    }

    #[test]
    fn auth_defaults_to_writes_and_accepts_the_old_hash_key() {
        let dir = tempfile::tempdir().unwrap();
        let base = minimal_config_toml(dir.path());
        let f = write_config(&format!("{base}\n[api]\nrest_file_token_hash = \"ab\""));
        let cfg = load(f.path()).unwrap();
        assert_eq!(cfg.api.auth_mode, AuthMode::Writes);
        assert_eq!(cfg.api.auth_token_hash.as_deref(), Some("ab"));
    }

    #[test]
    fn validate_fuzzy_threshold_out_of_range() {
        let dir = tempfile::tempdir().unwrap();
        let f = write_config(&format!(
            "{}\n[search]\nfuzzy_threshold = 1.5",
            minimal_config_toml(dir.path())
        ));
        let cfg = load(f.path()).unwrap();
        let errs = validate(&cfg);
        assert!(errs.iter().any(|e| e.contains("fuzzy_threshold")));
    }

    #[cfg(unix)]
    #[test]
    fn validate_ipc_path_too_long() {
        let dir = tempfile::tempdir().unwrap();
        let long = format!("/tmp/{}.sock", "x".repeat(120));
        let f = write_config(&format!(
            "{}\n[api]\nipc_path = \"{long}\"",
            minimal_config_toml(dir.path())
        ));
        let cfg = load(f.path()).unwrap();
        assert!(validate(&cfg).iter().any(|e| e.contains("ipc_path")));
    }

    #[cfg(unix)]
    #[test]
    fn validate_ipc_path_too_long_ignored_when_ipc_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let long = format!("/tmp/{}.sock", "x".repeat(120));
        let f = write_config(&format!(
            "{}\n[api]\nipc_enabled = false\nipc_path = \"{long}\"",
            minimal_config_toml(dir.path())
        ));
        let cfg = load(f.path()).unwrap();
        assert!(!validate(&cfg).iter().any(|e| e.contains("ipc_path")));
    }

    #[test]
    fn validate_nesting_depth_above_two_is_error() {
        let dir = tempfile::tempdir().unwrap();
        for (depth, bad) in [(2, false), (3, true)] {
            let f = write_config(&format!(
                "{}\n[archive_index]\nmax_nesting_depth = {depth}",
                minimal_config_toml(dir.path())
            ));
            let cfg = load(f.path()).unwrap();
            let errs = validate(&cfg);
            assert_eq!(
                errs.iter().any(|e| e.contains("max_nesting_depth")),
                bad,
                "{depth}: {errs:?}"
            );
        }
    }

    #[test]
    fn validate_duplicate_ports() {
        let dir = tempfile::tempdir().unwrap();
        let f = write_config(&format!(
            "{}\n[api]\nrest_port = 7700\ngrpc_port = 7700",
            minimal_config_toml(dir.path())
        ));
        let cfg = load(f.path()).unwrap();
        let errs = validate(&cfg);
        assert!(errs.iter().any(|e| e.contains("rest_port")));
    }

    #[test]
    fn duplicate_ports_ignored_when_one_service_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let f = write_config(&format!(
            "{}\n[api]\nrest_port = 7700\ngrpc_port = 7700\ngrpc_enabled = false",
            minimal_config_toml(dir.path())
        ));
        let cfg = load(f.path()).unwrap();
        assert!(!cfg.api.grpc_enabled);
        let errs = validate(&cfg);
        assert!(!errs.iter().any(|e| e.contains("rest_port")));
    }

    #[test]
    fn api_services_default_to_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let f = write_config(&minimal_config_toml(dir.path()));
        let cfg = load(f.path()).unwrap();
        assert!(cfg.api.rest_enabled);
        assert!(cfg.api.grpc_enabled);
        assert!(cfg.api.ipc_enabled);
    }

    #[test]
    fn api_services_can_be_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let f = write_config(&format!(
            "{}\n[api]\nrest_enabled = false\ngrpc_enabled = false\nipc_enabled = false",
            minimal_config_toml(dir.path())
        ));
        let cfg = load(f.path()).unwrap();
        assert!(!cfg.api.rest_enabled);
        assert!(!cfg.api.grpc_enabled);
        assert!(!cfg.api.ipc_enabled);
    }

    #[test]
    fn validate_unsupported_archive_format() {
        let dir = tempfile::tempdir().unwrap();
        let f = write_config(&format!(
            "{}\n[archive_index]\nformats = [\"rar\"]",
            minimal_config_toml(dir.path())
        ));
        let cfg = load(f.path()).unwrap();
        let errs = validate(&cfg);
        assert!(errs.iter().any(|e| e.contains("rar")));
    }

    #[test]
    fn verbatim_prefix_is_dropped_from_drive_and_unc_roots() {
        let w = |s: &str| without_verbatim(PathBuf::from(s));
        assert_eq!(
            w(r"\\?\C:\Windows\System32"),
            PathBuf::from(r"C:\Windows\System32")
        );
        assert_eq!(w(r"\\?\UNC\srv\share\x"), PathBuf::from(r"\\srv\share\x"));
        assert_eq!(
            w(r"\\?\Volume{1234}\x"),
            PathBuf::from(r"\\?\Volume{1234}\x")
        );
        assert_eq!(w("/already/plain"), PathBuf::from("/already/plain"));
    }

    #[test]
    fn auto_store_keeps_an_existing_sqlite_index_and_is_memory_otherwise() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        assert_eq!(StoreMode::Auto.resolve(d), StoreMode::Memory);
        std::fs::write(d.join("index.db"), b"").unwrap();
        assert_eq!(StoreMode::Auto.resolve(d), StoreMode::Sqlite);
        assert_eq!(StoreMode::Memory.resolve(d), StoreMode::Memory);
        std::fs::write(d.join("index.bin"), b"").unwrap();
        assert_eq!(StoreMode::Auto.resolve(d), StoreMode::Memory);
        assert_eq!(StoreMode::Sqlite.resolve(d), StoreMode::Sqlite);
        let cfg: StoreConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.mode, StoreMode::Auto);
    }

    #[test]
    fn the_example_configs_parse_and_validate() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let watch = tempfile::TempDir::new().unwrap();
        for file in [
            root.join("config.toml"),
            root.join("../../config.windows.toml"),
            root.join("../../config.macos.toml"),
            root.join("../../scripts/config.default.toml"),
        ] {
            let text = std::fs::read_to_string(&file).unwrap();
            let mut cfg: Config =
                toml::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
            // Their watch paths are placeholders.
            cfg.watch_paths = vec![watch.path().to_path_buf()];
            assert_eq!(validate(&cfg), Vec::<String>::new(), "{}", file.display());
        }
    }

    /// It says it sets every field to its default; a changed default must
    /// change it too.
    #[test]
    fn the_default_config_holds_the_defaults() {
        let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/config.default.toml");
        let cfg: Config = toml::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
        let same = |file: String, default: String| assert_eq!(file, default);
        same(
            format!("{:?}", cfg.store),
            format!("{:?}", StoreConfig::default()),
        );
        same(
            format!("{:?}", cfg.exclusions),
            format!("{:?}", ExclusionConfig::default()),
        );
        same(
            format!("{:?}", cfg.content),
            format!("{:?}", ContentConfig::default()),
        );
        same(
            format!("{:?}", cfg.archive_index),
            format!("{:?}", ArchiveIndexConfig::default()),
        );
        same(
            format!("{:?}", cfg.api),
            format!("{:?}", ApiConfig::default()),
        );
        same(
            format!("{:?}", cfg.search),
            format!("{:?}", SearchConfig::default()),
        );
        same(
            format!("{:?}", cfg.indexer),
            format!("{:?}", IndexerConfig::default()),
        );
        same(
            format!("{:?}", cfg.watcher),
            format!("{:?}", WatcherConfig::default()),
        );
        same(
            format!("{:?}", cfg.file_lists),
            format!("{:?}", FileListsConfig::default()),
        );
        same(
            format!("{:?}", cfg.macros),
            format!("{:?}", MacroConfig::default()),
        );
        let service = &cfg.archive_service;
        let default = ArchiveServiceConfig::default();
        assert!(!service.enabled && service.zidx_dir.is_none() && service.dzx_dir.is_none());
        assert_eq!(service.index_cache_size, default_index_cache_size());
        assert_eq!(service.dzx_min_entry_bytes, default_dzx_min_entry_bytes());
        assert_eq!(
            service.dzx_max_concurrent_builds,
            default_dzx_max_concurrent_builds()
        );
        same(
            format!("{:?}", service.scheduler),
            format!("{:?}", default.scheduler),
        );
    }
}
