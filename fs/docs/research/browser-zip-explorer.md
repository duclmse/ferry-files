# Browser ZIP Explorer Design Note

> **Status:** research note. Client-side (browser) ZIP explorer ideas: packed
> inode-style index, string pool, lazy loading. The server-side counterparts
> are `research/zidx.md` and `research/backend-design.md`. Not a spec.

## 2. High-performance ZIP explorer architecture (what you actually asked)

You’re now describing something closer to a **filesystem engine in the browser**. Below is a precise breakdown of each component and how they fit together.

---

# A. Memory-mapped ZIP index (inode-style)

### Problem

`zip.js` gives you a flat list of entries. Repeated string splitting + traversal is expensive.

### Solution

Build a **compact index table**, not a tree:

```ts
struct Node {
  nameOffset: u32
  nameLength: u16
  parent: u32
  firstChild: u32
  nextSibling: u32
  entryIndex: i32
}
```

### Backing storage

* `ArrayBuffer` → all nodes packed
* `Uint32Array` / `Uint16Array` views
* Single `Uint8Array` for string pool

### Why this matters

* No JS objects → avoids GC pauses
* Cache-friendly sequential scans
* Traversal = pointer arithmetic (like inode table)

---

# B. WASM unzip engine (libzip / miniz)

### Why not rely fully on zip.js?

* JS inflation is slower
* No SIMD / low-level memory control
* Limited streaming control

### Approach

Compile:

* miniz or libzip → WASM

Pipeline:

```
[ZIP Blob]
   ↓ (ArrayBuffer slice)
[WASM unzip]
   ↓ (Uint8Array view)
[UI / Preview]
```

### Key optimization

* **Central directory parsing in JS**
* **Decompression in WASM**

Best of both worlds:

* Fast indexing
* Fast inflate

---

# C. SharedArrayBuffer (zero-copy IPC)

### Problem

Worker ↔ Main thread copies kill performance for large data

### Solution

Use:

```js
const sab = new SharedArrayBuffer(size);
```

### Layout example

```
| Node Table | String Pool | File Cache | Scratch |
```

Both threads:

```js
const nodes = new Uint32Array(sab, 0, nodeCount * NODE_SIZE);
```

### Requirements

You MUST enable cross-origin isolation:

```http
Cross-Origin-Opener-Policy: same-origin
Cross-Origin-Embedder-Policy: require-corp
```

Otherwise `SharedArrayBuffer` is unavailable.

---

# D. Column diffing (not rebuild)

### Current problem

You’re doing:

```js
finder.innerHTML = ""
```

This is catastrophic at scale.

### Correct approach

Maintain:

```js
columns: Array<{
  nodeId: number
  dom: HTMLElement
}>
```

Then diff:

```js
function updateColumns(newPath) {
  let i = 0;

  while (i < columns.length && i < newPath.length &&
         columns[i].nodeId === newPath[i]) {
    i++;
  }

  // remove only changed tail
  for (let j = columns.length - 1; j >= i; j--) {
    columns[j].dom.remove();
    columns.pop();
  }

  // append new columns
  for (let j = i; j < newPath.length; j++) {
    const col = createColumn(newPath[j]);
    finder.appendChild(col);
    columns.push({ nodeId: newPath[j], dom: col });
  }
}
```

### Result

* O(depth) updates instead of O(total DOM)
* No layout thrash
* Smooth navigation even at scale

---

# E. SIMD hex rendering

### Problem

Hex conversion is CPU-heavy:

```js
b.toString(16)
```

### Better approach

Precompute lookup:

```js
const LUT = new Array(256);
for (let i = 0; i < 256; i++) {
  LUT[i] = i.toString(16).padStart(2, "0");
}
```

Then:

```js
hex += LUT[b]
```

---

### WASM SIMD (next level)

If you go serious:

* Use WASM SIMD (`v128`)
* Process 16 bytes per instruction
* Emit ASCII hex via vectorized lookup

This gives ~5–10x speedup on large buffers.

---

# F. End-to-end data flow

```
          (Main Thread)
                │
        file input (Blob)
                │
                ▼
        Worker: parse ZIP
                │
        build inode table (SAB)
                │
                ▼
     Main thread reads SAB directly
                │
     Column UI (virtualized + diffed)
                │
         user selects file
                │
                ▼
     WASM unzip (worker or main)
                │
        Shared buffer output
                │
                ▼
     Preview (text / image / hex SIMD)
```

---

# G. Real bottlenecks (you should expect)

Even with all this:

### 1. Central directory parsing

Still O(N), unavoidable

### 2. String memory

Filenames dominate memory, not nodes

### 3. Browser limits

* ~1–2GB heap cap
* SAB fragmentation

---

# H. What you actually end up building

This is no longer a “viewer”.

It’s effectively:

* a **read-only virtual filesystem**
* with:

  * inode table
  * block decoding (WASM)
  * UI projection layer

Very similar to how:

* Visual Studio Code handles large workspaces
* or how OS file explorers work internally

---

## If you want to go further

The next meaningful leap would be:

* Incremental ZIP parsing (stream central directory backwards)
* Persistent index cache (IndexedDB)
* Multi-threaded decompression pool
* GPU-backed rendering (WebGL for hex view)

If you want, I can help you design this as a **modular system (worker pool + FS core + UI renderer)** instead of a monolithic script.
