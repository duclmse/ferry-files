# Global Scheduler Design Note

This is future architecture research for a multi-tenant archive service. The
Rust snippets are pseudocode for scheduling logic; production code must be
structured to satisfy Rust borrowing rules and async cancellation/backpressure.

You’re now designing the **control plane** of your system: who gets CPU, when, and how much—across tenants and archives—without destroying latency or throughput. The right model is a **hierarchical scheduler**:

* **Level 1:** Tenant fairness (protects users from each other)
* **Level 2:** Archive fairness within a tenant (prevents noisy ZIPs)
* **Level 3:** Task execution on CPU cores (keeps hardware saturated)

Think “user-space kernel scheduler,” but specialized for **I/O + decompression workloads**.

---

# 1) Scheduling model (hierarchical DRR + token buckets)

## Why not a single queue?

A flat queue can’t express:

* tenant quotas
* per-archive limits
* latency vs throughput priorities

## Chosen primitives

* **Token buckets** → rate limiting / burst control
* **Deficit Round Robin (DRR)** → fair sharing with variable-cost tasks
* **Work stealing** → keep cores busy

---

# 2) Core data structures

```rust id="q2u6ij"
type TenantId = u32;
type ArchiveId = u32;

struct Task {
    tenant: TenantId,
    archive: ArchiveId,
    cost: u32,        // estimated CPU units (e.g., bytes to inflate / 4KB)
    priority: u8,     // 0 = latency-sensitive, 1 = normal, 2 = bulk
    job: Job,         // enum: DirList | Inflate | HexPreview | ...
}

struct ArchiveQueue {
    deficit: i64,
    quantum: i64,          // share for this archive
    queue: VecDeque<Task>,
    inflight: usize,
    max_concurrency: usize, // per-archive cap
}

struct TenantQueue {
    tokens: i64,            // token bucket (CPU budget)
    rate: i64,              // tokens/sec
    burst: i64,
    last_refill_ns: u64,

    deficit: i64,
    quantum: i64,           // tenant share
    archives: HashMap<ArchiveId, ArchiveQueue>,
    order: Vec<ArchiveId>,  // round-robin order
    rr_idx: usize,
}

struct Scheduler {
    tenants: HashMap<TenantId, TenantQueue>,
    tenant_order: Vec<TenantId>,
    rr_idx: usize,
}
```

---

# 3) Cost model (critical for fairness)

You need a **cheap, monotonic estimate**:

```text id="0m4u2g"
cost ≈ compressed_bytes / 4KB  + small constant
```

* directory listing: tiny constant (e.g., 1–2)
* small file preview: small
* large inflate: large

You don’t need perfect accuracy—just enough to avoid starvation.

---

# 4) Token bucket (tenant-level rate limiting)

```rust id="6x6l5f"
fn refill_tokens(t: &mut TenantQueue, now_ns: u64) {
    let dt = now_ns - t.last_refill_ns;
    let add = (dt as i64 * t.rate) / 1_000_000_000;
    t.tokens = (t.tokens + add).min(t.burst);
    t.last_refill_ns = now_ns;
}
```

Admission check:

```rust id="m0d0g0"
fn tenant_can_run(t: &TenantQueue, cost: i64) -> bool {
    t.tokens >= cost
}
```

On dispatch:

```rust id="m6f6hj"
t.tokens -= cost;
```

---

# 5) DRR at two levels

## Level 1: Tenants

```rust id="j0t6g9"
fn pick_tenant(s: &mut Scheduler) -> Option<TenantId> {
    for _ in 0..s.tenant_order.len() {
        let id = s.tenant_order[s.rr_idx];
        s.rr_idx = (s.rr_idx + 1) % s.tenant_order.len();

        let t = s.tenants.get_mut(&id)?;
        t.deficit += t.quantum;

        if t.deficit > 0 {
            return Some(id);
        }
    }
    None
}
```

## Level 2: Archives within tenant

```rust id="0s0u4y"
fn pick_archive(t: &mut TenantQueue) -> Option<ArchiveId> {
    for _ in 0..t.order.len() {
        let aid = t.order[t.rr_idx];
        t.rr_idx = (t.rr_idx + 1) % t.order.len();

        let a = t.archives.get_mut(&aid)?;
        a.deficit += a.quantum;

        if a.deficit > 0 && a.inflight < a.max_concurrency && !a.queue.is_empty() {
            return Some(aid);
        }
    }
    None
}
```

