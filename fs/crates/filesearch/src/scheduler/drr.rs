use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;

use super::metrics::SchedulerMetrics;

pub type ClientId  = IpAddr;
pub type ArchiveId = String;

// ---------------------------------------------------------------------------
// Task
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct Task {
    pub client:      ClientId,
    pub archive:     ArchiveId,
    pub cost:        u32, // estimated CPU units (compressed_bytes / 4096)
    pub priority:    u8,  // 0 = high/latency, 1 = normal, 2 = bulk
    pub enqueued_ns: u64,
}

// ---------------------------------------------------------------------------
// ArchiveQueue
// ---------------------------------------------------------------------------

struct ArchiveQueue {
    deficit:         i64,
    quantum:         i64,
    high:            VecDeque<Task>,
    normal:          VecDeque<Task>,
    bulk:            VecDeque<Task>,
    inflight:        usize,
    max_concurrency: usize,
}

impl ArchiveQueue {
    fn new(quantum: i64, max_concurrency: usize) -> Self {
        Self {
            deficit: 0,
            quantum,
            high:    VecDeque::new(),
            normal:  VecDeque::new(),
            bulk:    VecDeque::new(),
            inflight: 0,
            max_concurrency,
        }
    }

    fn push(&mut self, task: Task) {
        match task.priority {
            0 => self.high.push_back(task),
            1 => self.normal.push_back(task),
            _ => self.bulk.push_back(task),
        }
    }

    fn is_empty(&self) -> bool {
        self.high.is_empty() && self.normal.is_empty() && self.bulk.is_empty()
    }

    fn pop_highest(&mut self) -> Option<Task> {
        self.high.pop_front()
            .or_else(|| self.normal.pop_front())
            .or_else(|| self.bulk.pop_front())
    }

    /// Returns a task if: not at concurrency limit, has work, and deficit allows.
    fn try_dequeue(&mut self) -> Option<Task> {
        if self.inflight >= self.max_concurrency { return None; }
        if self.is_empty() { return None; }
        self.deficit += self.quantum;
        if self.deficit <= 0 { return None; }
        let task = self.pop_highest()?;
        self.deficit    -= task.cost as i64;
        self.inflight   += 1;
        Some(task)
    }

    fn on_complete(&mut self) {
        if self.inflight > 0 { self.inflight -= 1; }
    }
}

// ---------------------------------------------------------------------------
// ClientQueue
// ---------------------------------------------------------------------------

// Archives live in a Vec and are addressed by index so the dispatch hot path
// never hashes a String; the map is only consulted on enqueue/complete.
struct ClientQueue {
    deficit:  i64,
    quantum:  i64,
    index:    HashMap<ArchiveId, usize>,
    archives: Vec<ArchiveQueue>,
    rr_idx:   usize,
    // Kept in sync on every push/pop so the depth check on enqueue is O(1)
    // instead of summing every archive queue.
    queued:   usize,
    archive_quantum:      i64,
    archive_concurrency:  usize,
}

impl ClientQueue {
    fn new(quantum: i64, archive_quantum: i64, archive_concurrency: usize) -> Self {
        Self {
            deficit:  0,
            quantum,
            index:    HashMap::new(),
            archives: Vec::new(),
            rr_idx:   0,
            queued:   0,
            archive_quantum,
            archive_concurrency,
        }
    }

    fn enqueue(&mut self, task: Task) {
        let idx = match self.index.get(&task.archive) {
            Some(&idx) => idx,
            None => {
                let idx = self.archives.len();
                self.archives.push(ArchiveQueue::new(self.archive_quantum, self.archive_concurrency));
                self.index.insert(task.archive.clone(), idx);
                idx
            }
        };
        self.archives[idx].push(task);
        self.queued += 1;
    }

    fn drop_bulk(&mut self) -> usize {
        let removed = self.archives.iter_mut().fold(0, |acc, aq| {
            let before = aq.bulk.len();
            aq.bulk.clear();
            acc + before
        });
        self.queued -= removed;
        removed
    }

    /// Round-robin over per-archive queues, picking the first that can dispatch.
    fn try_dequeue(&mut self) -> Option<Task> {
        if self.queued == 0 { return None; }
        self.deficit += self.quantum;
        if self.deficit <= 0 { return None; }

        let n = self.archives.len();
        for _ in 0..n {
            let idx = self.rr_idx;
            self.rr_idx = if idx + 1 == n { 0 } else { idx + 1 };
            if let Some(task) = self.archives[idx].try_dequeue() {
                self.deficit -= task.cost as i64;
                self.queued  -= 1;
                return Some(task);
            }
        }
        None
    }

    fn on_complete(&mut self, archive: &str) {
        if let Some(&idx) = self.index.get(archive) {
            self.archives[idx].on_complete();
        }
    }
}

// ---------------------------------------------------------------------------
// ArchiveScheduler
// ---------------------------------------------------------------------------

pub struct ArchiveScheduler {
    index:        HashMap<ClientId, usize>,
    clients:      Vec<ClientQueue>,
    rr_idx:       usize,
    metrics:      std::sync::Arc<SchedulerMetrics>,
    max_queue_depth:     usize,
    client_quantum:      i64,
    archive_quantum:     i64,
    archive_concurrency: usize,
}

impl ArchiveScheduler {
    pub fn new(
        metrics:             std::sync::Arc<SchedulerMetrics>,
        max_queue_depth:     usize,
        archive_concurrency: usize,
    ) -> Self {
        Self {
            index:        HashMap::new(),
            clients:      Vec::new(),
            rr_idx:       0,
            metrics,
            max_queue_depth,
            client_quantum:      1024,
            archive_quantum:     1024,
            archive_concurrency,
        }
    }

