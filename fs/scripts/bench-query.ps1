#Requires -Version 5.1
# Query-latency benchmark: reuses a data directory produced by
# scripts\bench.ps1 -KeepDb (a config.toml plus its already-crawled
# sqlite/tantivy index), starts a daemon against it, waits for the daemon's
# startup (re)crawl to finish, then times a fixed set of queries chosen to
# cover each evaluator strategy (glob, size, date, ext, fuzzy, and content
# when the index has one) via GET /search.
#
# Usage:
#   scripts\bench-query.ps1 DataDir [Iterations]
#
# DataDir must contain a config.toml (as written by bench.ps1 -KeepDb or
# bench.ps1 -DataDir). Iterations defaults to 20 per query.
param(
    [Parameter(Mandatory = $true)]
    [string]$DataDir,
    [int]$Iterations = 20
)

$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")
$repoRoot = (Get-Location).Path

$DataDir = (Resolve-Path $DataDir).Path
$config  = Join-Path $DataDir "config.toml"
if (-not (Test-Path $config)) {
    Write-Error "no config.toml found in $DataDir (run scripts\bench.ps1 -KeepDb first)"
    exit 1
}

if (-not (Test-Path "target\release\fsd.exe")) {
    Write-Host "==> Building release binary first"
    & .\scripts\build.ps1
}
$bin = Join-Path $repoRoot "target\release\fsd.exe"

$restPortLine = Select-String -Path $config -Pattern '^rest_port\s*=\s*(\d+)' | Select-Object -First 1
$restPort = if ($restPortLine) { $restPortLine.Matches[0].Groups[1].Value } else { 17700 }

Write-Host "==> Validating config"
& $bin --config $config validate-config
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host "==> Starting daemon against existing data dir: $DataDir"
$proc = Start-Process -FilePath $bin -ArgumentList "--config", $config, "daemon" `
    -RedirectStandardOutput "$DataDir\daemon.query-bench.out.log" `
    -RedirectStandardError  "$DataDir\daemon.query-bench.err.log" `
    -PassThru -WindowStyle Hidden

try {
    $healthUrl = "http://127.0.0.1:$restPort/health"
    $up = $false
    for ($i = 0; $i -lt 50; $i++) {
        try { Invoke-RestMethod -Uri $healthUrl -TimeoutSec 1 | Out-Null; $up = $true; break }
        catch { Start-Sleep -Milliseconds 200 }
    }
    if (-not $up) { throw "daemon did not come up; see $DataDir\daemon.query-bench.err.log" }

    # /stats cannot tell a finished crawl from a kept index: both report a
    # last crawl time and counts from the start. The only reliable readiness
    # signal is which branch the daemon actually logged: either it skipped
    # the crawl (ready as soon as it logged that), or it ran one (ready once
    # it logs completion).
    Write-Host "==> Waiting for the daemon to be ready to serve queries (timeout 600s)"
    # The daemon logs to stderr; stdout stays empty.
    $logPath = "$DataDir\daemon.query-bench.err.log"
    $ready = $false
    for ($i = 0; $i -lt 600; $i++) {
        if ($proc.HasExited) {
            Get-Content "$DataDir\daemon.query-bench.err.log" | Write-Host
            throw "daemon exited unexpectedly"
        }
        if ((Test-Path $logPath) -and (Select-String -Path $logPath -Pattern "catch-up: |index already built|full crawl complete" -Quiet)) {
            $ready = $true
            break
        }
        Start-Sleep -Seconds 1
    }
    if (-not $ready) {
        Get-Content $logPath -ErrorAction SilentlyContinue | Write-Host
        throw "daemon did not become ready within timeout"
    }

    $stats = Invoke-RestMethod -Uri "http://127.0.0.1:$restPort/stats" -TimeoutSec 5

    # total_files is counted from the index shortly after start, so 0 means
    # that count has not run yet. A /search total isn't a substitute: it's
    # capped by config search.max_limit (10000 by default), so it silently
    # undercounts larger indices.
    $totalFilesDisplay = if ($stats.total_files -gt 0) { $stats.total_files } else { "unknown (not counted yet; see search hits below for scale)" }

    $queries = [ordered]@{
        "glob:*.rs"    = "*.rs"
        "glob:*.toml"  = "*.toml"
        "size:>1mb"    = "size:>1mb"
        "date:today"   = "dm:today"
        "ext:rs"       = "ext:rs"
        "fuzzy:main"   = "fuzzy:main"
        "content:fn"   = "content:fn"
    }

    Write-Host ""
    Write-Host "==> Index: $totalFilesDisplay files, content_indexed=$($stats.content_indexed)"
    Write-Host "==================== Query Benchmark ($Iterations iters each) ===================="
    "{0,-14} {1,8} {2,8} {3,8} {4,10} {5,8}" -f "query", "min_ms", "avg_ms", "max_ms", "server_ms", "hits" | Write-Host

    foreach ($name in $queries.Keys) {
        $q = [uri]::EscapeDataString($queries[$name])
        $timesMs = @()
        $serverMs = @()
        $hits = 0
        for ($i = 0; $i -lt $Iterations; $i++) {
            $sw = [System.Diagnostics.Stopwatch]::StartNew()
            $resp = Invoke-RestMethod -Uri "http://127.0.0.1:$restPort/search?q=$q&limit=50" -TimeoutSec 30
            $sw.Stop()
            $timesMs  += $sw.ElapsedMilliseconds
            $serverMs += [int64]$resp.took_ms
            $hits = $resp.total
        }
        $min = ($timesMs | Measure-Object -Minimum).Minimum
        $avg = [math]::Round(($timesMs | Measure-Object -Average).Average)
        $max = ($timesMs | Measure-Object -Maximum).Maximum
        $serverAvg = [math]::Round(($serverMs | Measure-Object -Average).Average)
        "{0,-14} {1,8} {2,8} {3,8} {4,10} {5,8}" -f $name, $min, $avg, $max, $serverAvg, $hits | Write-Host
    }
    Write-Host "==================================================================================="
}
finally {
    if (-not $proc.HasExited) {
        Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
        $proc.WaitForExit(10000) | Out-Null
    }
}
