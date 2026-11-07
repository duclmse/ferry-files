#!/usr/bin/env bash
# End-to-end crawl benchmark: builds (if needed), runs a real daemon against a
# real directory tree in an isolated scratch config, times the initial full
# crawl via GET /stats, and reports throughput.
#
# Usage:
#   scripts/bench.sh [TARGET_DIR] [--with-content] [--keep-db] [--data-dir DIR]
#
# TARGET_DIR defaults to the repo root. --with-content turns on full-text
# content extraction for common source extensions; by default it's off, which
# isolates pure filesystem-crawl throughput (stat + SQLite upsert only).
# Archive-inner-file indexing is always off, so results are comparable across
# runs regardless of how many archives happen to live under TARGET_DIR.
#
# --keep-db leaves the scratch config/data/logs directory in place instead of
# deleting it on exit (its path is printed at the end). --data-dir DIR uses a
# specific directory instead of a fresh mktemp one (implies --keep-db); reuse
# the same DIR across runs to re-crawl on top of an existing index. Either
# way, the kept directory's config.toml can be fed straight into
# scripts/bench-query.sh to benchmark query latency against that data.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
REPO_ROOT="$(pwd)"

TARGET_DIR="$REPO_ROOT"
WITH_CONTENT=0
KEEP_DB=0
DATA_DIR_ARG=""
while [ $# -gt 0 ]; do
    case "$1" in
        --with-content) WITH_CONTENT=1 ;;
        --keep-db)      KEEP_DB=1 ;;
        --data-dir)
            shift
            DATA_DIR_ARG="${1:?--data-dir requires a path}"
            KEEP_DB=1
            ;;
        -*) echo "unknown flag: $1" >&2; exit 1 ;;
        *)  TARGET_DIR="$1" ;;
    esac
    shift
done
TARGET_DIR="$(cd "$TARGET_DIR" && pwd)"

if [ ! -x "target/release/filesearch" ]; then
    echo "==> Building release binary first"
    ./scripts/build.sh
fi
BIN="$REPO_ROOT/target/release/filesearch"

if [ -n "$DATA_DIR_ARG" ]; then
    mkdir -p "$DATA_DIR_ARG"
    WORKDIR="$(cd "$DATA_DIR_ARG" && pwd)"
else
    WORKDIR="$(mktemp -d)"
fi
CONFIG="$WORKDIR/config.toml"
mkdir -p "$WORKDIR/data" "$WORKDIR/logs"

REST_PORT=17700
GRPC_PORT=17701

CONTENT_TOML=""
if [ "$WITH_CONTENT" -eq 1 ]; then
    CONTENT_TOML='
[content.extension_limits]
rs = 524288
py = 524288
js = 262144
ts = 262144
go = 524288
md = 131072
txt = 131072
"*" = 32768
'
fi

cat > "$CONFIG" <<EOF
watch_paths = ["$TARGET_DIR"]
data_dir    = "$WORKDIR/data"
log_dir     = "$WORKDIR/logs"

[archive_index]
enabled = false

[api]
rest_port    = $REST_PORT
grpc_port    = $GRPC_PORT
grpc_enabled = false
ipc_enabled  = false
$CONTENT_TOML
EOF

echo "==> Validating config"
"$BIN" --config "$CONFIG" validate-config

DAEMON_PID=""
cleanup() {
    if [ -n "$DAEMON_PID" ] && kill -0 "$DAEMON_PID" 2>/dev/null; then
        kill -TERM "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
    fi
    [ "$KEEP_DB" -eq 1 ] || rm -rf "$WORKDIR"
}
trap cleanup EXIT INT TERM

echo "==> Target:  $TARGET_DIR"
echo "==> Content: $([ "$WITH_CONTENT" -eq 1 ] && echo enabled || echo disabled)"
echo "==> Starting daemon"

START_EPOCH=$(date +%s)
"$BIN" --config "$CONFIG" daemon > "$WORKDIR/daemon.log" 2>&1 &
DAEMON_PID=$!

for _ in $(seq 1 50); do
    if curl -s -o /dev/null "http://127.0.0.1:$REST_PORT/health"; then
        break
    fi
    sleep 0.2
done

# daemon.rs skips the initial crawl entirely when the data dir already has a
# built index (see "needs_crawl" there), which is exactly the --data-dir
# reuse case this script documents. Detect that from the daemon's own log and
# force a fresh crawl via the reindex API so there's actually something to
# time; otherwise last_crawl_secs never gets set and the wait below hangs
# until timeout.
for _ in $(seq 1 20); do
    if grep -q "skipping initial crawl\|full crawl starting" "$WORKDIR/daemon.log" 2>/dev/null; then
        break
    fi
    sleep 0.5
done
if grep -q "skipping initial crawl" "$WORKDIR/daemon.log" 2>/dev/null; then
    echo "==> Existing index found; forcing a fresh crawl via /reindex to benchmark"
    START_EPOCH=$(date +%s)
    curl -s -X POST -H 'Content-Type: application/json' -d '{"full": true}' \
        "http://127.0.0.1:$REST_PORT/reindex" -o /dev/null
fi

echo "==> Waiting for full crawl to finish (timeout 600s)"
STATS=""
LAST_CRAWL=0
for _ in $(seq 1 600); do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "daemon exited unexpectedly; log follows:" >&2
        cat "$WORKDIR/daemon.log" >&2
        exit 1
    fi
    STATS="$(curl -s "http://127.0.0.1:$REST_PORT/stats")"
    LAST_CRAWL="$(printf '%s' "$STATS" | sed -n 's/.*"last_crawl_secs":\([0-9]*\).*/\1/p')"
    if [ -n "$LAST_CRAWL" ] && [ "$LAST_CRAWL" != "0" ]; then
        break
    fi
    sleep 1
done

if [ -z "$LAST_CRAWL" ] || [ "$LAST_CRAWL" = "0" ]; then
    echo "crawl did not finish within timeout" >&2
    exit 1
fi

ELAPSED=$((LAST_CRAWL - START_EPOCH))
[ "$ELAPSED" -lt 1 ] && ELAPSED=1

TOTAL_FILES="$(printf '%s' "$STATS" | sed -n 's/.*"total_files":\([0-9]*\).*/\1/p')"
TOTAL_DIRS="$(printf '%s' "$STATS" | sed -n 's/.*"total_dirs":\([0-9]*\).*/\1/p')"
CONTENT_INDEXED="$(printf '%s' "$STATS" | sed -n 's/.*"content_indexed":\([0-9]*\).*/\1/p')"

FILES_PER_SEC=$(( TOTAL_FILES / ELAPSED ))
ENTRIES_PER_SEC=$(( (TOTAL_FILES + TOTAL_DIRS) / ELAPSED ))

echo ""
echo "==================== Benchmark Results ===================="
echo "Target directory:   $TARGET_DIR"
echo "Content extraction: $([ "$WITH_CONTENT" -eq 1 ] && echo enabled || echo disabled)"
echo "Elapsed:            ${ELAPSED}s"
echo "Files indexed:      $TOTAL_FILES"
echo "Dirs indexed:       $TOTAL_DIRS"
echo "Content indexed:    $CONTENT_INDEXED"
echo "Throughput:         ${FILES_PER_SEC} files/sec, ${ENTRIES_PER_SEC} entries/sec"
if [ "$KEEP_DB" -eq 1 ]; then
    echo "Data dir kept at:   $WORKDIR"
    echo "Query it with:      scripts/bench-query.sh $WORKDIR"
fi
echo "=============================================================="
