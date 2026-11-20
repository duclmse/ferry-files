#Requires -Version 5.1
# The same measurements as bench-windows.ps1, taken from voidtools
# Everything on the same machine, so the two can be compared side by side:
#   cold index build -> query latency -> clean exit -> offline changes
#   -> restart and catch-up -> query latency again -> clean exit
# while sampling Everything's working set and private bytes.
#
# Usage:
#   scripts\bench-everything.ps1 [-OutDir <path>] [-Iterations 3]
#       [-IndexTimeout 1800] [-EverythingVersion 1.4.1.1032] [-EsVersion 1.1.0.38]
#
# Everything reads the file table of every fixed NTFS volume, which needs an
# elevated prompt (or its service, which this script does not install). It
# runs portable: Everything.ini and Everything.db stay in the work dir.
#
# Queries go through es.exe, one process per query, so each timing includes
# starting es.exe and the IPC round trip. The summary gives the time of
# `es.exe -get-everything-version` as that baseline; bench-windows.ps1's
# timings likewise include an HTTP request from PowerShell. Hits are the
# total result count, where bench-windows.ps1 reports up to 100.
#
# Results go to -OutDir: summary.txt, samples.csv, queries.csv, checks.csv.
param(
    [string]$OutDir = "",
    [int]$Iterations = 3,
    [int]$IndexTimeout = 1800,
    [string]$EverythingVersion = "1.4.1.1032",
    [string]$EsVersion = "1.1.0.38"
)

$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")
$repoRoot = (Get-Location).Path

$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
if (-not $OutDir) { $OutDir = Join-Path $repoRoot "target\bench-everything\$stamp" }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$OutDir = (Resolve-Path $OutDir).Path

$workDir = Join-Path $env:TEMP ("fs-evbench-" + [guid]::NewGuid().ToString("N"))
$tools   = Join-Path $workDir "tools"
$fixture = Join-Path $workDir "fixture"
$outside = Join-Path $workDir "outside"
foreach ($d in $tools, $fixture, $outside) { New-Item -ItemType Directory -Force -Path $d | Out-Null }

$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)

$report    = New-Object System.Collections.Generic.List[string]
$samples   = New-Object System.Collections.Generic.List[object]
$queryRows = New-Object System.Collections.Generic.List[object]
$checkRows = New-Object System.Collections.Generic.List[object]

function Say([string]$line) {
    Write-Host $line
    $report.Add($line)
}

function Format-MB([double]$bytes) { [math]::Round($bytes / 1MB, 1) }

function Get-Percentile([double[]]$sorted, [double]$p) {
    if ($sorted.Count -eq 0) { return 0 }
    $i = [math]::Min($sorted.Count - 1, [math]::Floor($p * $sorted.Count))
    return [math]::Round($sorted[$i], 1)
}

# ---------------------------------------------------------------------------
# Tools
# ---------------------------------------------------------------------------

function Get-Tool([string]$name) {
    $zip = Join-Path $tools $name
    $dir = Join-Path $tools ([IO.Path]::GetFileNameWithoutExtension($name))
    Invoke-WebRequest -Uri "https://www.voidtools.com/$name" -OutFile $zip -UseBasicParsing
    Expand-Archive -Path $zip -DestinationPath $dir -Force
    [pscustomobject]@{ Dir = $dir; Sha256 = (Get-FileHash $zip -Algorithm SHA256).Hash }
}

$evTool = Get-Tool "Everything-$EverythingVersion.x64.zip"
$esTool = Get-Tool "ES-$EsVersion.x64.zip"
$exe = Join-Path $evTool.Dir "Everything.exe"
$es  = Join-Path $esTool.Dir "es.exe"
$ini = Join-Path $evTool.Dir "Everything.ini"
$db  = Join-Path $workDir "Everything.db"

# app_data=0 keeps settings and the database beside Everything.exe instead of
# %APPDATA%; Everything reads that one setting only from the ini beside it.
@"
[Everything]
app_data=0
run_as_admin=0
check_for_updates_on_startup=0
"@ | Set-Content -Path $ini -Encoding ASCII

