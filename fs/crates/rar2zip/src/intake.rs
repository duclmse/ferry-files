use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

use crate::config::Config;
use crate::error::{AppError, Result};
use crate::queue::{JobQueue, Strategy};

// ---------------------------------------------------------------------------
// Multi-part classification
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum MultipartClass {
    /// `archive.partN.rar` — new-style volume N (1-based)
    NewStylePart { base: String, part_num: u32 },
    /// `archive.rar` — old-style main (always first/only old-style file)
    OldStyleMain { base: String },
    /// `archive.rNN` — old-style continuation part (0-based NN, so part_num = NN + 1)
    #[allow(dead_code)]
    OldStylePart { base: String, part_num: u32 },
    /// Single archive — not part of any multi-part set
    Single,
}

fn classify(filename: &str) -> MultipartClass {
    let lower = filename.to_ascii_lowercase();

    // New style: ends with ".rar" and the stem ends with ".partN"
    if let Some(stem) = lower.strip_suffix(".rar") {
        if let Some(dot_part) = stem.rfind(".part") {
            let num_str = &stem[dot_part + 5..];
            if !num_str.is_empty() && num_str.bytes().all(|b| b.is_ascii_digit()) {
                if let Ok(n) = num_str.parse::<u32>() {
                    // Keep original-case base (everything in real filename before .partN.rar)
                    let base = filename[..dot_part].to_string();
                    return MultipartClass::NewStylePart { base, part_num: n };
                }
            }
        }
        // Old-style main
        let base = filename[..filename.len() - 4].to_string();
        return MultipartClass::OldStyleMain { base };
    }

    // Old style continuation: ends with ".rNN" where NN is exactly 2 digits
    let bytes = filename.as_bytes();
    let len = bytes.len();
    if len >= 5
        && bytes[len - 4] == b'.'
        && bytes[len - 3].eq_ignore_ascii_case(&b'r')
        && bytes[len - 2].is_ascii_digit()
        && bytes[len - 1].is_ascii_digit()
    {
        let nn = (bytes[len - 2] - b'0') as u32 * 10 + (bytes[len - 1] - b'0') as u32;
        let base = filename[..len - 4].to_string();
        return MultipartClass::OldStylePart { base, part_num: nn + 1 };
    }

    MultipartClass::Single
}

/// Check if all new-style parts 1..=max_part are on disk and part max_part+1 is NOT.
fn new_style_complete(dir: &Path, base: &str, max_part: u32) -> bool {
    // All parts 1..=max_part must exist
    for n in 1..=max_part {
        let name = format!("{base}.part{n}.rar");
        if !dir.join(&name).exists() {
            return false;
        }
    }
    // The next part must NOT exist
    let next = format!("{base}.part{}.rar", max_part + 1);
    !dir.join(&next).exists()
}

/// Check if the old-style set is complete:
/// - `<base>.rar` must exist
/// - `<base>.r00` .. `<base>.rNN` must all exist sequentially
/// - `<base>.r(NN+1)` must NOT exist
fn old_style_complete(dir: &Path, base: &str) -> bool {
    if !dir.join(format!("{base}.rar")).exists() {
        return false;
    }
    let mut nn: u32 = 0;
    loop {
        let part = format!("{base}.r{:02}", nn);
        if dir.join(&part).exists() {
            nn += 1;
        } else {
            // If nn == 0 and r00 doesn't exist, the set is just the .rar (single)
            break;
        }
    }
    // Ensure there's no unexpected next part
    let next = format!("{base}.r{:02}", nn);
    !dir.join(&next).exists()
}

// ---------------------------------------------------------------------------
// Hashing
// ---------------------------------------------------------------------------

