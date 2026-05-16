# Phase 12 — Scheduler & Multi-tenant Isolation

**Goal:** Prevent any single client or archive from monopolising CPU or I/O in
the archive service. Implement fair resource sharing through a layered strategy:
first a per-archive semaphore, then per-client token buckets, then a full
two-level Deficit Round Robin (DRR) scheduler. Each layer is independently
testable and the implementation starts with the simplest step that provides
meaningful isolation.

**Estimated scope:** ~650 lines of Rust.

**Depends on:** Phase 10 (streaming ZIP core, inflate pipeline).

**Design source:** `docs/00/global scheduler.md`,
`docs/00/system-level optimizations.md`.

---

## 12.1 Module Layout

```
crates/filesearch/src/scheduler/
├── mod.rs       ← public API: ArchiveScheduler, SchedulerConfig, Task
├── bucket.rs    ← TokenBucket (per-client rate limiting)
├── drr.rs       ← DeficitRoundRobin (archive-level fairness)
└── metrics.rs   ← SchedulerMetrics (AtomicU64 counters)
```

Add `pub mod scheduler;` to `crates/filesearch/src/lib.rs`.

---

## 12.2 Rollout Plan

Build in three steps that can each ship independently:

| Step | What | When to activate |
|------|------|-----------------|
| 1 | Per-archive semaphore | Phase 12 baseline — always on |
| 2 | Per-client token bucket | When `scheduler.rate_limit_enabled = true` |
| 3 | Two-level DRR | When `scheduler.drr_enabled = true` |

Tests verify each step independently and then together.

---

## 12.3 Step 1 — Per-archive Semaphore

The simplest isolation primitive: each archive gets a bounded permit pool. An
inflate task must acquire a permit before starting. This prevents a single large
archive from tying up all workers.

```rust
pub struct ArchiveSemaphores {
    map: DashMap<String, Arc<Semaphore>>,  // archive_id → semaphore
    max_per_archive: usize,
}

impl ArchiveSemaphores {
    pub fn new(max_per_archive: usize) -> Self;

    /// Acquire a permit for archive `id`. Creates semaphore on first call.
    pub async fn acquire(&self, id: &str) -> SemaphorePermit<'_>;
}
```

Used in `api/archive.rs`'s `stream_file` handler:

```rust
let _permit = state.scheduler.semaphores.acquire(&id).await;
// inflate work proceeds here; permit released on drop
```

Default `max_per_archive = 4`. Dynamic sizing formula (advisory, not enforced):

```
max_per_archive = min(num_cpus / active_archives.len().max(1), 4).max(1)
```

---

## 12.4 Step 2 — Per-client Token Bucket

Rate-limits incoming requests by client IP. Each IP gets a `TokenBucket` with a
configured `rate` (tokens/second) and `burst` capacity.

```rust
pub struct TokenBucket {
    tokens:           i64,
    rate:             i64,   // tokens added per second
    burst:            i64,   // max tokens (cap)
    last_refill_ns:   u64,
}

impl TokenBucket {
    pub fn new(rate: i64, burst: i64) -> Self;

    /// Refill based on elapsed nanoseconds, then try to consume `cost`.
    /// Returns Ok(()) if tokens available, Err if depleted.
    pub fn try_consume(&mut self, cost: i64, now_ns: u64) -> Result<(), ()>;

    fn refill(&mut self, now_ns: u64) {
        let dt = now_ns.saturating_sub(self.last_refill_ns);
        let add = (dt as i64 * self.rate) / 1_000_000_000;
        self.tokens = (self.tokens + add).min(self.burst);
        self.last_refill_ns = now_ns;
    }
}
```

Token buckets are stored in `DashMap<IpAddr, Mutex<TokenBucket>>` with LRU
eviction after 5 minutes of inactivity:

```rust
pub struct ClientRateLimiter {
    buckets: Arc<DashMap<IpAddr, (Mutex<TokenBucket>, Instant)>>,
    rate:    i64,
    burst:   i64,
}

impl ClientRateLimiter {
    pub fn new(rate_per_sec: i64, burst: i64) -> Self;

    /// Returns HTTP 429 response if client is over limit, Ok otherwise.
    pub fn check(&self, ip: IpAddr, cost: i64) -> Result<(), StatusCode>;
}
```

Cost model: each file stream request costs `compressed_size / 4096 + 1` tokens.
Directory listing and index requests cost 1 token.

Axum middleware layer — insert before archive routes:

```rust
let archive_router = Router::new()
    // ...existing archive routes...
    .layer(axum::middleware::from_fn_with_state(
        state.clone(),
        rate_limit_middleware,
    ));
```

