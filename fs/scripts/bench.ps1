#Requires -Version 5.1
# End-to-end crawl benchmark: builds (if needed), runs a real daemon against a
# real directory tree in an isolated scratch config, times the initial full
# crawl via GET /stats, and reports throughput.
#
# Usage:
#   scripts\bench.ps1 [-TargetDir <path>] [-WithContent]
#
# -TargetDir defaults to the repo root. -WithContent turns on full-text
# content extraction for common source extensions; by default it's off, which
# isolates pure filesystem-crawl throughput (stat + SQLite upsert only).
# Archive-inner-file indexing is always off, so results are comparable across
# runs regardless of how many archives happen to live under TargetDir.
#
# Note: the daemon is stopped with Stop-Process (hard-kill), not a graceful
# SIGTERM-equivalent - this is a benchmark helper, not a production stop path.
param(
    [string]$TargetDir = "",
    [switch]$WithContent
)

$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")
$repoRoot = (Get-Location).Path

if (-not $TargetDir) { $TargetDir = $repoRoot }
$TargetDir = (Resolve-Path $TargetDir).Path

if (-not (Test-Path "target\release\filesearch.exe")) {
    Write-Host "==> Building release binary first"
    & .\scripts\build.ps1
}
$bin = Join-Path $repoRoot "target\release\filesearch.exe"

$workDir = Join-Path $env:TEMP ("filesearch-bench-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path "$workDir\data" | Out-Null
New-Item -ItemType Directory -Force -Path "$workDir\logs" | Out-Null
$config = Join-Path $workDir "config.toml"

$restPort = 17700
$grpcPort = 17701

$contentToml = ""
if ($WithContent) {
    $contentToml = @"

[content.extension_limits]
rs = 524288
py = 524288
js = 262144
ts = 262144
go = 524288
md = 131072
txt = 131072
"*" = 32768
"@
}

@"
watch_paths = ["$($TargetDir -replace '\\','/')"]
data_dir    = "$($workDir -replace '\\','/')/data"
log_dir     = "$($workDir -replace '\\','/')/logs"

[archive_index]
enabled = false

[api]
rest_port    = $restPort
grpc_port    = $grpcPort
grpc_enabled = false
ipc_enabled  = false
$contentToml
"@ | Set-Content -Path $config -Encoding UTF8

Write-Host "==> Validating config"
& $bin --config $config validate-config
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

Write-Host "==> Target:  $TargetDir"
Write-Host "==> Content: $(if ($WithContent) {'enabled'} else {'disabled'})"
Write-Host "==> Starting daemon"

$startEpoch = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
$proc = Start-Process -FilePath $bin -ArgumentList "--config", $config, "daemon" `
    -RedirectStandardOutput "$workDir\daemon.out.log" `
    -RedirectStandardError  "$workDir\daemon.err.log" `
    -PassThru -WindowStyle Hidden

try {
    $healthUrl = "http://127.0.0.1:$restPort/health"
    $up = $false
    for ($i = 0; $i -lt 50; $i++) {
        try { Invoke-RestMethod -Uri $healthUrl -TimeoutSec 1 | Out-Null; $up = $true; break }
        catch { Start-Sleep -Milliseconds 200 }
    }
    if (-not $up) { throw "daemon did not come up; see $workDir\daemon.err.log" }

    Write-Host "==> Waiting for full crawl to finish (timeout 600s)"
    $stats = $null
    $lastCrawl = 0
    for ($i = 0; $i -lt 600; $i++) {
        if ($proc.HasExited) {
            Get-Content "$workDir\daemon.err.log" | Write-Host
            throw "daemon exited unexpectedly"
        }
        $stats = Invoke-RestMethod -Uri "http://127.0.0.1:$restPort/stats" -TimeoutSec 5
        $lastCrawl = [int64]$stats.last_crawl_secs
        if ($lastCrawl -ne 0) { break }
        Start-Sleep -Seconds 1
    }
    if ($lastCrawl -eq 0) { throw "crawl did not finish within timeout" }

    $elapsed = $lastCrawl - $startEpoch
    if ($elapsed -lt 1) { $elapsed = 1 }
    $filesPerSec   = [math]::Round($stats.total_files / $elapsed)
    $entriesPerSec = [math]::Round(($stats.total_files + $stats.total_dirs) / $elapsed)

    Write-Host ""
    Write-Host "==================== Benchmark Results ===================="
    Write-Host "Target directory:   $TargetDir"
    Write-Host "Content extraction: $(if ($WithContent) {'enabled'} else {'disabled'})"
    Write-Host "Elapsed:            ${elapsed}s"
    Write-Host "Files indexed:      $($stats.total_files)"
    Write-Host "Dirs indexed:       $($stats.total_dirs)"
    Write-Host "Content indexed:    $($stats.content_indexed)"
    Write-Host "Throughput:         $filesPerSec files/sec, $entriesPerSec entries/sec"
    Write-Host "=============================================================="
}
finally {
    if (-not $proc.HasExited) {
        Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
        $proc.WaitForExit(10000) | Out-Null
    }
    Remove-Item -Recurse -Force $workDir -ErrorAction SilentlyContinue
}