---

# 6) Dispatch loop (central scheduler)

`pick_tenant` and `pick_archive` take `&mut` references and return only the ID
(a `Copy` type). The mutable borrow from `pick_tenant(s)` ends before the next
`s.tenants.get_mut()` call — this is required for the code to satisfy Rust's
borrow checker. In production code ensure each function returns the ID and drops
its borrow before the caller re-borrows `s`.

```rust id="c4h1h8"
fn dispatch(s: &mut Scheduler, now_ns: u64) -> Option<Task> {
    for _ in 0..s.tenant_order.len() {
        let tid = pick_tenant(s)?;  // borrow ends here; tid is Copy
        let t = s.tenants.get_mut(&tid)?;

        refill_tokens(t, now_ns);

        let aid = match pick_archive(t) {
            Some(a) => a,
            None => continue,
        };

        let a = t.archives.get_mut(&aid)?;
        let task = a.queue.front()?;

        if !tenant_can_run(t, task.cost as i64) {
            continue;
        }

        // charge
        t.tokens -= task.cost as i64;
        t.deficit -= task.cost as i64;
        a.deficit -= task.cost as i64;

        // dispatch
        let task = a.queue.pop_front()?;
        a.inflight += 1;

        return Some(task);
    }
    None
}
```

---

# 7) Worker execution (per-core)

## Model

* Fixed number of worker threads = CPU cores
* Each thread:

  * pulls from scheduler
  * executes
  * reports completion

```rust id="r9qv5m"
loop {
    if let Some(task) = scheduler.dispatch(now()) {
        execute(task);
        on_complete(task);
    } else {
        park_or_steal();
    }
}
```

---

# 8) Completion path

```rust id="g5b5f8"
fn on_complete(s: &mut Scheduler, task: &Task) -> Option<()> {
    let t = s.tenants.get_mut(&task.tenant)?;
    let a = t.archives.get_mut(&task.archive)?;

    a.inflight -= 1;
    Some(())
}
```

---

# 9) Priority handling (latency vs throughput)

Use **multi-queue per archive**:

```rust id="k0j2dp"
struct ArchiveQueue {
    high: VecDeque<Task>,
    normal: VecDeque<Task>,
    bulk: VecDeque<Task>,
    ...
}
```

Selection order:

1. high
2. normal
3. bulk

Additionally:

* give higher **quantum** to latency queues
* cap bulk concurrency

---

# 10) Work stealing (keep cores busy)

If a worker can’t get work from the global scheduler:

* try local shard (NUMA-aware)
* steal from another shard

```rust id="6w9o9l"
fn steal(other: &Shard) -> Option<Task> {
    // take from tail (LIFO) to reduce contention
}
```

---

# 11) NUMA integration

Each NUMA node has:

```rust id="4k5q3c"
struct Shard {
    scheduler: Scheduler,
    workers: Vec<Thread>,
}
```

Routing:

```text id="q6z2pq"
tenant/archive → shard (hash or placement policy)
```

No cross-node scheduling unless stealing.

---

# 12) Backpressure (don’t skip this)

When queues grow:

* reject early (HTTP 429)
* or shed low-priority tasks

```rust id="j9c1q8"
if queue_len > LIMIT {
    drop_bulk_tasks();
}
```

---

# 13) Observability (you’ll need it)

Expose:

* per-tenant:

  * queue length
  * tokens
  * latency
* per-archive:

  * inflight
  * throughput
* per-core:

  * utilization

---

# 14) What this achieves

### Guarantees

* No tenant can starve others
* Large archives don’t block small ones
* CPU stays saturated
* Latency-sensitive tasks stay responsive

---

# 15) Practical rollout (don’t build everything at once)

Start with:

1. Per-tenant token bucket
2. Per-archive concurrency limit
3. Single global queue

Then evolve to:

4. Two-level DRR
5. Priority queues
6. NUMA shards

---

# Bottom line

You’ve now designed a scheduler that behaves like:

> a **hierarchical fair-share CPU scheduler specialized for archive workloads**

It coordinates:

* **who** runs (tenant)
* **what** runs (archive)
* **where** it runs (CPU / NUMA)

---

If you want to go further, the next serious step is:

* **adaptive scheduling** (auto-tune quantum, cost model, and concurrency based on real latency + throughput metrics)