---

## 12.5 Step 3 — Two-level Deficit Round Robin

Full hierarchical scheduler when both per-archive semaphores and client token
buckets are insufficient under sustained multi-client load.

### Core Types

```rust
pub type ClientId = IpAddr;
pub type ArchiveId = String;

pub struct Task {
    pub client:   ClientId,
    pub archive:  ArchiveId,
    pub cost:     u32,        // estimated CPU units (compressed_bytes / 4096)
    pub priority: u8,         // 0=latency (dir listing), 1=normal, 2=bulk (large file)
    pub work:     BoxFuture<'static, io::Result<Bytes>>,
}

struct ArchiveQueue {
    deficit:         i64,
    quantum:         i64,
    high:            VecDeque<Task>,
    normal:          VecDeque<Task>,
    bulk:            VecDeque<Task>,
    inflight:        usize,
    max_concurrency: usize,
}

struct ClientQueue {
    tokens:          i64,
    rate:            i64,
    burst:           i64,
    last_refill_ns:  u64,
    deficit:         i64,
    quantum:         i64,
    archives:        HashMap<ArchiveId, ArchiveQueue>,
    rr_idx:          usize,
}

pub struct ArchiveScheduler {
    clients:        HashMap<ClientId, ClientQueue>,
    client_order:   Vec<ClientId>,
    rr_idx:         usize,
    workers:        usize,  // = num_cpus
    metrics:        Arc<SchedulerMetrics>,
}
```

### Dispatch

```rust
impl ArchiveScheduler {
    /// Pick the next task to execute, or return None if all queues are empty.
    /// Borrow structure: pick_client returns ClientId (Copy), borrow ends before
    /// the subsequent get_mut — required for borrow checker correctness.
    pub fn dispatch(&mut self, now_ns: u64) -> Option<Task>;

    pub fn enqueue(&mut self, task: Task);

    pub fn on_complete(&mut self, client: &ClientId, archive: &ArchiveId);
}
```

DRR algorithm:

```text
for each pass through clients (round-robin):
    client_id = next in round-robin order
    refill client tokens
    if client token bucket < task.cost: skip
    deficit += quantum
    if deficit <= 0: skip
    for each archive in client's archives (round-robin):
        archive deficit += quantum
        if archive inflight >= max_concurrency: skip
        if archive queue empty: skip
        task = dequeue highest-priority task
        charge client tokens, client deficit, archive deficit
        archive inflight += 1
        return task
```

### Backpressure

When a client queue exceeds `max_queue_depth` tasks: reject the new task with
HTTP 429. Drop bulk tasks first when global queue is overloaded:

```rust
if total_queue_depth > GLOBAL_LIMIT {
    drop_bulk_tasks();
}
```

### Worker Loop

Fixed-size `tokio::task::JoinSet` capped at `workers` concurrent tasks. The
scheduler loop:

```rust
loop {
    while inflight < workers {
        if let Some(task) = scheduler.dispatch(now_ns()) {
            let result_tx = result_tx.clone();
            join_set.spawn(async move {
                let result = (task.work).await;
                result_tx.send((task.client, task.archive, result)).await.ok();
            });
            inflight += 1;
        } else {
            break;
        }
    }
    if let Some(res) = join_set.join_next().await {
        inflight -= 1;
        scheduler.on_complete(/* ... */);
    }
}
```

---

## 12.6 `SchedulerMetrics`

```rust
pub struct SchedulerMetrics {
    pub tasks_dispatched:     AtomicU64,
    pub tasks_rejected_429:   AtomicU64,
    pub queue_wait_ns_total:  AtomicU64,
    pub execution_ns_total:   AtomicU64,
    pub inflight:             AtomicU64,
}
```

Exposed via `GET /metrics` Prometheus format (extend existing metrics endpoint):

```
# HELP archive_scheduler_tasks_dispatched_total ...
archive_scheduler_tasks_dispatched_total N

# HELP archive_scheduler_queue_wait_seconds ...
archive_scheduler_queue_wait_seconds N.NNN

# HELP archive_scheduler_inflight ...
archive_scheduler_inflight N
```

---

