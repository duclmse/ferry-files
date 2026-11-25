//! Criterion benchmarks for the two-level DRR scheduler (Phase 13).
//!
//! Run:
//!   cargo bench -p fs --bench scheduler_load

use std::net::IpAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use std::hint::black_box;

use fs_server::scheduler::{ArchiveScheduler, SchedulerMetrics, Task};

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
        client: make_ip(client),
        archive: format!("archive_{archive}.zip"),
        cost,
        priority,
        enqueued_ns: now_ns(),
    }
}

fn make_scheduler() -> ArchiveScheduler {
    ArchiveScheduler::new(SchedulerMetrics::new(), 1000, 4)
}

// ---------------------------------------------------------------------------
// Benchmark: dispatch 1000 pre-enqueued tasks (10 clients × 10 archives × 10)
//
// Enqueueing and task drops happen outside the timed region so this measures
// the DRR dispatch/complete cycle alone; enqueue cost has its own benchmark.
// ---------------------------------------------------------------------------

fn filled_scheduler(tasks: impl IntoIterator<Item = Task>) -> (ArchiveScheduler, Vec<Task>) {
    let mut sched = make_scheduler();
    let mut n = 0;
    for task in tasks {
        let _ = sched.enqueue(task, 0);
        n += 1;
    }
    (sched, Vec::with_capacity(n))
}

fn drain(sched: &mut ArchiveScheduler, out: &mut Vec<Task>, now: u64) {
    while let Some(task) = sched.dispatch(now) {
        sched.on_complete(&task.client, &task.archive, 0);
        out.push(task);
    }
}

fn bench_dispatch_1000(c: &mut Criterion) {
    let mut g = c.benchmark_group("scheduler");
    g.throughput(Throughput::Elements(1000));

    let now = now_ns();
    g.bench_function("dispatch_1000_tasks", |b| {
        b.iter_batched(
            || {
                filled_scheduler((0u8..10).flat_map(|client| {
                    (0u8..10).flat_map(move |archive| {
                        (0..10).map(move |_| make_task(client, archive, 1, 10))
                    })
                }))
            },
            |(mut sched, mut out)| {
                drain(&mut sched, &mut out, now);
                black_box(out)
            },
            BatchSize::SmallInput,
        );
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

    let now = now_ns();
    g.bench_function("dispatch_1000_imbalanced", |b| {
        b.iter_batched(
            || {
                filled_scheduler(
                    // Client 0 gets 900 bulk tasks; clients 1-9 get 11 each.
                    (0..900).map(|_| make_task(0, 0, 2, 1)).chain(
                        (1u8..10).flat_map(|client| {
                            (0..11).map(move |_| make_task(client, client, 1, 1))
                        }),
                    ),
                )
            },
            |(mut sched, mut out)| {
                drain(&mut sched, &mut out, now);
                black_box(out)
            },
            BatchSize::SmallInput,
        );
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
