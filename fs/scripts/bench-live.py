"""Live-change benchmark: how fast and how completely each watcher backend
gets changes made while fsd runs into the index.

usage: python scripts/bench-live.py <fsd binary> [--backends notify,native,usn]
                                    [--dirs 40] [--files 1000] [--es es.exe]

Each backend gets a fresh daemon (memory store) watching a fresh folder under
the temp dir. Scenarios:

  latency       one new file at a time, until a search finds it (15 trials)
  tree create   --dirs new folders of --files files each
  tree delete   those folders removed with shutil.rmtree
  flat create   dirs*files files in one existing folder
  flat delete   those files removed one by one (no folder event to lean on)
  folder move   a folder of --files files renamed

"usn" needs an elevated console; without one it falls back to "native" and
the log says so. --es also polls a running Everything through es.exe for the
same changes (Everything must index the temp dir).
"""
import argparse
import json
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
import urllib.parse
import urllib.request
import uuid

ap = argparse.ArgumentParser()
ap.add_argument("fsd")
ap.add_argument("--backends", default="notify,native,usn")
ap.add_argument("--dirs", type=int, default=40)
ap.add_argument("--files", type=int, default=1000)
ap.add_argument("--port", type=int, default=17710)
ap.add_argument("--settle", type=float, default=15.0,
                help="seconds without progress before a scenario counts as stuck")
ap.add_argument("--buffer-kib", type=int, default=1024,
                help="watcher.buffer_kib; a small one forces overflows, to exercise recovery")
ap.add_argument("--es", help="es.exe, to compare with a running Everything")
args = ap.parse_args()

TOTAL = args.dirs * args.files
fwd = lambda p: p.replace("\\", "/")


class Daemon:
    def __init__(self, backend):
        self.work = tempfile.mkdtemp(prefix=f"fs-live-{backend}-")
        self.root = os.path.join(self.work, "root")
        os.makedirs(self.root)
        cfg = os.path.join(self.work, "config.toml")
        with open(cfg, "w") as f:
            f.write(f'''watch_paths = ["{fwd(self.root)}"]
data_dir = "{fwd(os.path.join(self.work, "data"))}"
log_dir = "{fwd(os.path.join(self.work, "logs"))}"

[store]
mode = "memory"

[watcher]
backend = "{backend}"
buffer_kib = {args.buffer_kib}

[archive_index]
enabled = false

[content.extension_limits]
"*" = 0

[search]
max_limit = 100000

[api]
rest_port = {args.port}
grpc_port = {args.port + 1}
grpc_enabled = false
ipc_enabled = false
''')
        self.log = os.path.join(self.work, "fsd.err.log")
        self.err = open(self.log, "w")
        self.proc = subprocess.Popen([args.fsd, "--config", cfg, "daemon"],
                                     stdout=subprocess.DEVNULL, stderr=self.err)
        t0 = time.time()
        while "full crawl complete" not in self.log_text():
            if self.proc.poll() is not None or time.time() - t0 > 60:
                raise SystemExit(f"fsd did not start; see {self.log}")
            time.sleep(0.1)
        time.sleep(1)

    def log_text(self):
        with open(self.log, encoding="utf-8", errors="replace") as f:
            return f.read()

    def total(self, q):
        url = f"http://127.0.0.1:{args.port}/search?q={urllib.parse.quote(q)}&limit=100000"
        try:
            with urllib.request.urlopen(url, timeout=30) as r:
                return int(json.load(r)["total"])
        except Exception:
            return -1

    def stop(self):
        self.proc.kill()
        self.proc.wait()
        self.err.close()


def cpu_seconds(proc):
    """User + kernel CPU time of a running process (Windows only)."""
    if os.name != "nt":
        return None
    import ctypes
    from ctypes import wintypes
    k = ctypes.windll.kernel32
    h = k.OpenProcess(0x1000, False, proc.pid)  # PROCESS_QUERY_LIMITED_INFORMATION
    if not h:
        return None
    t = [wintypes.FILETIME() for _ in range(4)]
    ok = k.GetProcessTimes(h, *[ctypes.byref(x) for x in t])
    k.CloseHandle(h)
    if not ok:
        return None
    ft = lambda f: ((f.dwHighDateTime << 32) | f.dwLowDateTime) / 1e7
    return ft(t[2]) + ft(t[3])


def es_count(*q):
    r = subprocess.run([args.es, "-get-result-count", *q], capture_output=True, text=True)
    try:
        return int(r.stdout.strip())
    except ValueError:
        return -1


def follow(probe, want):
    """Seconds until probe() == want, or ('stuck', last value)."""
    t = time.perf_counter()
    last, last_change = None, t
    while True:
        n = probe()
        if n != last:
            last, last_change = n, time.perf_counter()
        if n == want:
            return f"{time.perf_counter() - t:.1f} s"
        if time.perf_counter() - last_change > args.settle:
            return f"stuck at {last:,} of {want:,}" if want else f"{last:,} stale"
        time.sleep(0.25)