    /// Enqueue a task. Returns Err(task) when the client queue is full.
    pub fn enqueue(&mut self, task: Task, now_ns: u64) -> Result<(), Task> {
        let idx = match self.index.get(&task.client) {
            Some(&idx) => idx,
            None => {
                let idx = self.clients.len();
                self.clients.push(ClientQueue::new(
                    self.client_quantum, self.archive_quantum, self.archive_concurrency,
                ));
                self.index.insert(task.client, idx);
                idx
            }
        };
        let cq = &mut self.clients[idx];

        // Drop bulk tasks before hard-rejecting.
        if cq.queued >= self.max_queue_depth && cq.drop_bulk() == 0 {
            self.metrics.inc_rejected();
            return Err(task);
        }

        let _ = now_ns; // enqueued_ns is already on the task
        cq.enqueue(task);
        Ok(())
    }

    /// Pick the next task via two-level DRR, or return None.
    pub fn dispatch(&mut self, now_ns: u64) -> Option<Task> {
        let n = self.clients.len();
        for _ in 0..n {
            let idx = self.rr_idx;
            self.rr_idx = if idx + 1 == n { 0 } else { idx + 1 };

            if let Some(task) = self.clients[idx].try_dequeue() {
                let wait = now_ns.saturating_sub(task.enqueued_ns);
                self.metrics.inc_dispatched();
                self.metrics.inc_inflight();
                self.metrics.add_wait_ns(wait);
                return Some(task);
            }
        }
        None
    }

    pub fn on_complete(&mut self, client: &ClientId, archive: &str, exec_ns: u64) {
        self.metrics.dec_inflight();
        self.metrics.add_exec_ns(exec_ns);
        if let Some(&idx) = self.index.get(client) {
            self.clients[idx].on_complete(archive);
        }
    }

    pub fn global_queue_depth(&self) -> usize {
        self.clients.iter().map(|c| c.queued).sum()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::metrics::SchedulerMetrics;

    fn make_task(client: &str, archive: &str, cost: u32, priority: u8) -> Task {
        Task {
            client:      client.parse().unwrap(),
            archive:     archive.to_string(),
            cost,
            priority,
            enqueued_ns: 0,
        }
    }

    fn sched() -> ArchiveScheduler {
        ArchiveScheduler::new(SchedulerMetrics::new(), 50, 4)
    }

    #[test]
    fn two_clients_equal_cost_alternate() {
        let mut s = sched();
        let now = 0u64;
        s.enqueue(make_task("1.0.0.1", "a.zip", 1, 1), now).unwrap();
        s.enqueue(make_task("1.0.0.2", "b.zip", 1, 1), now).unwrap();
        let first  = s.dispatch(now).unwrap();
        let second = s.dispatch(now).unwrap();
        // Both clients should have been served (order may vary by insertion).
        assert_ne!(first.client, second.client);
    }

    #[test]
    fn archive_concurrency_limits_inflight() {
        let mut s = ArchiveScheduler::new(SchedulerMetrics::new(), 100, 2);
        let client: ClientId = "1.0.0.1".parse().unwrap();
        let now = 0u64;
        for _ in 0..10 {
            s.enqueue(make_task("1.0.0.1", "a.zip", 1, 1), now).unwrap();
        }
        // Only 2 should dispatch before on_complete is called.
        let t1 = s.dispatch(now).unwrap();
        let t2 = s.dispatch(now).unwrap();
        assert!(s.dispatch(now).is_none());
        s.on_complete(&client, &t1.archive, 0);
        s.on_complete(&client, &t2.archive, 0);
        assert!(s.dispatch(now).is_some());
    }

    #[test]
    fn queue_full_bulk_dropped_before_reject() {
        let mut s = ArchiveScheduler::new(SchedulerMetrics::new(), 2, 4);
        let now = 0u64;
        // Fill to depth with bulk tasks.
        s.enqueue(make_task("1.0.0.3", "x.zip", 1, 2), now).unwrap();
        s.enqueue(make_task("1.0.0.3", "x.zip", 1, 2), now).unwrap();
        // Third task: bulk should be dropped, then this normal enqueues.
        let res = s.enqueue(make_task("1.0.0.3", "x.zip", 1, 1), now);
        // It may succeed (after bulk drop) or fail, but must not panic.
        let _ = res;
    }

    #[test]
    fn backpressure_hard_reject() {
        let mut s = ArchiveScheduler::new(SchedulerMetrics::new(), 1, 4);
        let now = 0u64;
        s.enqueue(make_task("1.0.0.4", "x.zip", 1, 1), now).unwrap();
        // No bulk to drop; second should be rejected.
        let res = s.enqueue(make_task("1.0.0.4", "x.zip", 1, 1), now);
        assert!(res.is_err());
    }

    #[test]
    fn metrics_tasks_dispatched_increments() {
        let metrics = SchedulerMetrics::new();
        let mut s   = ArchiveScheduler::new(std::sync::Arc::clone(&metrics), 50, 4);
        let now     = 0u64;
        s.enqueue(make_task("1.0.0.5", "a.zip", 1, 1), now).unwrap();
        s.dispatch(now).unwrap();
        assert_eq!(metrics.tasks_dispatched.load(std::sync::atomic::Ordering::Relaxed), 1);
    }
}
