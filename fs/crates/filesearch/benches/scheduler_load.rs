//! Criterion benchmarks for the two-level DRR scheduler (Phase 13).
//!
//! Run:
//!   cargo bench -p filesearch --bench scheduler_load

use std::net::IpAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};

use filesearch::scheduler::{ArchiveScheduler, SchedulerMetrics, Task};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn make_ip(i: u8) -> IpAddr {
    IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, i))
}

fn make_task(client: u8, archive: u8, priority: u8, cost: u32) -> Task {
    Task {
        client:      make_ip(client),
        archive:     format!("archive_{archive}.zip"),
        cost,
        priority,
        enqueued_ns: now_ns(),
    }
}

fn make_scheduler() -> ArchiveScheduler {
    ArchiveScheduler::new(SchedulerMetrics::new(), 1000, 4)
}

// ---------------------------------------------------------------------------
// Benchmark: enqueue + dispatch 1000 tasks (10 clients × 10 archives × 10 tasks)
// ---------------------------------------------------------------------------

fn bench_dispatch_1000(c: &mut Criterion) {
    let mut g = c.benchmark_group("scheduler");
    g.throughput(Throughput::Elements(1000));

    g.bench_function("dispatch_1000_tasks", |b| {
        b.iter(|| {
            let mut sched = make_scheduler();
            let now = now_ns();

            // 10 clients × 10 archives × 10 tasks = 1000.
            for client in 0u8..10 {
                for archive in 0u8..10 {
                    for _ in 0..10 {
                        let task = make_task(client, archive, 1, 10);
                        let _ = sched.enqueue(task, now);
                    }
                }
            }

            let mut dispatched = 0usize;
            while let Some(task) = sched.dispatch(now_ns()) {
                sched.on_complete(&task.client, &task.archive, 0);
                dispatched += 1;
            }
            black_box(dispatched)
        });
    });

    g.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: enqueue only (measures queue insertion)
// ---------------------------------------------------------------------------

fn bench_enqueue_1000(c: &mut Criterion) {
    let mut g = c.benchmark_group("scheduler");
    g.throughput(Throughput::Elements(1000));

    g.bench_function("enqueue_1000_tasks", |b| {
        b.iter(|| {
            let mut sched = make_scheduler();
            let now = now_ns();
            for client in 0u8..10 {
                for archive in 0u8..10 {
                    for _ in 0..10 {
                        let task = make_task(client, archive, 1, 10);
                        black_box(sched.enqueue(task, now).ok());
                    }
                }
            }
            black_box(sched.global_queue_depth())
        });
    });

    g.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: dispatch fairness under imbalanced load
// (1 client with 900 tasks, 9 clients with 100 tasks total)
// ---------------------------------------------------------------------------

fn bench_dispatch_imbalanced(c: &mut Criterion) {
    let mut g = c.benchmark_group("scheduler");
    g.throughput(Throughput::Elements(1000));

    g.bench_function("dispatch_1000_imbalanced", |b| {
        b.iter(|| {
            let mut sched = make_scheduler();
            let now = now_ns();

            // Client 0 gets 900 tasks.
            for _ in 0..900 {
                let task = make_task(0, 0, 2, 1); // bulk priority
                let _ = sched.enqueue(task, now);
            }
            // Clients 1-9 each get ~11 tasks.
            for client in 1u8..10 {
                for _ in 0..11 {
                    let task = make_task(client, client, 1, 1);
                    let _ = sched.enqueue(task, now);
                }
            }

            let mut dispatched = 0usize;
            while let Some(task) = sched.dispatch(now_ns()) {
                sched.on_complete(&task.client, &task.archive, 0);
                dispatched += 1;
            }
            black_box(dispatched)
        });
    });

    g.finish();
}

// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_dispatch_1000,
    bench_enqueue_1000,
    bench_dispatch_imbalanced,
);
criterion_main!(benches);