function Invoke-Es([string[]]$esArgs) {
    $out = & $es @esArgs 2>$null
    [pscustomobject]@{ Code = $LASTEXITCODE; Out = $out }
}

# Everything syntax takes space-separated terms; one quoted argument would
# be searched as a phrase.
function Split-Query([string]$q) { @($q -split ' ' | Where-Object { $_ }) }

function Get-Count([string]$q) {
    $r = Invoke-Es (@("-get-result-count") + (Split-Query $q))
    if ($r.Code -ne 0) { return -1 }
    return [int64](@($r.Out)[0])
}

# ---------------------------------------------------------------------------
# Everything control
# ---------------------------------------------------------------------------

function Add-Sample($ev, [string]$phase) {
    $p = $ev.Proc
    try { $p.Refresh() } catch { return }
    if ($p.HasExited) { return }
    $samples.Add([pscustomobject]@{
        run        = $ev.Label
        phase      = $phase
        t_s        = [math]::Round($ev.Clock.Elapsed.TotalSeconds, 2)
        cpu_s      = [math]::Round($p.TotalProcessorTime.TotalSeconds, 3)
        ws_mb      = Format-MB $p.WorkingSet64
        private_mb = Format-MB $p.PrivateMemorySize64
        peak_ws_mb = Format-MB $p.PeakWorkingSet64
    })
}

