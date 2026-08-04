#!/usr/bin/env bash
# Query-latency benchmark: reuses a data directory produced by
# `scripts/bench.sh --keep-db` (a config.toml plus its already-crawled
# sqlite/tantivy index), starts a daemon against it, waits for the daemon's
# startup (re)crawl to finish, then times a fixed set of queries chosen to
# cover each evaluator strategy (glob, size, date, ext, fuzzy, and content
# when the index has one) via GET /search.
#
# Usage:
#   scripts/bench-query.sh DATA_DIR [ITERATIONS]
#
# DATA_DIR must contain a config.toml (as written by bench.sh --keep-db or
# bench.sh --data-dir DIR). ITERATIONS defaults to 20 per query.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
REPO_ROOT="$(pwd)"

DATA_DIR="${1:?usage: scripts/bench-query.sh DATA_DIR [ITERATIONS]}"
ITERATIONS="${2:-20}"
DATA_DIR="$(cd "$DATA_DIR" && pwd)"
CONFIG="$DATA_DIR/config.toml"

if [ ! -f "$CONFIG" ]; then
    echo "no config.toml found in $DATA_DIR (run scripts/bench.sh --keep-db first)" >&2
    exit 1
fi

if [ ! -x "target/release/filesearch" ]; then
    echo "==> Building release binary first"
    ./scripts/build.sh
fi
BIN="$REPO_ROOT/target/release/filesearch"

REST_PORT="$(sed -n 's/^rest_port *= *\([0-9]*\).*/\1/p' "$CONFIG" | head -1)"
REST_PORT="${REST_PORT:-17700}"

# Pure-bash percent-encoding; the fixed query list below only ever uses
# ASCII, but this keeps the script correct if queries are edited later.
urlencode() {
    local s="$1" out="" i c
    for (( i = 0; i < ${#s}; i++ )); do
        c="${s:$i:1}"
        case "$c" in
            [a-zA-Z0-9.~_-]) out+="$c" ;;
            *) out+=$(printf '%%%02X' "'$c") ;;
        esac
    done
    printf '%s' "$out"
}

echo "==> Validating config"
"$BIN" --config "$CONFIG" validate-config

DAEMON_PID=""
cleanup() {
    if [ -n "$DAEMON_PID" ] && kill -0 "$DAEMON_PID" 2>/dev/null; then
        kill -TERM "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

echo "==> Starting daemon against existing data dir: $DATA_DIR"
"$BIN" --config "$CONFIG" daemon > "$DATA_DIR/daemon.query-bench.log" 2>&1 &
DAEMON_PID=$!

for _ in $(seq 1 50); do
    if curl -s -o /dev/null "http://127.0.0.1:$REST_PORT/health"; then
        break
    fi
    sleep 0.2
done

# total_files/last_crawl_secs in /stats are in-process atomics that reset to 0
# on every daemon start and are only ever written by a live crawl. Against an
# already-warm data dir (the normal case here), daemon.rs's "needs_crawl" check
# skips the crawl entirely, so those atomics stay 0 forever even though the
# persisted sqlite/tantivy state is already fully queryable. The only reliable
# readiness signal is which branch the daemon actually logged: either it skipped
# the crawl (ready as soon as it logged that), or it ran one (ready once it logs completion).
echo "==> Waiting for the daemon to be ready to serve queries (timeout 600s)"
READY=0
for _ in $(seq 1 600); do
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "daemon exited unexpectedly; log follows:" >&2
        cat "$DATA_DIR/daemon.query-bench.log" >&2
        exit 1
    fi
    if grep -q "skipping initial crawl" "$DATA_DIR/daemon.query-bench.log" 2>/dev/null; then
        READY=1
        break
    fi
    if grep -q "full crawl complete" "$DATA_DIR/daemon.query-bench.log" 2>/dev/null; then
        READY=1
        break
    fi
    sleep 1
done
if [ "$READY" -ne 1 ]; then
    echo "daemon did not become ready within timeout; log follows:" >&2
    cat "$DATA_DIR/daemon.query-bench.log" >&2
    exit 1
fi

STATS="$(curl -s "http://127.0.0.1:$REST_PORT/stats")"
CONTENT_INDEXED="$(printf '%s' "$STATS" | sed -n 's/.*"content_indexed":\([0-9]*\).*/\1/p')"
TOTAL_FILES="$(printf '%s' "$STATS" | sed -n 's/.*"total_files":\([0-9]*\).*/\1/p')"
if [ -z "$TOTAL_FILES" ] || [ "$TOTAL_FILES" = "0" ]; then
    # 0 here just means this process didn't run a crawl (warm-started against
    # an existing index) - total_files is an in-process counter only ever
    # written by a live crawl, not read back from the persisted index. A
    # /search total isn't a substitute: it's capped by config search.max_limit
    # (10000 by default), so it silently undercounts larger indices.
    TOTAL_FILES="unknown (warm-started; see search hits below for scale)"
fi

QUERIES=(
    "glob:*.rs|*.rs"
    "glob:*.toml|*.toml"
    "size:>1mb|size:>1mb"
    "date:today|dm:today"
    "ext:rs|ext:rs"
    "fuzzy:main|fuzzy:main"
    "content:fn|content:fn"
)

echo ""
echo "==> Index: $TOTAL_FILES files, content_indexed=$CONTENT_INDEXED"
echo "==================== Query Benchmark (${ITERATIONS} iters each) ===================="
printf "%-14s %8s %8s %8s %10s %8s\n" "query" "min_ms" "avg_ms" "max_ms" "server_ms" "hits"

for entry in "${QUERIES[@]}"; do
    name="${entry%%|*}"
    q="${entry#*|}"
    encoded_q="$(urlencode "$q")"

    min=-1
    max=0
    total=0
    server_total=0
    hits=0
    for _ in $(seq 1 "$ITERATIONS"); do
        start_ns=$(date +%s%N)
        resp="$(curl -s "http://127.0.0.1:$REST_PORT/search?q=$encoded_q&limit=50")"
        end_ns=$(date +%s%N)
        elapsed_ms=$(( (end_ns - start_ns) / 1000000 ))

        [ "$min" -eq -1 ] && min=$elapsed_ms
        [ "$elapsed_ms" -lt "$min" ] && min=$elapsed_ms
        [ "$elapsed_ms" -gt "$max" ] && max=$elapsed_ms
        total=$(( total + elapsed_ms ))

        took="$(printf '%s' "$resp" | sed -n 's/.*"took_ms":\([0-9]*\).*/\1/p')"
        server_total=$(( server_total + ${took:-0} ))
        hits="$(printf '%s' "$resp" | sed -n 's/.*"total":\([0-9]*\).*/\1/p')"
    done
    avg=$(( total / ITERATIONS ))
    server_avg=$(( server_total / ITERATIONS ))

    printf "%-14s %8s %8s %8s %10s %8s\n" "$name" "${min}" "${avg}" "${max}" "${server_avg}" "${hits:-0}"
done
echo "==================================================================================="
