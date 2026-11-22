use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::error::{AppError, Result};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub watch_paths: Vec<PathBuf>,
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

/// How live changes reach the index (`watcher/live/`).
#[derive(Debug, Clone, Deserialize)]
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
#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
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
#[derive(Debug, Clone, Deserialize)]
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
#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
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

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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

#[derive(Debug, Clone, Deserialize, Default)]
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

#[derive(Debug, Clone, Deserialize, Default)]
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    /// Only REST routes that change files need the token.
    #[default]
    Writes,
    /// Every REST route except `/health`, and every gRPC call, need the token.
    All,
}

#[derive(Debug, Clone, Deserialize)]
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
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize, Default)]
pub struct MacroConfig {
    #[serde(flatten)]
    pub entries: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
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
    let mut cfg: Config = toml::from_str(&text).map_err(|e| AppError::Config(e.to_string()))?;

    // Canonicalize watch roots so the crawler's on-disk paths and the fs
    // watcher's reported event paths agree byte-for-byte. On macOS, FSEvents
    // resolves symlinked components (e.g. `/var` -> `/private/var`) in every
    // event path; if watch_paths were left un-resolved, a file created right
    // after startup could race the initial crawl (which discovers it under
    // the un-resolved root) against the watcher (whose event carries the
    // resolved root), producing two distinct `path` strings - and thus two
    // rows - for the same file. Left as-is if the path doesn't exist yet;
    // `validate()` reports that separately.
    for p in &mut cfg.watch_paths {
        if let Ok(canon) = p.canonicalize() {
            *p = if cfg!(windows) {
                without_verbatim(canon)
            } else {
                canon
            };
        }
    }

    cfg.source_path = Some(path.to_path_buf());
    Ok(cfg)
}

/// `canonicalize` on Windows returns `\\?\C:\x`. Kept, that prefix made the
/// USN code open a volume named `\\.\\\?\C:`, so journal replay never ran,
/// and it ended up in every indexed path. Drive and UNC paths read the same
/// without it as long as they stay under MAX_PATH, as these roots do.
pub(crate) fn without_verbatim(p: PathBuf) -> PathBuf {
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

    if cfg.watch_paths.is_empty() {
        errors.push("watch_paths must not be empty".into());
    }
    for p in &cfg.watch_paths {
        if !p.exists() {
            errors.push(format!("watch_path does not exist: {}", p.display()));
        } else if !p.is_dir() {
            errors.push(format!("watch_path is not a directory: {}", p.display()));
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

/// Watch the config file for changes. On a valid change the shared config is
/// atomically replaced. Invalid configs are logged and the current config is
/// kept. Exits when `shutdown` is cancelled.
pub async fn watch_config(path: PathBuf, config: Arc<RwLock<Config>>, shutdown: CancellationToken) {
    use notify::{recommended_watcher, RecursiveMode, Watcher};

    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = match recommended_watcher(move |res| {
        let _ = tx.send(res);
    }) {
        Ok(w) => w,
        Err(e) => {
            tracing::error!("failed to create config watcher: {e}");
            return;
        }
    };

    if let Err(e) = watcher.watch(&path, RecursiveMode::NonRecursive) {
        tracing::error!("failed to watch config path {}: {e}", path.display());
        return;
    }

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                tracing::debug!("config watcher shutting down");
                break;
            }
            _ = tokio::time::sleep(Duration::from_millis(500)) => {
                // Drain all pending events and reload once if any modify event arrived.
                let mut should_reload = false;
                while let Ok(Ok(event)) = rx.try_recv() {
                    use notify::EventKind;
                    if matches!(event.kind, EventKind::Modify(_)) {
                        should_reload = true;
                    }
                }
                if should_reload {
                    match load(&path) {
                        Ok(new_cfg) => {
                            let errs = validate(&new_cfg);
                            if errs.is_empty() {
                                *config.write().await = new_cfg;
                                tracing::info!("config reloaded from {}", path.display());
                            } else {
                                tracing::warn!("config reload rejected - validation errors: {:?}", errs);
                            }
                        }
                        Err(e) => tracing::warn!("config reload parse error: {e}"),
                    }
                }
            }
        }
    }
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
    fn load_missing_watch_paths_field() {
        let f = write_config("data_dir = \"/tmp\"\nlog_dir = \"/tmp\"");
        assert!(matches!(load(f.path()), Err(AppError::Config(_))));
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

    #[tokio::test]
    async fn hot_reload_updates_config() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = NamedTempFile::new_in(dir.path()).unwrap();
        f.write_all(minimal_config_toml(dir.path()).as_bytes())
            .unwrap();
        f.flush().unwrap();

        let cfg = load(f.path()).unwrap();
        let shared = Arc::new(RwLock::new(cfg));
        let token = CancellationToken::new();

        let path = f.path().to_path_buf();
        let shared_clone = shared.clone();
        let token_clone = token.clone();
        tokio::spawn(async move {
            watch_config(path, shared_clone, token_clone).await;
        });

        // Give watcher time to start up.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Write an updated config with a different port.
        {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(f.path())
                .unwrap();
            write!(
                file,
                "{}\n[api]\nrest_port = 8800",
                minimal_config_toml(dir.path())
            )
            .unwrap();
        }

        // Wait for the watcher to pick up the change (poll interval is 500 ms).
        tokio::time::sleep(Duration::from_secs(1)).await;

        let port = shared.read().await.api.rest_port;
        token.cancel();
        assert_eq!(port, 8800);
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
        ] {
            let text = std::fs::read_to_string(&file).unwrap();
            let mut cfg: Config =
                toml::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
            // Their watch paths are placeholders.
            cfg.watch_paths = vec![watch.path().to_path_buf()];
            assert_eq!(validate(&cfg), Vec::<String>::new(), "{}", file.display());
        }
    }
}
