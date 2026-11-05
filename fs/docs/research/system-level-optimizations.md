# System-Level Optimizations Design Note

> **Status:** research note. Design source for `docs/phases/12-scheduler-and-isolation.md`; the phase doc is the
> implementation plan and `docs/reference/` stays authoritative.

These three ideas are about **controlling contention, fault isolation, and
memory locality** once your system scales beyond “a few archives” into **many
concurrent users + large datasets**. They’re orthogonal, but they reinforce each
other.

---

# 1) Multi-tenant archive isolation

## What it actually means

Instead of one global registry:

```text
/archive/{id}/...
```

you introduce **tenancy boundaries**:

```text
/tenant/{tenant_id}/archive/{archive_id}/...
```

Each tenant gets:

- its own **logical namespace**
- optional **resource quotas**
- isolated **failure domain**

---

## Why you need it

Without isolation:

- One user opening a huge ZIP → saturates CPU + I/O
- A malformed archive → crashes shared worker
- Cache pollution across unrelated users

With isolation:

- Contention is scoped
- You can enforce fairness
- You can apply different policies per tenant

---

## Data model

```rust
struct Tenant {
    id: String,
    manager: Arc<ArchiveManager>, // the RCU system you built
    limits: Limits,
}

struct Limits {
    max_workers: usize,
    max_memory: usize,
    max_concurrent_reads: usize,
}
```

---

## Isolation levels (choose intentionally)

### 1. Soft isolation (lightweight)

- shared process
- per-tenant limits enforced in scheduler

### 2. Strong isolation

- separate process per tenant
- or container (cgroup limits)

### 3. Hybrid (practical)

- shared index (mmap is read-only anyway)
- isolated execution (worker pools per tenant)

---

## Key mechanism: budgeting

Before executing a request:

```rust
if tenant.cpu_tokens.try_acquire() {
    // proceed
} else {
    return 429;
}
```

---

# 2) Per-archive worker pools

## Problem

Decompression is CPU-bound. If you use a global pool:

```text
[Archive A huge file] → blocks → [Archive B small file]
```

---

## Solution

Each archive gets its own bounded executor:

```rust
struct ArchiveRuntime {
    pool: tokio::runtime::Handle,
    semaphore: Arc<Semaphore>,
}
```

---

## Execution model

```rust
async fn extract(archive: Arc<Archive>, task: Task) {
    let permit = archive.runtime.semaphore.acquire().await;

    archive.runtime.pool.spawn_blocking(move || {
        // inflate / decode
    });
}
```

---

## Why this matters

### Without it

- head-of-line blocking
- noisy neighbor problem

### With it

- fairness across archives
- predictable latency

---

## Pool sizing strategy

```text
small archives → 1–2 workers
large archives → N workers (bounded)
```

Dynamic:

```rust
workers = min(cpu_cores / active_archives, MAX_PER_ARCHIVE)
```

---

## Advanced: priority scheduling

- small files → high priority
- large streaming → low priority

---

# 3) NUMA-aware shard placement

## Reality check

On modern servers:

- multiple CPU sockets
- each has its own memory (NUMA node)

Accessing “remote” memory:

- 1.5×–3× slower

---

## Problem in your system

- mmap index may sit on one NUMA node
- worker thread runs on another
- every access becomes cross-node

---

## Solution: shard by NUMA node

### Step 1 - detect topology

Use:

- `numa` crate (Linux)
- or manual `/sys/devices/system/node`

---

### Step 2 - partition archives

```rust
struct NumaShard {
    node_id: usize,
    manager: Arc<ArchiveManager>,
}
```

Assign archives:

```text
archive_id % numa_nodes → shard
```

---

### Step 3 - pin threads

```rust
use core_affinity;

core_affinity::set_for_current(core_id);
```

Ensure:

- worker threads run on same node as data

---

## Memory placement

When loading index:

- touch pages on target NUMA node (first-touch policy)

```rust
for page in mmap_chunks {
    read_byte(page); // forces allocation on this node
}
```

---

## Result

```text
Request → shard → local CPU → local memory
```

Instead of:

```text
Request → CPU 0 → memory on CPU 1 → latency penalty
```

---

# How these three fit together

```text
┌─────────────────────────┐
│        Tenant A         │
│  (isolated budgets)     │
└───────────┬─────────────┘
            │
┌───────────▼───────────────┐
│   Archive Manager (RCU)   │
└──────┬──────────┬─────────┘
       │          │
┌──────▼────┐  ┌──▼─────────┐
│ Archive 1 │  │ Archive 2  │
│ pool=2    │  │ pool=4     │
└───────┬───┘  └────┬───────┘
        │           │
┌───────▼───────────▼───────┐
│     NUMA Node 0           │
│  (local threads + mmap)   │
└───────────────────────────┘
```

---

# Practical guidance (don’t over-engineer prematurely)

## Start with:

1. RCU-style archive manager if profiling shows registry contention
2. Per-archive semaphore (NOT full pool yet)

```rust
Semaphore::new(2)
```

---

## Then add:

3. Tenant-level rate limiting

---

## Only add NUMA when:

- > 16 cores
- or multi-socket machine
- or profiling shows memory latency issues

---

# Failure modes to watch

### Multi-tenant isolation

- ❌ starvation if limits too strict
- ❌ unused capacity if too rigid

### Per-archive pools

- ❌ too many threads → context switching
- ❌ too few → underutilization

### NUMA

- ❌ wrong placement → worse than no NUMA
- ❌ debugging complexity increases a lot

---

# Bottom line

These features turn your system from:

> “fast ZIP server”

into:

> **a predictable, multi-tenant, high-throughput archive platform**

---

If you want to go deeper, the next meaningful step is:

- designing a **global scheduler** that coordinates tenants + archives + CPU
  cores (basically a mini kernel scheduler in user space)