/// Stream-read a file with a 64 KB buffer and return its SHA-256 hex digest.
pub async fn hash_file(path: &Path) -> Result<String> {
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path).await.map_err(AppError::Io)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 65536];
    loop {
        let n = file.read(&mut buf).await.map_err(AppError::Io)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

// ---------------------------------------------------------------------------
// Strategy selection
// ---------------------------------------------------------------------------

pub fn select_strategy(file_size_bytes: u64, threshold_mb: u64) -> Strategy {
    if threshold_mb > 0 && file_size_bytes > threshold_mb * 1024 * 1024 {
        Strategy::Stream
    } else {
        Strategy::Disk
    }
}

// ---------------------------------------------------------------------------
// Filters (belt-and-suspenders; watcher already applied them once)
// ---------------------------------------------------------------------------

fn passes_filters(path: &Path, cfg: &Config) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();
    if !cfg.include_extensions.iter().any(|inc| inc.eq_ignore_ascii_case(&ext)) {
        return false;
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    for pat in &cfg.exclude_patterns {
        if glob::Pattern::new(pat).map(|p| p.matches(name)).unwrap_or(false) {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Main intake entry point
// ---------------------------------------------------------------------------

pub struct IntakeProcessor {
    queue:  Arc<JobQueue>,
    config: Arc<RwLock<Config>>,
}

impl IntakeProcessor {
    pub fn new(queue: Arc<JobQueue>, config: Arc<RwLock<Config>>) -> Self {
        Self { queue, config }
    }

    pub async fn process(&self, path: PathBuf) -> Result<()> {
        let cfg = self.config.read().await.clone();

        // Belt-and-suspenders filter
        if !passes_filters(&path, &cfg) {
            return Ok(());
        }

        let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let dir = path.parent().unwrap_or(Path::new("."));

        match classify(filename) {
            MultipartClass::Single => {
                self.enqueue_path(&path, &cfg).await?;
            }

            MultipartClass::NewStylePart { base, part_num } => {
                let group_id = format!("{}/{}", dir.display(), base);
                self.queue.add_part(&group_id, &path)?;

                // Only check completeness when we've seen at least part2 — part1 alone
                // is ambiguous (we can't tell if more parts are still being downloaded).
                if part_num >= 2 && new_style_complete(dir, &base, part_num) {
                    let main = dir.join(format!("{base}.part1.rar"));
                    if main.exists() {
                        self.enqueue_path(&main, &cfg).await?;
                    }
                }
            }

            MultipartClass::OldStyleMain { ref base } => {
                let group_id = format!("{}/{}", dir.display(), base);
                self.queue.add_part(&group_id, &path)?;

                if old_style_complete(dir, base) {
                    self.enqueue_path(&path, &cfg).await?;
                }
            }

            MultipartClass::OldStylePart { ref base, .. } => {
                let group_id = format!("{}/{}", dir.display(), base);
                self.queue.add_part(&group_id, &path)?;

                if old_style_complete(dir, base) {
                    let main = dir.join(format!("{base}.rar"));
                    if main.exists() {
                        self.enqueue_path(&main, &cfg).await?;
                    }
                }
            }
        }

        Ok(())
    }

    async fn enqueue_path(&self, path: &Path, cfg: &Config) -> Result<()> {
        let hash = hash_file(path).await?;

        // Dedup check
        if self.queue.is_duplicate(&hash)? {
            tracing::info!("skipping duplicate: {} (hash {})", path.display(), &hash[..12]);
            return Ok(());
        }

        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let strategy = select_strategy(size, cfg.conversion.stream_to_transfer_threshold_mb);
        let dests: Vec<String> = cfg.destinations.iter().map(|d| d.host.clone()).collect();

        let id = self.queue.enqueue(path, &hash, strategy.clone(), &dests)?;
        tracing::info!(
            "enqueued job {id} for {} ({:?}, {} dests)",
            path.display(),
            strategy,
            dests.len()
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_queue(dir: &TempDir) -> Arc<JobQueue> {
        Arc::new(JobQueue::new(&dir.path().join("q.db")).unwrap())
    }

    fn make_config(watch: &Path) -> Arc<RwLock<Config>> {
        use crate::config::*;
        Arc::new(RwLock::new(Config {
            watch_folder: watch.to_path_buf(),
            failed_folder: watch.join("failed"),
            exclude_patterns: vec!["*.tmp".into()],
            include_extensions: vec!["rar".into()],
            watch_subdirs: None,
            min_file_age_secs: 0,
            conversion: ConversionConfig {
                compression_level: 6,
                archive_password: None,
                stream_to_transfer_threshold_mb: 500,
                multipart_timeout_hours: 24,
            },
            destinations: vec![],
            transfer: TransferConfig::default(),
            observability: ObservabilityConfig::default(),
            smtp: None,
        }))
    }

    fn write_file(dir: &TempDir, name: &str) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, b"dummy archive content").unwrap();
        path
    }

    #[tokio::test]
    async fn single_rar_enqueued_immediately() {
        let dir = TempDir::new().unwrap();
        let qdir = TempDir::new().unwrap();
        let queue  = make_queue(&qdir);
        let config = make_config(dir.path());
        let proc   = IntakeProcessor::new(Arc::clone(&queue), config);

        let path = write_file(&dir, "archive.rar");
        proc.process(path).await.unwrap();

        let depth = queue.queue_depth().unwrap();
        let pending = depth.get("pending").copied().unwrap_or(0);
        assert_eq!(pending, 1);
    }

    #[tokio::test]
    async fn excluded_file_not_enqueued() {
        let dir = TempDir::new().unwrap();
        let qdir = TempDir::new().unwrap();
        let queue  = make_queue(&qdir);
        let config = make_config(dir.path());
        let proc   = IntakeProcessor::new(Arc::clone(&queue), config);

        let path = write_file(&dir, "partial.tmp");
        proc.process(path).await.unwrap();

        let depth = queue.queue_depth().unwrap();
        assert_eq!(depth.values().sum::<i64>(), 0);
    }

    #[tokio::test]
    async fn duplicate_hash_not_enqueued_twice() {
        let dir = TempDir::new().unwrap();
        let qdir = TempDir::new().unwrap();
        let queue  = make_queue(&qdir);
        let config = make_config(dir.path());
        let proc   = IntakeProcessor::new(Arc::clone(&queue), config);

        let path = write_file(&dir, "archive.rar");

        // Pre-seed the queue with this hash as a completed job (simulates prior run)
        let hash = hash_file(&path).await.unwrap();
        let id = queue.enqueue(&path, &hash, Strategy::Disk, &[]).unwrap();
        queue.set_status(&id, crate::queue::Status::Done, None).unwrap();

        // Intake should recognise the duplicate and not create a new pending job
        proc.process(path).await.unwrap();

        let depth = queue.queue_depth().unwrap();
        assert_eq!(depth.get("pending").copied().unwrap_or(0), 0);
        assert_eq!(depth.get("done").copied().unwrap_or(0), 1);
    }

    #[test]
    fn strategy_disk_when_small() {
        assert_eq!(select_strategy(100 * 1024 * 1024, 500), Strategy::Disk);
    }

    #[test]
    fn strategy_stream_when_large() {
        assert_eq!(select_strategy(600 * 1024 * 1024, 500), Strategy::Stream);
    }

    #[test]
    fn strategy_disk_when_threshold_zero() {
        assert_eq!(select_strategy(u64::MAX, 0), Strategy::Disk);
    }

    #[tokio::test]
    async fn new_style_multipart_not_enqueued_until_complete() {
        let dir = TempDir::new().unwrap();
        let qdir = TempDir::new().unwrap();
        let queue  = make_queue(&qdir);
        let config = make_config(dir.path());
        let proc   = IntakeProcessor::new(Arc::clone(&queue), config);

        // Only part1 arrives — part2 not yet on disk
        let p1 = write_file(&dir, "movie.part1.rar");
        proc.process(p1).await.unwrap();

        let depth = queue.queue_depth().unwrap();
        assert_eq!(depth.values().sum::<i64>(), 0, "should not enqueue until complete");

        // part2 arrives — now complete (no part3 on disk)
        let _p2 = write_file(&dir, "movie.part2.rar");
        // Re-process part2 path (simulating watcher event)
        let p2 = dir.path().join("movie.part2.rar");
        proc.process(p2).await.unwrap();

        let depth2 = queue.queue_depth().unwrap();
        assert_eq!(depth2.get("pending").copied().unwrap_or(0), 1);
    }
}
