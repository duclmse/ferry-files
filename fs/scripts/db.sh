#!/usr/bin/env bash
# db.sh - inspect a filesearch/rar2zip SQLite database: list tables, show
# DDL, run queries, watch a query live, or get a quick health summary. Thin
# wrapper around the sqlite3 CLI (must be on PATH). The databases this
# project writes (filesearch's index.db, rar2zip's job queue) are ordinary
# WAL-mode SQLite files, so reading them read-only is safe alongside a
# running daemon.
#
# Usage:
#   scripts/db.sh [FLAGS] tables
#   scripts/db.sh [FLAGS] ddl [TABLE]
#   scripts/db.sh [FLAGS] stats
#   scripts/db.sh [FLAGS] query   "SELECT ..."
#   scripts/db.sh [FLAGS] explain "SELECT ..."
#   scripts/db.sh [FLAGS] recipe [NAME]
#   scripts/db.sh [FLAGS] watch   "SELECT ..."
#
# Flags:
#   -d, --db PATH        database file. If omitted, auto-discovers via
#                        data*/data/index.db - if exactly one match exists it
#                        is used, otherwise you're asked to pick with -d.
#   --json               query/explain/recipe output as JSON.
#   --csv                query/explain/recipe output as CSV.
#   --write              open the database read-write instead of -readonly.
#                        Only affects `query` - `tables`/`ddl` stay
#                        read-only regardless, since they never need to
#                        write. Use with care: this is a live index, not a
#                        scratch database.
#   -n, --interval SEC   poll interval for `watch` (default: 2).
#
# `ddl` with no TABLE dumps the schema for every table. `recipe` with no
# NAME lists the available canned queries.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

DB=""
FORMAT=""
WRITE=0
INTERVAL=2

ARGS=()
while [ $# -gt 0 ]; do
    case "$1" in
        -d|--db)
            shift
            DB="${1:?-d/--db requires a path}"
            ;;
        --json) FORMAT="json" ;;
        --csv)  FORMAT="csv" ;;
        --write) WRITE=1 ;;
        -n|--interval)
            shift
            INTERVAL="${1:?-n/--interval requires seconds}"
            ;;
        *)
            ARGS+=("$1")
            ;;
    esac
    shift
done

if [ -z "$DB" ]; then
    CANDIDATES=()
    while IFS= read -r c; do
        CANDIDATES+=("$c")
    done < <(compgen -G "data*/data/index.db" 2>/dev/null || true)
    if [ "${#CANDIDATES[@]}" -eq 1 ]; then
        DB="${CANDIDATES[0]}"
    elif [ "${#CANDIDATES[@]}" -gt 1 ]; then
        echo "multiple databases found; pick one with -d/--db:" >&2
        printf '  %s\n' "${CANDIDATES[@]}" >&2
        exit 1
    fi
fi

if ! command -v sqlite3 &>/dev/null; then
    echo "sqlite3 CLI not found on PATH - install it (e.g. 'brew install sqlite' / 'apt install sqlite3')" >&2
    exit 1
fi
if [ -z "$DB" ]; then
    echo "no database found; pass one with -d/--db PATH" >&2
    exit 1
fi
if [ ! -f "$DB" ]; then
    echo "database not found: $DB" >&2
    exit 1
fi

# Runs a SELECT/EXPLAIN through sqlite3 honoring --write/--json/--csv.
run_sql() {
    local sql="$1"
    local flags=()
    [ "$WRITE" -eq 0 ] && flags+=("-readonly")
    case "$FORMAT" in
        json) flags+=("-json") ;;
        csv)  flags+=("-csv" "-header") ;;
        *)    flags+=("-column" "-header") ;;
    esac
    sqlite3 "${flags[@]}" "$DB" "$sql"
}

CMD="${ARGS[0]:-}"
case "$CMD" in
    tables)
        sqlite3 -readonly "$DB" ".tables"
        ;;

    ddl)
        TABLE="${ARGS[1]:-}"
        if [ -n "$TABLE" ]; then
            sqlite3 -readonly "$DB" ".schema $TABLE"
        else
            sqlite3 -readonly "$DB" ".schema"
        fi
        ;;

    stats)
        SIZE="$(du -h "$DB" 2>/dev/null | awk '{print $1}')"
        PAGE_COUNT="$(sqlite3 -readonly "$DB" "PRAGMA page_count;")"
        FREELIST="$(sqlite3 -readonly "$DB" "PRAGMA freelist_count;")"
        echo "Database:  $DB"
        echo "File size: $SIZE"
        echo "Pages:     $PAGE_COUNT (freelist: $FREELIST)"
        echo ""
        printf "%-30s %14s\n" "TABLE" "ROWS"
        sqlite3 -readonly "$DB" "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name;" |
            while IFS= read -r t; do
                [ -z "$t" ] && continue
                n="$(sqlite3 -readonly "$DB" "SELECT COUNT(*) FROM \"$t\";")"
                printf "%-30s %14s\n" "$t" "$n"
            done
        ;;

    query)
        SQL="${ARGS[1]:-}"
        if [ -z "$SQL" ]; then
            echo "usage: scripts/db.sh [FLAGS] query \"SELECT ...\"" >&2
            exit 1
        fi
        run_sql "$SQL"
        ;;

    explain)
        SQL="${ARGS[1]:-}"
        if [ -z "$SQL" ]; then
            echo "usage: scripts/db.sh [FLAGS] explain \"SELECT ...\"" >&2
            exit 1
        fi
        run_sql "EXPLAIN QUERY PLAN $SQL"
        ;;

    recipe)
        NAME="${ARGS[1]:-}"
        case "$NAME" in
            "")
                echo "Available recipes:"
                echo "  archive-inner   rows indexed from inside an archive (archive_path IS NOT NULL)"
                echo "  stale           rows left behind by an older crawl_generation than the latest"
                exit 0
                ;;
            archive-inner)
                SQL="SELECT * FROM files WHERE archive_path IS NOT NULL LIMIT 50;"
                ;;
            stale)
                SQL="SELECT * FROM files WHERE crawl_generation IS NOT NULL AND crawl_generation < (SELECT MAX(crawl_generation) FROM files) LIMIT 50;"
                ;;
            *)
                echo "unknown recipe: $NAME (run 'recipe' with no name to list available recipes)" >&2
                exit 1
                ;;
        esac
        run_sql "$SQL"
        ;;

    watch)
        SQL="${ARGS[1]:-}"
        if [ -z "$SQL" ]; then
            echo "usage: scripts/db.sh [FLAGS] watch \"SELECT ...\" [-n SECONDS]" >&2
            exit 1
        fi
        while true; do
            if [ -t 1 ]; then clear; fi
            echo "== $(date '+%H:%M:%S')  every ${INTERVAL}s  ==  $SQL"
            echo ""
            run_sql "$SQL"
            sleep "$INTERVAL"
        done
        ;;

    *)
        echo "Usage: scripts/db.sh [FLAGS] {tables|ddl [TABLE]|stats|query \"SQL\"|explain \"SQL\"|recipe [NAME]|watch \"SQL\"}" >&2
        exit 1
        ;;
esac