function Start-Everything([string]$label) {
    $ev = [pscustomobject]@{
        Label  = $label
        Clock  = [Diagnostics.Stopwatch]::StartNew()
        Proc   = $null
        IpcAt  = $null
        LoadAt = $null
    }
    $ev.Proc = Start-Process -FilePath $exe -ArgumentList "-config", "`"$ini`"", "-db", "`"$db`"", "-startup" -PassThru
    $deadline = (Get-Date).AddSeconds($IndexTimeout)
    while ($true) {
        if ($ev.Proc.HasExited) { throw "Everything ($label) exited during startup (code $($ev.Proc.ExitCode))" }
        if (-not $ev.IpcAt) {
            if ((Invoke-Es @("-get-everything-version")).Code -eq 0) { $ev.IpcAt = $ev.Clock.Elapsed.TotalSeconds }
        }
        # Until its database is loaded, es.exe gets no results rather than
        # an error; every file on C: matches "C:\".
        if ($ev.IpcAt -and (Get-Count "C:\") -gt 0) { $ev.LoadAt = $ev.Clock.Elapsed.TotalSeconds; break }
        if ((Get-Date) -gt $deadline) { throw "Everything ($label) did not load its database within $IndexTimeout s" }
        Add-Sample $ev "index"
        Start-Sleep -Milliseconds 250
    }
    Add-Sample $ev "index"
    return $ev
}

function Stop-Everything($ev) {
    if ($ev.Proc.HasExited) { return "already exited (code $($ev.Proc.ExitCode))" }
    $sw = [Diagnostics.Stopwatch]::StartNew()
    & $exe -exit | Out-Null
    if ($ev.Proc.WaitForExit(60000)) { return "-exit, exited in $([math]::Round($sw.Elapsed.TotalSeconds, 1))s" }
    Stop-Process -Id $ev.Proc.Id -Force -ErrorAction SilentlyContinue
    $ev.Proc.WaitForExit(10000) | Out-Null
    return "KILLED after -exit did not stop it (database not saved)"
}

# ---------------------------------------------------------------------------
# Queries: bench-windows.ps1's list for TargetDir C:\, without fuzzy:, which
# Everything does not have.
# ---------------------------------------------------------------------------

$latencyQueries = @(
    "*.rs", "*.toml", "size:>1mb", "dm:today", "ext:rs",
    "!ext:rs size:>100kb", "ext:md | ext:toml",
    "zzqx", "readme", "depth:99", "dupe: count:10",
    "parent:C:\", "path:C:\zzqx"
)

function Measure-Overhead {
    $ms = New-Object System.Collections.Generic.List[double]
    for ($i = 0; $i -lt [math]::Max($Iterations, 5); $i++) {
        $sw = [Diagnostics.Stopwatch]::StartNew()
        Invoke-Es @("-get-everything-version") | Out-Null
        $ms.Add($sw.Elapsed.TotalMilliseconds)
    }
    Get-Percentile ([double[]]($ms | Sort-Object)) 0.5
}

function Measure-Queries($ev) {
    foreach ($q in $latencyQueries) {
        $ms = New-Object System.Collections.Generic.List[double]
        $failed = ""
        for ($i = 0; $i -lt $Iterations; $i++) {
            $sw = [Diagnostics.Stopwatch]::StartNew()
            $r = Invoke-Es (@("-n", "100") + (Split-Query $q))
            $ms.Add($sw.Elapsed.TotalMilliseconds)
            if ($r.Code -ne 0) { $failed = "es.exe exit $($r.Code)"; break }
        }
        $sorted = [double[]]($ms | Sort-Object)
        $queryRows.Add([pscustomobject]@{
            run    = $ev.Label
            query  = $q
            min_ms = Get-Percentile $sorted 0
            med_ms = Get-Percentile $sorted 0.5
            max_ms = Get-Percentile $sorted 0.999
            hits   = Get-Count $q
            error  = $failed
        })
        Add-Sample $ev "queries"
    }
}

# ---------------------------------------------------------------------------
# Fixture: bench-windows.ps1's offline changes and checks
# ---------------------------------------------------------------------------

$tag = [guid]::NewGuid().ToString("N").Substring(0, 8)
function Fx([string]$kind) { "fsb_${kind}_$tag.txt" }
function Write-Bytes([string]$path, [int]$count) { [IO.File]::WriteAllText($path, ("x" * $count)) }

function New-Fixture {
    Write-Bytes (Join-Path $fixture (Fx "modify")) 100
    Write-Bytes (Join-Path $fixture (Fx "delete")) 10
    Write-Bytes (Join-Path $fixture (Fx "renold")) 10
    $dd = Join-Path $fixture "fsb_deldir_$tag"
    New-Item -ItemType Directory -Force -Path $dd | Out-Null
    Write-Bytes (Join-Path $dd (Fx "deldirchild")) 10
}

function Invoke-OfflineChanges {
    Write-Bytes (Join-Path $fixture (Fx "new")) 10
    [IO.File]::AppendAllText((Join-Path $fixture (Fx "modify")), ("y" * 4000))
    Remove-Item (Join-Path $fixture (Fx "delete"))
    Rename-Item (Join-Path $fixture (Fx "renold")) (Fx "rennew")
    $md = Join-Path $outside "fsb_movedir_$tag"
    New-Item -ItemType Directory -Force -Path (Join-Path $md "sub") | Out-Null
    Write-Bytes (Join-Path $md (Fx "movedchild")) 10
    Write-Bytes (Join-Path $md ("sub\" + (Fx "moveddeep"))) 10
    Move-Item $md $fixture
    Remove-Item -Recurse (Join-Path $fixture "fsb_deldir_$tag")
    $nd = Join-Path $fixture "fsb_newdir_$tag"
    New-Item -ItemType Directory -Force -Path $nd | Out-Null
    Write-Bytes (Join-Path $nd (Fx "newdirchild")) 10
}

# Everything indexes all of C:, so "moved in" is a move from one folder to
# another; it is the same journal record either way.
function Expect([string]$desc, [string]$query, [bool]$present) {
    [pscustomobject]@{ Desc = $desc; Query = $query; Present = $present; Seen = ""; Ok = $false; At = $null }
}

$baseline = @(
    (Expect "file to modify, original size" "wfn:$(Fx 'modify') size:100" $true),
    (Expect "file to delete"                "wfn:$(Fx 'delete')" $true),
    (Expect "file to rename"                "wfn:$(Fx 'renold')" $true),
    (Expect "file in dir to delete"         "wfn:$(Fx 'deldirchild')" $true)
)
$afterChanges = @(
    (Expect "created file"                   "wfn:$(Fx 'new')" $true),
    (Expect "modified file, new size"        "wfn:$(Fx 'modify') size:4100" $true),
    (Expect "deleted file"                   "wfn:$(Fx 'delete')" $false),
    (Expect "renamed file, old name"         "wfn:$(Fx 'renold')" $false),
    (Expect "renamed file, new name"         "wfn:$(Fx 'rennew')" $true),
    (Expect "file in dir moved in"           "wfn:$(Fx 'movedchild')" $true),
    (Expect "file in subdir of dir moved in" "wfn:$(Fx 'moveddeep')" $true),
    (Expect "file in deleted dir"            "wfn:$(Fx 'deldirchild')" $false),
    (Expect "file in new dir"                "wfn:$(Fx 'newdirchild')" $true)
)

function Wait-Expectations($ev, $list, [int]$timeoutSecs) {
    $deadline = (Get-Date).AddSeconds($timeoutSecs)
    while ($true) {
        foreach ($e in $list | Where-Object { -not $_.Ok }) {
            $n = Get-Count $e.Query
            $e.Seen = "$n hit(s)"
            if (($e.Present -and $n -ge 1) -or (-not $e.Present -and $n -eq 0)) {
                $e.Ok = $true; $e.At = [math]::Round($ev.Clock.Elapsed.TotalSeconds, 2)
            }
        }
        Add-Sample $ev "catch-up"
        if (@($list | Where-Object { -not $_.Ok }).Count -eq 0) { return $true }
        if ((Get-Date) -gt $deadline) { return $false }
        Start-Sleep -Milliseconds 250
    }
}

function Write-Phase($ev, [string]$phase) {
    $rows = @($samples | Where-Object { $_.run -eq $ev.Label -and $_.phase -eq $phase })
    if ($rows.Count -eq 0) { return }
    Say ("{0,-8} {1,-10} {2,9} {3,9} {4,9}" -f $ev.Label, $phase,
        ($rows | Measure-Object ws_mb -Maximum).Maximum, $rows[-1].ws_mb, $rows[-1].private_mb)
}

# ---------------------------------------------------------------------------
# Run
# ---------------------------------------------------------------------------

$running = $null
$exitCode = 1
try {
    $os = Get-CimInstance Win32_OperatingSystem
    $cpu = Get-CimInstance Win32_Processor | Select-Object -First 1
    $vols = @(Get-Volume | Where-Object { $_.DriveLetter -and $_.DriveType -eq 'Fixed' } |
        ForEach-Object { "$($_.DriveLetter): $($_.FileSystem)" }) -join ", "

    Say "==================== Everything Windows benchmark ===================="
    Say "Date:         $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')"
    Say "OS:           $($os.Caption) $($os.Version)"
    Say "CPU:          $($cpu.Name.Trim()) ($([Environment]::ProcessorCount) logical)"
    Say "RAM:          $([math]::Round($os.TotalVisibleMemorySize / 1MB, 1)) GB"
    Say "Elevated:     $isAdmin$(if (-not $isAdmin) {' (Everything cannot read the file table without its service)'})"
    Say "Everything:   $EverythingVersion x64 portable, sha256 $($evTool.Sha256)"
    Say "es.exe:       $EsVersion x64, sha256 $($esTool.Sha256)"
    Say "Volumes:      $vols (Everything indexes every fixed NTFS volume)"
    Say "Work dir:     $workDir"
    Say ""

    New-Fixture
    Write-Host "==> Starting Everything (first run, no database)"
    $running = Start-Everything "first"
    $e1 = $running
    $total = Get-Count "*"
    $onC = Get-Count "C:\"
    Say "---- first start ----"
    Say ("Index:        IPC up {0:N1}s, database built {1:N1}s after launch; {2:N0} entries on C:, {3:N0} in all" -f `
        $e1.IpcAt, $e1.LoadAt, $onC, $total)
    if (-not (Wait-Expectations $e1 $baseline 60)) {
        foreach ($e in $baseline | Where-Object { -not $_.Ok }) { Say "  BASELINE FAIL  $($e.Desc): $($e.Seen)" }
        throw "the fixture is not searchable after the first start, so catch-up cannot be checked"
    }
    $overhead = Measure-Overhead
    Write-Host "==> Query latency ($Iterations iterations each)"
    Measure-Queries $e1
    $stop1 = Stop-Everything $e1
    $running = $null
    Say "Stop:         $stop1"
    Say ("Disk:         {0:N1} MB Everything.db" -f (Format-MB (Get-Item $db -ErrorAction SilentlyContinue).Length))

    Write-Host "==> Making changes while stopped"
    Invoke-OfflineChanges

    Write-Host "==> Starting Everything (catch-up)"
    $running = Start-Everything "catchup"
    $e2 = $running
    $allOk = Wait-Expectations $e2 $afterChanges 300
    $verifiedAt = $e2.Clock.Elapsed.TotalSeconds
    Say ""
    Say "---- catch-up after restart ----"
    Say ("Timing:       IPC up {0:N2}s, database loaded {1:N2}s, all changes visible {2}" -f `
        $e2.IpcAt, $e2.LoadAt, $(if ($allOk) { "{0:N2}s" -f $verifiedAt } else { "NEVER (timed out)" }))
    Say "Checks:"
    foreach ($e in $afterChanges) {
        $mark = if ($e.Ok) { "ok  " } else { "FAIL" }
        $when = if ($e.Ok) { "at {0:N2}s" -f $e.At } else { "seen: $($e.Seen)" }
        Say ("  {0} {1,-32} {2}" -f $mark, $e.Desc, $when)
        $checkRows.Add([pscustomobject]@{ check = $e.Desc; query = $e.Query; ok = $e.Ok; at_s = $e.At; seen = $e.Seen })
    }
    Write-Host "==> Query latency after restart"
    Measure-Queries $e2
    $stop2 = Stop-Everything $e2
    $running = $null
    Say "Stop:         $stop2"

    Say ""
    Say "---- process (MB are working set / private bytes) ----"
    Say ("{0,-8} {1,-10} {2,9} {3,9} {4,9}" -f "run", "phase", "peak_ws", "end_ws", "end_priv")
    foreach ($ev in $e1, $e2) { foreach ($ph in "index", "queries", "catch-up") { Write-Phase $ev $ph } }
    Say "Peak working set over the whole run: $(($samples | Measure-Object peak_ws_mb -Maximum).Maximum) MB"

    Say ""
    Say "---- query latency ms ($Iterations iterations via es.exe; es.exe baseline $overhead ms) ----"
    Say ("{0,-8} {1,-40} {2,8} {3,8} {4,8} {5,10}" -f "run", "query", "min", "med", "max", "hits")
    foreach ($r in $queryRows) {
        $line = "{0,-8} {1,-40} {2,8} {3,8} {4,8} {5,10}" -f $r.run, $r.query, $r.min_ms, $r.med_ms, $r.max_ms, $r.hits
        if ($r.error) { $line += "  ERROR: $($r.error)" }
        Say $line
    }

    $failed = @($afterChanges | Where-Object { -not $_.Ok }).Count
    Say ""
    Say "Result: Everything catch-up, $($afterChanges.Count - $failed)/$($afterChanges.Count) checks passed"
    Say "======================================================================"
    $exitCode = if ($failed -gt 0) { 1 } else { 0 }
}
finally {
    if ($running -and -not $running.Proc.HasExited) {
        Stop-Process -Id $running.Proc.Id -Force -ErrorAction SilentlyContinue
    }
    $samples   | Export-Csv -NoTypeInformation -Path (Join-Path $OutDir "samples.csv")
    $queryRows | Export-Csv -NoTypeInformation -Path (Join-Path $OutDir "queries.csv")
    $checkRows | Export-Csv -NoTypeInformation -Path (Join-Path $OutDir "checks.csv")
    $report    | Set-Content -Path (Join-Path $OutDir "summary.txt") -Encoding UTF8
    Remove-Item -Recurse -Force $workDir -ErrorAction SilentlyContinue
    Write-Host ""
    Write-Host "Results: $OutDir"
}
exit $exitCode