def scenarios(d, probe_name, es=False):
    """Runs every scenario against daemon `d`; returns {scenario: result}."""
    root = d.root
    out = {}
    count = (lambda *q: es_count("-path", root, *q)) if es else None

    lat = []
    for _ in range(15):
        name = f"lat_{uuid.uuid4().hex[:12]}.txt"
        with open(os.path.join(root, name), "w") as f:
            f.write("x")
        made = time.perf_counter()
        while time.perf_counter() - made < 10:
            if (count(name) if es else d.total(name)) > 0:
                lat.append((time.perf_counter() - made) * 1000)
                break
            time.sleep(0.005)
        time.sleep(0.3)
    out["latency"] = (f"median {statistics.median(lat):.0f} ms, max {max(lat):.0f}"
                      + (f", {15 - len(lat)} missed" if len(lat) < 15 else "")) if lat else "never"

    dirs = [os.path.join(root, f"d{i:03}") for i in range(args.dirs)]
    if es:
        tree = lambda: count("file:", "tree_")
        flat = lambda: count("file:", "flat_")
    else:
        tree = lambda: max(d.total("tree_"), 0)
        flat = lambda: max(d.total("flat_"), 0)

    for x in dirs:
        os.mkdir(x)
        for i in range(args.files):
            with open(os.path.join(x, f"tree_{i:05}.txt"), "w") as f:
                f.write("x")
    out["tree create"] = follow(tree, TOTAL)

    for x in dirs:
        shutil.rmtree(x)
    out["tree delete"] = follow(tree, 0)

    fl = os.path.join(root, "flat")
    os.mkdir(fl)
    time.sleep(2)
    for i in range(TOTAL):
        with open(os.path.join(fl, f"flat_{i:06}.txt"), "w") as f:
            f.write("x")
    out["flat create"] = follow(flat, TOTAL)

    for i in range(TOTAL):
        os.remove(os.path.join(fl, f"flat_{i:06}.txt"))
    out["flat delete"] = follow(flat, 0)

    mv = os.path.join(root, "mv_src")
    os.mkdir(mv)
    for i in range(args.files):
        with open(os.path.join(mv, f"moved_{i:05}.txt"), "w") as f:
            f.write("x")
    time.sleep(3)
    os.rename(mv, os.path.join(root, "mv_dst"))
    if es:
        moved = lambda: count("file:", os.path.join(root, "mv_dst") + "\\")
    else:
        moved = lambda: max(d.total(f"parent:{os.path.join(root, 'mv_dst')}"), 0)
    out["folder move"] = follow(moved, args.files)
    return out


results = {}
notes = {}
for backend in args.backends.split(","):
    print(f"== {backend}", flush=True)
    d = Daemon(backend)
    try:
        cpu0 = cpu_seconds(d.proc)
        results[backend] = scenarios(d, backend)
        cpu1 = cpu_seconds(d.proc)
        # Idle: what following the volume costs with nothing changing here.
        time.sleep(10)
        cpu2 = cpu_seconds(d.proc)
        if cpu0 is not None:
            results[backend]["fsd CPU, scenarios"] = f"{cpu1 - cpu0:.1f} s"
            results[backend]["fsd CPU, 10 s idle"] = f"{(cpu2 - cpu1) * 1000:.0f} ms"
        log = d.log_text()
        notes[backend] = (f"{log.count('watcher lost changes')} recovery scans; "
                          + ("USN journal" if "following the USN journal" in log else
                             "ReadDirectoryChangesW" if "(Native" in log or "(Usn" in log
                             else "notify"))
        for k, v in results[backend].items():
            print(f"  {k:12} {v}", flush=True)
    finally:
        d.stop()
        print(f"  log: {d.log}")
if args.es:
    print("== everything", flush=True)
    d = Daemon("notify")  # only for its temp root; Everything does the watching
    try:
        if es_count("-path", d.root) < 0:
            print("  es.exe does not answer; skipped")
        else:
            results["everything"] = scenarios(d, "everything", es=True)
            results["everything"]["fsd CPU, scenarios"] = "-"
            results["everything"]["fsd CPU, 10 s idle"] = "-"
            notes["everything"] = "polled through es.exe (~36 ms per poll)"
    finally:
        d.stop()

names = list(results)
rows = list(next(iter(results.values())).keys()) if results else []
print()
print("| scenario | " + " | ".join(names) + " |")
print("|---|" + "---|" * len(names))
for r in rows:
    print(f"| {r} | " + " | ".join(results[n][r] for n in names) + " |")
for n in names:
    print(f"- {n}: {notes.get(n, '')}")
print(f"\n{args.dirs} folders x {args.files} files = {TOTAL:,} files per burst")