## 12.7 New Config Section

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct SchedulerConfig {
    /// Max concurrent inflate tasks per archive. Default: 4.
    pub max_per_archive: usize,

    /// Enable per-client rate limiting. Default: false.
    pub rate_limit_enabled: bool,

    /// Rate limit: tokens per second per client. Default: 100.
    pub rate_per_client: i64,

    /// Rate limit: burst capacity per client. Default: 200.
    pub burst_per_client: i64,

    /// Enable full DRR scheduler. Default: false. Requires rate_limit_enabled.
    pub drr_enabled: bool,

    /// Max tasks per client queue before 429. Default: 50.
    pub max_queue_depth: usize,
}
```

Add `pub scheduler: SchedulerConfig` to `ArchiveServiceConfig`.

---

## 12.8 Tests — Phase 12

```
token bucket:
- new bucket, try_consume below burst → Ok
- try_consume more than burst → Err
- refill after elapsed time → tokens replenished correctly
- refill does not exceed burst

per-archive semaphore:
- 4 concurrent acquires on max_per_archive=4 → all succeed
- 5th acquire blocks until one is released

client rate limiter:
- first request: Ok
- requests at rate_per_client/s steady state: all Ok
- burst of 200+1 requests → 201st returns 429

DRR:
- two clients, each with one task of equal cost → dispatched alternately
- client A has 10 small tasks, client B has 1 large task → B not starved (DRR)
- archive concurrency: enqueue 10 tasks for archive with max=2 → only 2 inflight
- on_complete → inflight decreases, next task dispatched

backpressure:
- queue at max_queue_depth + 1 → Err (429) returned from enqueue
- bulk tasks dropped first under global overload

metrics:
- tasks_dispatched increments on each dispatch
- queue_wait_ns_total reflects real wait time
```

---

## 12.9 Definition of Done

- [x] Step 1 (per-archive semaphore) is always active for archive service routes.
- [x] Step 2 (token bucket) returns 429 when enabled and client exceeds limit.
- [x] Step 3 (DRR) dispatches fairly between clients under synthetic load test.
- [x] Scheduler metrics appear in `GET /metrics` output.
- [x] `cargo test -p filesearch scheduler` passes all tests.
- [x] No `unwrap()` in `scheduler/` module.
- [x] `cargo clippy -- -D warnings` clean.
- [ ] A 1-client / 1-archive load test with `max_per_archive=2` shows ≤ 2
      concurrent inflates at any moment (verified via `inflight` metric).

---

## 12.10 Implementation Notes

**Implemented:** 2026-05-16

### Module Layout (as built)

```
crates/filesearch/src/scheduler/
├── mod.rs      ArchiveSemaphores, SchedulerConfig, re-exports
├── bucket.rs   TokenBucket, ClientRateLimiter
├── drr.rs      Task, ArchiveScheduler (two-level DRR)
└── metrics.rs  SchedulerMetrics, prometheus_text()
```

### Key Deviations from Spec

- **`TokenBucket::try_consume` returns `bool` (not `Result<(), ()>`)** — Clippy
  `result_unit_err` forbids `Result<_, ()>`; `bool` is cleaner and sufficient.

- **`TokenBucket::with_now(rate, burst, now_ns)`** constructor added alongside
  `new()` for test determinism; tests that use fixed timestamps must call
  `with_now` to avoid `saturating_sub` returning 0 for timestamps in the past.

- **`write_dzx` params wrapped in `WriteDzxParams<'_>` struct** (Phase 11 fix) —
  Clippy `too_many_arguments` limit is 7; the 8-arg function was wrapped.

- **Rate-limiter stored in `AppState.rate_limiter`** — the middleware creates one
  instance per `AppState`, not per request. `ClientRateLimiter` holds
  `Arc<DashMap<IpAddr, …>>` so `Clone` on `AppState` shares state correctly.

- **DRR scheduler (`ArchiveScheduler`) is unit-testable but not yet wired to a
  worker loop** — the spec §12.5 worker loop requires a long-running task with
  channels; this is a separate wiring concern. The scheduler logic itself is
  fully implemented and tested. Wire it into a background task when
  `drr_enabled = true` in a future step.

- **`GET /metrics`** returns `text/plain; version=0.0.4` (Prometheus text format)
  with five counters/gauges from `SchedulerMetrics`.

### AppState additions

```rust
pub archive_semaphores: Arc<ArchiveSemaphores>,
pub scheduler_metrics:  Arc<SchedulerMetrics>,
pub rate_limiter:       Arc<ClientRateLimiter>,
```

`daemon.rs` constructs these from `cfg.archive_service.scheduler` at startup.

### Config

`ArchiveServiceConfig.scheduler: SchedulerConfig` — TOML section
`[archive_service.scheduler]` with keys: `max_per_archive`, `rate_limit_enabled`,
`rate_per_client`, `burst_per_client`, `drr_enabled`, `max_queue_depth`.
