pub mod bucket;
pub mod drr;
pub mod metrics;

pub use bucket::{ClientRateLimiter, TokenBucket};
pub use drr::{ArchiveId, ArchiveScheduler, ClientId, Task};
pub use metrics::SchedulerMetrics;

use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

// ---------------------------------------------------------------------------
// SchedulerConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Deserialize)]
pub struct SchedulerConfig {
    /// Max concurrent inflate tasks per archive. Default: 4.
    #[serde(default = "default_max_per_archive")]
    pub max_per_archive: usize,

    /// Enable per-client rate limiting. Default: false.
    #[serde(default)]
    pub rate_limit_enabled: bool,

    /// Rate limit: tokens per second per client. Default: 100.
    #[serde(default = "default_rate_per_client")]
    pub rate_per_client: i64,

    /// Rate limit: burst capacity per client. Default: 200.
    #[serde(default = "default_burst_per_client")]
    pub burst_per_client: i64,

    /// Enable full DRR scheduler. Default: false. Requires rate_limit_enabled.
    #[serde(default)]
    pub drr_enabled: bool,

    /// Max tasks per client queue before 429. Default: 50.
    #[serde(default = "default_max_queue_depth")]
    pub max_queue_depth: usize,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            max_per_archive: default_max_per_archive(),
            rate_limit_enabled: false,
            rate_per_client: default_rate_per_client(),
            burst_per_client: default_burst_per_client(),
            drr_enabled: false,
            max_queue_depth: default_max_queue_depth(),
        }
    }
}

fn default_max_per_archive() -> usize {
    4
}
fn default_rate_per_client() -> i64 {
    100
}
fn default_burst_per_client() -> i64 {
    200
}
fn default_max_queue_depth() -> usize {
    50
}

// ---------------------------------------------------------------------------
// ArchiveSemaphores - Step 1 isolation
// ---------------------------------------------------------------------------

/// Per-archive bounded permit pool. An inflate task must acquire a permit
/// before work starts; the permit is released on drop.
pub struct ArchiveSemaphores {
    map: DashMap<String, Arc<Semaphore>>,
    max_per_archive: usize,
}

impl ArchiveSemaphores {
    pub fn new(max_per_archive: usize) -> Arc<Self> {
        Arc::new(Self {
            map: DashMap::new(),
            max_per_archive,
        })
    }

    /// Acquire a permit for `archive_id`, creating the semaphore on first call.
    pub async fn acquire(&self, archive_id: &str) -> OwnedSemaphorePermit {
        let sem = self
            .map
            .entry(archive_id.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(self.max_per_archive)))
            .clone();
        sem.acquire_owned().await.expect("semaphore never closed")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn semaphore_max_concurrent_succeed() {
        let sems = ArchiveSemaphores::new(4);
        let mut permits = Vec::new();
        for _ in 0..4 {
            permits.push(sems.acquire("test.zip").await);
        }
        assert_eq!(permits.len(), 4);
    }

    #[tokio::test]
    async fn semaphore_fifth_blocks_until_release() {
        let sems = Arc::new(ArchiveSemaphores::new(1));
        let permit = sems.acquire("z.zip").await;
        // Spawn the fifth acquire in the background; it must wait.
        let sems2 = Arc::clone(&sems);
        let handle = tokio::spawn(async move { sems2.acquire("z.zip").await });
        // Drop the first permit - the waiting task should now get through.
        drop(permit);
        let _second = tokio::time::timeout(std::time::Duration::from_millis(100), handle)
            .await
            .expect("timed out waiting for permit")
            .expect("task panicked");
    }
}
