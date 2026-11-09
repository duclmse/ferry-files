use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub struct SchedulerMetrics {
    pub tasks_dispatched: AtomicU64,
    pub tasks_rejected_429: AtomicU64,
    pub queue_wait_ns_total: AtomicU64,
    pub execution_ns_total: AtomicU64,
    pub inflight: AtomicU64,
}

impl SchedulerMetrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            tasks_dispatched: AtomicU64::new(0),
            tasks_rejected_429: AtomicU64::new(0),
            queue_wait_ns_total: AtomicU64::new(0),
            execution_ns_total: AtomicU64::new(0),
            inflight: AtomicU64::new(0),
        })
    }

    pub fn inc_dispatched(&self) {
        self.tasks_dispatched.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_rejected(&self) {
        self.tasks_rejected_429.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_inflight(&self) {
        self.inflight.fetch_add(1, Ordering::Relaxed);
    }
    pub fn dec_inflight(&self) {
        self.inflight.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn add_wait_ns(&self, ns: u64) {
        self.queue_wait_ns_total.fetch_add(ns, Ordering::Relaxed);
    }

    pub fn add_exec_ns(&self, ns: u64) {
        self.execution_ns_total.fetch_add(ns, Ordering::Relaxed);
    }

    pub fn prometheus_text(&self) -> String {
        let dispatched = self.tasks_dispatched.load(Ordering::Relaxed);
        let rejected = self.tasks_rejected_429.load(Ordering::Relaxed);
        let wait_ns = self.queue_wait_ns_total.load(Ordering::Relaxed);
        let exec_ns = self.execution_ns_total.load(Ordering::Relaxed);
        let inflight = self.inflight.load(Ordering::Relaxed);

        format!(
            "# HELP archive_scheduler_tasks_dispatched_total Total archive tasks dispatched\n\
             # TYPE archive_scheduler_tasks_dispatched_total counter\n\
             archive_scheduler_tasks_dispatched_total {dispatched}\n\
             # HELP archive_scheduler_tasks_rejected_total Total archive tasks rejected (429)\n\
             # TYPE archive_scheduler_tasks_rejected_total counter\n\
             archive_scheduler_tasks_rejected_total {rejected}\n\
             # HELP archive_scheduler_queue_wait_seconds Total time tasks spent queued\n\
             # TYPE archive_scheduler_queue_wait_seconds counter\n\
             archive_scheduler_queue_wait_seconds {:.6}\n\
             # HELP archive_scheduler_execution_seconds Total time tasks spent executing\n\
             # TYPE archive_scheduler_execution_seconds counter\n\
             archive_scheduler_execution_seconds {:.6}\n\
             # HELP archive_scheduler_inflight Current number of inflight archive tasks\n\
             # TYPE archive_scheduler_inflight gauge\n\
             archive_scheduler_inflight {inflight}\n",
            wait_ns as f64 / 1e9,
            exec_ns as f64 / 1e9,
        )
    }
}
