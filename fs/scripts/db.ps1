#Requires -Version 5.1
# db.ps1 - inspect a fs/rar2zip SQLite database: list tables, show
# DDL, run queries, watch a query live, or get a quick health summary. Thin
# wrapper around the sqlite3 CLI (must be on PATH). The databases this
# project writes (fs's index.db, rar2zip's job queue) are ordinary
# WAL-mode SQLite files, so reading them read-only is safe alongside a
# running daemon.
#
# Usage:
#   scripts\db.ps1 tables            [-Db <path>]
#   scripts\db.ps1 ddl   [TABLE]      [-Db <path>]
#   scripts\db.ps1 stats             [-Db <path>]
#   scripts\db.ps1 query   "SELECT ..." [-Db <path>] [-Json|-Csv] [-Write]
#   scripts\db.ps1 explain "SELECT ..." [-Db <path>] [-Json|-Csv]
#   scripts\db.ps1 recipe [NAME]      [-Db <path>] [-Json|-Csv]
#   scripts\db.ps1 watch   "SELECT ..." [-Db <path>] [-Json|-Csv] [-IntervalSec <n>]
#
# -Db defaults to auto-discovery via data*\data\index.db - if exactly one
# match exists it's used, otherwise pass one explicitly. -Write opens the
# database read-write instead of -readonly; it only affects `query` - use
# with care, this is a live index, not a scratch database. `ddl` with no
# TABLE dumps the schema for every table. `recipe` with no NAME lists the
# available canned queries.
param(
    [Parameter(Position = 0, Mandatory = $true)]
    [ValidateSet("tables", "ddl", "stats", "query", "explain", "recipe", "watch")]
    [string]$Command,

    [Parameter(Position = 1)]
    [string]$Argument = "",

    [string]$Db = "",
    [switch]$Json,
    [switch]$Csv,
    [switch]$Write,
    [int]$IntervalSec = 2
)

$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

if (-not $Db) {
    $candidates = @(Get-ChildItem -Path . -Filter "index.db" -Recurse -Depth 2 -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match '[\\/]data[^\\/]*[\\/]data[\\/]index\.db$' } |
        ForEach-Object { $_.FullName })
    if ($candidates.Count -eq 1) {
        $Db = $candidates[0]
    } elseif ($candidates.Count -gt 1) {
        Write-Host "multiple databases found; pick one with -Db:"
        $candidates | ForEach-Object { Write-Host "  $_" }
        exit 1
    }
}

if (-not (Get-Command sqlite3 -ErrorAction SilentlyContinue)) {
    Write-Error "sqlite3 CLI not found on PATH - install it from https://sqlite.org/download.html (or 'choco install sqlite')"
    exit 1
}
if (-not $Db) {
    Write-Error "no database found; pass one with -Db <path>"
    exit 1
}
if (-not (Test-Path $Db)) {
    Write-Error "database not found: $Db"
    exit 1
}

function Invoke-Sql($sql) {
    $flags = @()
    if (-not $Write) { $flags += "-readonly" }
    if ($Json) { $flags += "-json" }
    elseif ($Csv) { $flags += "-csv", "-header" }
    else { $flags += "-column", "-header" }
    & sqlite3 @flags $Db $sql
}

switch ($Command) {
    "tables" {
        & sqlite3 -readonly $Db ".tables"
    }

    "ddl" {
        if ($Argument) {
            & sqlite3 -readonly $Db ".schema $Argument"
        } else {
            & sqlite3 -readonly $Db ".schema"
        }
    }

    "stats" {
        $size = "{0:N1} MB" -f ((Get-Item $Db).Length / 1MB)
        $pageCount = & sqlite3 -readonly $Db "PRAGMA page_count;"
        $freelist  = & sqlite3 -readonly $Db "PRAGMA freelist_count;"
        Write-Host "Database:  $Db"
        Write-Host "File size: $size"
        Write-Host "Pages:     $pageCount (freelist: $freelist)"
        Write-Host ""
        Write-Host ("{0,-30} {1,14}" -f "TABLE", "ROWS")
        $tables = & sqlite3 -readonly $Db "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name;"
        foreach ($t in $tables) {
            if (-not $t) { continue }
            $n = & sqlite3 -readonly $Db "SELECT COUNT(*) FROM `"$t`";"
            Write-Host ("{0,-30} {1,14}" -f $t, $n)
        }
    }

    "query" {
        if (-not $Argument) {
            Write-Error "usage: scripts\db.ps1 query `"SELECT ...`" [-Db <path>]"
            exit 1
        }
        Invoke-Sql $Argument
    }

    "explain" {
        if (-not $Argument) {
            Write-Error "usage: scripts\db.ps1 explain `"SELECT ...`" [-Db <path>]"
            exit 1
        }
        Invoke-Sql "EXPLAIN QUERY PLAN $Argument"
    }

    "recipe" {
        $recipes = @{
            "archive-inner" = "SELECT * FROM files WHERE archive_path IS NOT NULL LIMIT 50;"
            "stale"         = "SELECT * FROM files WHERE crawl_generation IS NOT NULL AND crawl_generation < (SELECT MAX(crawl_generation) FROM files) LIMIT 50;"
        }
        if (-not $Argument) {
            Write-Host "Available recipes:"
            Write-Host "  archive-inner   rows indexed from inside an archive (archive_path IS NOT NULL)"
            Write-Host "  stale           rows left behind by an older crawl_generation than the latest"
            exit 0
        }
        if (-not $recipes.ContainsKey($Argument)) {
            Write-Error "unknown recipe: $Argument (run 'recipe' with no name to list available recipes)"
            exit 1
        }
        Invoke-Sql $recipes[$Argument]
    }

    "watch" {
        if (-not $Argument) {
            Write-Error "usage: scripts\db.ps1 watch `"SELECT ...`" [-Db <path>] [-IntervalSec <n>]"
            exit 1
        }
        while ($true) {
            Clear-Host
            Write-Host "== $(Get-Date -Format 'HH:mm:ss')  every ${IntervalSec}s  ==  $Argument"
            Write-Host ""
            Invoke-Sql $Argument
            Start-Sleep -Seconds $IntervalSec
        }
    }
}
