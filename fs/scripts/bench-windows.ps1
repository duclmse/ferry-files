#Requires -Version 5.1
# Windows end-to-end benchmark. One run covers:
#   cold crawl -> idle -> single-client query latency -> concurrent query load
#   -> clean stop -> offline changes -> warm restart and catch-up -> idle
#   -> query latency again -> clean stop
# while sampling the daemon's CPU time, working set and private bytes.
#
# Usage:
#   scripts\bench-windows.ps1 [-TargetDir <path>] [-Store sqlite|memory]
#       [-DataDir <path>] [-OutDir <path>] [-RootStyle forward|backslash]
#       [-Clients 8] [-LoadSeconds 20] [-IdleSeconds 15] [-Iterations 5]
#       [-CrawlTimeout 3600]
#
# Catch-up has two paths on Windows, and one run exercises only one of them:
#   - USN journal replay needs admin rights, as the service's LocalSystem
#     account has. Run from an elevated prompt to get it.
#   - From a normal prompt, replay fails and the timestamp scan runs instead.
# Run once of each to compare. The summary says which path ran and why.
#
# Besides TargetDir, the daemon watches a small fixture root that the script
# creates under the work dir. All offline changes happen there; TargetDir is
# only read. The changes are: create, modify, delete, rename, a directory
# moved in from outside the roots, a directory deleted with its contents, and
# a new directory with a file in it. After the restart, each one is checked
# through GET /search until all pass or 300s elapse.
#
# -DataDir keeps the index across runs. A later run warm-starts instead of
# crawling, which keeps repeated runs over a large TargetDir (e.g. C:\) cheap.
# Without it, a fresh temp dir is used and deleted at the end.
#
# -RootStyle backslash writes watch_paths with '\' separators instead of '/',
# to check that replayed paths match crawled ones either way.
#
# Results go to -OutDir, default target\bench-windows\<timestamp>:
#   summary.txt    the report printed at the end
#   samples.csv    one row per process sample
#   queries.csv    per-query latency
#   daemon logs and the config used
param(
    [string]$TargetDir = "",
    [ValidateSet("sqlite", "memory")]
    [string]$Store = "sqlite",
    [string]$DataDir = "",
    [string]$OutDir = "",
    [ValidateSet("forward", "backslash")]
    [string]$RootStyle = "forward",
    [int]$Clients = 8,
    [int]$LoadSeconds = 20,
    [int]$IdleSeconds = 15,
    [int]$Iterations = 5,
    [int]$CrawlTimeout = 3600
)

$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")
$repoRoot = (Get-Location).Path

# WebClient shares a per-host connection limit of 2 across the whole process,
# which would cap the concurrent load phase at two in-flight requests.
[Net.ServicePointManager]::DefaultConnectionLimit = [math]::Max(64, $Clients * 2)

if (-not $TargetDir) { $TargetDir = $repoRoot }
$TargetDir = (Resolve-Path $TargetDir).Path

if (-not (Test-Path "target\release\fsd.exe")) {
    Write-Host "==> Building release binary first"
    & .\scripts\build.ps1
}
$bin = Join-Path $repoRoot "target\release\fsd.exe"

$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
if (-not $OutDir) { $OutDir = Join-Path $repoRoot "target\bench-windows\$stamp" }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$OutDir = (Resolve-Path $OutDir).Path

$keepData = [bool]$DataDir
if ($DataDir) {
    New-Item -ItemType Directory -Force -Path $DataDir | Out-Null
    $workDir = (Resolve-Path $DataDir).Path
} else {
    $workDir = Join-Path $env:TEMP ("fs-winbench-" + [guid]::NewGuid().ToString("N"))
}
$fixture = Join-Path $workDir "fixture"
$outside = Join-Path $workDir "outside"
foreach ($d in "data", "logs", "fixture", "outside") {
    New-Item -ItemType Directory -Force -Path (Join-Path $workDir $d) | Out-Null
}
$config   = Join-Path $workDir "config.toml"
$restPort = 17700
$base     = "http://127.0.0.1:$restPort"

$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)

# ---------------------------------------------------------------------------
# Report and samples
# ---------------------------------------------------------------------------

$report     = New-Object System.Collections.Generic.List[string]
$samples    = New-Object System.Collections.Generic.List[object]
$queryRows  = New-Object System.Collections.Generic.List[object]
$phaseRows  = New-Object System.Collections.Generic.List[object]
$checkRows  = New-Object System.Collections.Generic.List[object]

function Say([string]$line) {
    Write-Host $line
    $report.Add($line)
}

function Format-MB([double]$bytes) { [math]::Round($bytes / 1MB, 1) }

function Add-Sample($d, [string]$phase) {
    $p = $d.Proc
    try { $p.Refresh() } catch { return }
    if ($p.HasExited) { return }
    $samples.Add([pscustomobject]@{
        run        = $d.Label
        phase      = $phase
        t_s        = [math]::Round($d.Clock.Elapsed.TotalSeconds, 2)
        cpu_s      = [math]::Round($p.TotalProcessorTime.TotalSeconds, 3)
        ws_mb      = Format-MB $p.WorkingSet64
        private_mb = Format-MB $p.PrivateMemorySize64
        peak_ws_mb = Format-MB $p.PeakWorkingSet64
    })
}

# A phase is bracketed by samples, so its duration and CPU time come from the
# first and last sample tagged with its name.
function Close-Phase($d, [string]$phase) {
    Add-Sample $d $phase
    $rows = @($samples | Where-Object { $_.run -eq $d.Label -and $_.phase -eq $phase })
    if ($rows.Count -lt 2) { return }
    $first = $rows[0]; $last = $rows[-1]
    $secs = $last.t_s - $first.t_s
    $cpu  = $last.cpu_s - $first.cpu_s
    $phaseRows.Add([pscustomobject]@{
        run        = $d.Label
        phase      = $phase
        secs       = [math]::Round($secs, 1)
        cpu_s      = [math]::Round($cpu, 1)
        cpu_pct    = if ($secs -gt 0) { [math]::Round(100 * $cpu / $secs) } else { 0 }
        peak_ws_mb = ($rows | Measure-Object ws_mb -Maximum).Maximum
        end_ws_mb  = $last.ws_mb
        end_priv_mb = $last.private_mb
    })
}

function Get-Percentile([double[]]$sorted, [double]$p) {
    if ($sorted.Count -eq 0) { return 0 }
    $i = [math]::Min($sorted.Count - 1, [math]::Floor($p * $sorted.Count))
    return [math]::Round($sorted[$i], 1)
}

# ---------------------------------------------------------------------------
# Daemon control
# ---------------------------------------------------------------------------

function Read-Shared([string]$path) {
    if (-not (Test-Path $path)) { return "" }
    $fs = [IO.File]::Open($path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::ReadWrite)
    try { return (New-Object IO.StreamReader($fs)).ReadToEnd() } finally { $fs.Dispose() }
}

function Get-LogMessages($d) {
    $text = Read-Shared $d.Out
    $re = [regex]'"message":"((?:[^"\\]|\\.)*)"'
    @($re.Matches($text) | ForEach-Object { $_.Groups[1].Value -replace '\\\\', '\' })
}

function Find-Log($d, [string]$pattern) {
    Get-LogMessages $d | Where-Object { $_ -match $pattern } | Select-Object -First 1
}

function Start-Daemon([string]$label) {
    $d = [pscustomobject]@{
        Label = $label
        Out   = Join-Path $workDir "logs\daemon.$label.out.log"
        Err   = Join-Path $workDir "logs\daemon.$label.err.log"
        Clock = [Diagnostics.Stopwatch]::StartNew()
        Proc  = $null
        Ready = 0.0
    }
    # Start-Process gives the child its own (hidden) console unless
    # -NoNewWindow is passed; Stop-Daemon needs that to send it Ctrl+C.
    $d.Proc = Start-Process -FilePath $bin -ArgumentList "--config", "`"$config`"", "daemon" `
        -RedirectStandardOutput $d.Out -RedirectStandardError $d.Err `
        -PassThru -WindowStyle Hidden
    Add-Sample $d "startup"
    $up = $false
    for ($i = 0; $i -lt 1200; $i++) {
        if ($d.Proc.HasExited) {
            Get-Content $d.Err -ErrorAction SilentlyContinue | Write-Host
            throw "daemon ($label) exited during startup; see $($d.Err)"
        }
        try { Invoke-RestMethod -Uri "$base/health" -TimeoutSec 1 | Out-Null; $up = $true; break }
        catch { Start-Sleep -Milliseconds 100 }
        if ($i % 10 -eq 0) { Add-Sample $d "startup" }
    }
    if (-not $up) { throw "daemon ($label) did not answer /health; see $($d.Out)" }
    $d.Ready = $d.Clock.Elapsed.TotalSeconds
    Close-Phase $d "startup"
    return $d
}

# tokio's ctrl_c is the daemon's only clean-shutdown trigger on Windows, and
# the memory store writes index.bin only then. A process can only send
# Ctrl+C to a console it is attached to, so a helper process detaches from
# its own console, attaches to the daemon's and sends it there, ignoring it
# itself.
function Send-CtrlC([int]$targetPid) {
    $helper = @'
$sig = @"
[DllImport("kernel32.dll", SetLastError=true)] public static extern bool FreeConsole();
[DllImport("kernel32.dll", SetLastError=true)] public static extern bool AttachConsole(uint pid);
[DllImport("kernel32.dll", SetLastError=true)] public static extern bool SetConsoleCtrlHandler(IntPtr h, bool add);
[DllImport("kernel32.dll", SetLastError=true)] public static extern bool GenerateConsoleCtrlEvent(uint ev, uint group);
"@
$k = Add-Type -MemberDefinition $sig -Name K -Namespace FsBench -PassThru
[void]$k::FreeConsole()
if (-not $k::AttachConsole(__PID__)) { exit 2 }
[void]$k::SetConsoleCtrlHandler([IntPtr]::Zero, $true)
if (-not $k::GenerateConsoleCtrlEvent(0, 0)) { exit 3 }
Start-Sleep -Milliseconds 500
exit 0
'@
    $helper = $helper -replace '__PID__', $targetPid
    $enc = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($helper))
    $h = Start-Process powershell.exe -ArgumentList "-NoProfile", "-NonInteractive", "-EncodedCommand", $enc `
        -WindowStyle Hidden -PassThru -Wait
    return $h.ExitCode
}

function Stop-Daemon($d) {
    if ($d.Proc.HasExited) { return "already exited (code $($d.Proc.ExitCode))" }
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $code = Send-CtrlC $d.Proc.Id
    if ($d.Proc.WaitForExit(60000)) {
        return "Ctrl+C, exited in $([math]::Round($sw.Elapsed.TotalSeconds, 1))s"
    }
    Stop-Process -Id $d.Proc.Id -Force -ErrorAction SilentlyContinue
    $d.Proc.WaitForExit(10000) | Out-Null
    return "KILLED after Ctrl+C did not stop it (helper exit $code)"
}

# ---------------------------------------------------------------------------
# Queries
# ---------------------------------------------------------------------------

function Search([string]$q, [int]$limit = 100) {
    Invoke-RestMethod -Uri "$base/search?q=$([uri]::EscapeDataString($q))&limit=$limit" -TimeoutSec 60
}

$latencyQueries = @(
    "*.rs", "*.toml", "size:>1mb", "dm:today", "ext:rs", "fuzzy:main",
    "!ext:rs size:>100kb", "ext:md | ext:toml",
    "zzqx", "readme", "depth:99", "dupe:name count:10",
    "parent:$TargetDir", "path:$TargetDir\zzqx"
)
$loadQueries = @("*.rs", "*.toml", "size:>1mb", "dm:today", "ext:rs", "fuzzy:main", "!ext:rs size:>100kb", "ext:md | ext:toml")

function Measure-Queries($d, [string]$phase) {
    Add-Sample $d $phase
    foreach ($q in $latencyQueries) {
        $ms = New-Object System.Collections.Generic.List[double]
        $server = New-Object System.Collections.Generic.List[double]
        $hits = 0
        $failed = ""
        for ($i = 0; $i -lt $Iterations; $i++) {
            $sw = [Diagnostics.Stopwatch]::StartNew()
            try { $r = Search $q 100 }
            catch { $failed = $_.Exception.Message; break }
            $ms.Add($sw.Elapsed.TotalMilliseconds)
            $server.Add([double]$r.took_ms)
            $hits = $r.total
        }
        $sorted = [double[]]($ms | Sort-Object)
        $queryRows.Add([pscustomobject]@{
            run       = $d.Label
            query     = $q
            min_ms    = Get-Percentile $sorted 0
            med_ms    = Get-Percentile $sorted 0.5
            max_ms    = Get-Percentile $sorted 0.999
            server_ms = if ($server.Count) { [math]::Round(($server | Measure-Object -Average).Average) } else { 0 }
            hits      = $hits
            error     = $failed
        })
        Add-Sample $d $phase
    }
    Close-Phase $d $phase
}

function Measure-Load($d) {
    $worker = {
        param($base, $queries, $seconds, $offset)
        $wc = New-Object System.Net.WebClient
        $wc.Proxy = $null
        $lat = New-Object System.Collections.Generic.List[double]
        $errors = 0
        $n = $offset
        $sw = [Diagnostics.Stopwatch]::StartNew()
        while ($sw.Elapsed.TotalSeconds -lt $seconds) {
            $q = [uri]::EscapeDataString($queries[$n % $queries.Count])
            $t0 = $sw.Elapsed.TotalMilliseconds
            try { [void]$wc.DownloadString("$base/search?q=$q&limit=50") } catch { $errors++ }
            $lat.Add($sw.Elapsed.TotalMilliseconds - $t0)
            $n++
        }
        [pscustomobject]@{ Latencies = $lat.ToArray(); Errors = $errors }
    }
    $pool = [runspacefactory]::CreateRunspacePool(1, $Clients)
    $pool.Open()
    $jobs = @()
    try {
        Add-Sample $d "load"
        $sw = [Diagnostics.Stopwatch]::StartNew()
        for ($i = 0; $i -lt $Clients; $i++) {
            $ps = [powershell]::Create()
            $ps.RunspacePool = $pool
            [void]$ps.AddScript($worker).AddArgument($base).AddArgument($loadQueries).AddArgument($LoadSeconds).AddArgument($i)
            $jobs += [pscustomobject]@{ PS = $ps; Handle = $ps.BeginInvoke() }
        }
        while (@($jobs | Where-Object { -not $_.Handle.IsCompleted }).Count -gt 0) {
            Start-Sleep -Seconds 1
            Add-Sample $d "load"
        }
        $wall = $sw.Elapsed.TotalSeconds
        Close-Phase $d "load"
        $all = New-Object System.Collections.Generic.List[double]
        $errors = 0
        foreach ($j in $jobs) {
            foreach ($r in $j.PS.EndInvoke($j.Handle)) {
                $all.AddRange([double[]]$r.Latencies)
                $errors += $r.Errors
            }
        }
        $sorted = [double[]]($all | Sort-Object)
        return [pscustomobject]@{
            Requests = $all.Count
            Errors   = $errors
            Qps      = [math]::Round($all.Count / $wall)
            P50      = Get-Percentile $sorted 0.5
            P95      = Get-Percentile $sorted 0.95
            P99      = Get-Percentile $sorted 0.99
            Max      = Get-Percentile $sorted 1
        }
    } finally {
        foreach ($j in $jobs) { $j.PS.Dispose() }
        $pool.Close()
    }
}

function Wait-Idle($d, [string]$phase, [int]$seconds) {
    Add-Sample $d $phase
    for ($i = 0; $i -lt $seconds; $i++) {
        Start-Sleep -Seconds 1
        Add-Sample $d $phase
    }
    Close-Phase $d $phase
}

# ---------------------------------------------------------------------------
# Fixture: offline changes and how each must look after catch-up
# ---------------------------------------------------------------------------

$tag = [guid]::NewGuid().ToString("N").Substring(0, 8)
function Fx([string]$kind) { "fsb_${kind}_$tag.txt" }

function Write-Bytes([string]$path, [int]$count) {
    [IO.File]::WriteAllText($path, ("x" * $count))
}

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

function Expect([string]$desc, [string]$name, [string]$state, [int64]$size = -1) {
    [pscustomobject]@{ Desc = $desc; Name = $name; State = $state; Size = $size; Seen = ""; Ok = $false; At = $null }
}

$baseline = @(
    (Expect "file to modify, original size" (Fx "modify") "size" 100),
    (Expect "file to delete"                (Fx "delete") "present"),
    (Expect "file to rename"                (Fx "renold") "present"),
    (Expect "file in dir to delete"         (Fx "deldirchild") "present")
)
$afterChanges = @(
    (Expect "created file"                  (Fx "new") "present"),
    (Expect "modified file, new size"       (Fx "modify") "size" 4100),
    (Expect "deleted file"                  (Fx "delete") "absent"),
    (Expect "renamed file, old name"        (Fx "renold") "absent"),
    (Expect "renamed file, new name"        (Fx "rennew") "present"),
    (Expect "file in dir moved in"          (Fx "movedchild") "present"),
    (Expect "file in subdir of dir moved in" (Fx "moveddeep") "present"),
    (Expect "file in deleted dir"           (Fx "deldirchild") "absent"),
    (Expect "file in new dir"               (Fx "newdirchild") "present")
)

function Test-Expect($e) {
    try { $r = Search $e.Name 100 } catch { $e.Seen = "search failed: $($_.Exception.Message)"; return $false }
    $hits = @($r.result | Where-Object { $_.name -eq $e.Name })
    $e.Seen = if ($hits.Count -eq 0) { "no hit" } else { "$($hits.Count) hit(s), size $($hits[0].size_bytes), $($hits[0].path)" }
    switch ($e.State) {
        "present" { return $hits.Count -ge 1 }
        "absent"  { return $hits.Count -eq 0 }
        "size"    { return ($hits.Count -ge 1) -and ([int64]$hits[0].size_bytes -eq $e.Size) }
    }
    return $false
}

# Polls every pending expectation until all hold or the timeout passes, and
# records when each first held, measured on the daemon's clock.
function Wait-Expectations($d, $list, [int]$timeoutSecs, [string]$phase) {
    $deadline = (Get-Date).AddSeconds($timeoutSecs)
    while ($true) {
        foreach ($e in $list | Where-Object { -not $_.Ok }) {
            if (Test-Expect $e) { $e.Ok = $true; $e.At = [math]::Round($d.Clock.Elapsed.TotalSeconds, 2) }
        }
        Add-Sample $d $phase
        if (@($list | Where-Object { -not $_.Ok }).Count -eq 0) { return $true }
        if ((Get-Date) -gt $deadline) { return $false }
        Start-Sleep -Milliseconds 250
    }
}

# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------

function Format-Root([string]$p) {
    # A TOML literal string takes backslashes as they are.
    if ($RootStyle -eq "backslash") { return "'" + $p + "'" }
    return '"' + ($p -replace '\\', '/') + '"'
}

$leaf = Split-Path $workDir -Leaf
@"
watch_paths = [$(Format-Root $TargetDir), $(Format-Root $fixture)]
data_dir    = "$($workDir -replace '\\','/')/data"
log_dir     = "$($workDir -replace '\\','/')/logs"

[store]
mode = "$Store"

[exclusions]
# The work dir may sit under TargetDir (e.g. TargetDir = C:\); without these
# the daemon would index its own database and log writes.
path_patterns = ["**/$leaf/data/**", "**/$leaf/logs/**", "**/$leaf/outside/**"]

[archive_index]
enabled = false

[api]
rest_port    = $restPort
grpc_port    = $($restPort + 1)
grpc_enabled = false
ipc_enabled  = false
"@ | Set-Content -Path $config -Encoding UTF8

& $bin --config $config validate-config
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# ---------------------------------------------------------------------------
# Run
# ---------------------------------------------------------------------------

$running = $null
$catchUp = $null
try {
    $os = Get-CimInstance Win32_OperatingSystem
    $cpu = Get-CimInstance Win32_Processor | Select-Object -First 1
    $fsTypes = @($TargetDir, $fixture | ForEach-Object {
        $root = [IO.Path]::GetPathRoot($_)
        "$root $((New-Object IO.DriveInfo($root)).DriveFormat)"
    }) -join ", "
    $gitRev = (& git rev-parse --short HEAD 2>$null)
    $dirty = if (& git status --porcelain --untracked-files=no 2>$null) { " (dirty)" } else { "" }

    Say "==================== fs Windows benchmark ===================="
    Say "Date:         $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')"
    Say "Commit:       $gitRev$dirty"
    Say "OS:           $($os.Caption) $($os.Version)"
    Say "CPU:          $($cpu.Name.Trim()) ($([Environment]::ProcessorCount) logical)"
    Say "RAM:          $([math]::Round($os.TotalVisibleMemorySize / 1MB, 1)) GB"
    Say "Elevated:     $isAdmin $(if ($isAdmin) {'(USN replay expected)'} else {'(timestamp scan expected)'})"
    Say "Store:        $Store"
    Say "Roots:        $TargetDir, $fixture  [$RootStyle separators]"
    Say "Volumes:      $fsTypes"
    Say "Work dir:     $workDir$(if ($keepData) {' (kept)'})"
    Say ""

    # ---- first start: cold crawl, or warm start on a kept index -----------
    New-Fixture
    Write-Host "==> Starting daemon (first run)"
    $running = Start-Daemon "first"
    $d1 = $running
    Add-Sample $d1 "crawl"
    $decision = $null
    $crawled = $false
    $deadline = (Get-Date).AddSeconds($CrawlTimeout)
    while ($true) {
        if ($d1.Proc.HasExited) { throw "daemon exited during first start; see $($d1.Err)" }
        if (-not $decision) {
            $decision = Find-Log $d1 '^full crawl: |^catch-up: |^index already built'
        }
        if ($decision -and $decision -notmatch '^full crawl: ') { break }
        if ($decision -and (Find-Log $d1 '^full crawl complete')) { $crawled = $true; break }
        if ((Get-Date) -gt $deadline) { throw "first start did not finish within $CrawlTimeout s" }
        Start-Sleep -Milliseconds 500
        Add-Sample $d1 "crawl"
    }
    $firstDone = $d1.Clock.Elapsed.TotalSeconds
    Close-Phase $d1 "crawl"
    $stats = Invoke-RestMethod -Uri "$base/stats" -TimeoutSec 10

    Say "---- first start ----"
    Say "Decision:     $decision"
    if ($crawled) {
        $crawlSecs = $firstDone - $d1.Ready
        $entries = [int64]$stats.total_files + [int64]$stats.total_dirs
        Say ("Crawl:        {0:N1}s after /health ({1:N1}s after launch), {2:N0} files, {3:N0} dirs, {4:N0} entries/s" -f `
            $crawlSecs, $firstDone, [int64]$stats.total_files, [int64]$stats.total_dirs, ($entries / [math]::Max($crawlSecs, 0.1)))
    } else {
        Say ("Warm start:   ready {0:N1}s after launch (index reused, no crawl)" -f $firstDone)
    }

    if (-not (Wait-Expectations $d1 $baseline 60 "baseline")) {
        foreach ($e in $baseline | Where-Object { -not $_.Ok }) { Say "  BASELINE FAIL  $($e.Desc): $($e.Seen)" }
        throw "the fixture is not searchable after the first start, so catch-up cannot be checked"
    }
    Close-Phase $d1 "baseline"

    Write-Host "==> Idle $IdleSeconds s"
    Wait-Idle $d1 "idle" $IdleSeconds
    Write-Host "==> Query latency ($Iterations iterations each)"
    Measure-Queries $d1 "queries"
    Write-Host "==> Query load: $Clients clients for $LoadSeconds s"
    $load = Measure-Load $d1
    Wait-Idle $d1 "after-load" 5

    # The stored change-log position is written once a crawl is over, or 60s
    # after a start without one. Stopping before that turns the restart into
    # a full crawl ("no change-log position stored yet").
    $minUp = if ($crawled) { $firstDone + 5 } else { 65 }
    if ($d1.Clock.Elapsed.TotalSeconds -lt $minUp) {
        Write-Host "==> Waiting for the daemon to store its change-log position"
        Wait-Idle $d1 "position-wait" ([int][math]::Ceiling($minUp - $d1.Clock.Elapsed.TotalSeconds))
    }

    Write-Host "==> Stopping daemon"
    $stop1 = Stop-Daemon $d1
    $running = $null
    Say "Stop:         $stop1"

    # ---- offline changes ---------------------------------------------------
    Write-Host "==> Making changes while stopped"
    Invoke-OfflineChanges

    # ---- second start: catch-up --------------------------------------------
    Write-Host "==> Starting daemon (catch-up)"
    $running = Start-Daemon "catchup"
    $d2 = $running
    Add-Sample $d2 "catch-up"
    $decision2 = $null
    $decisionAt = $null
    $deadline = (Get-Date).AddSeconds($CrawlTimeout)
    while (-not $decision2) {
        if ($d2.Proc.HasExited) { throw "daemon exited during catch-up; see $($d2.Err)" }
        $decision2 = Find-Log $d2 '^full crawl: |^catch-up: |^index already built'
        if ($decision2) { $decisionAt = $d2.Clock.Elapsed.TotalSeconds; break }
        if ((Get-Date) -gt $deadline) { throw "no catch-up decision within $CrawlTimeout s" }
        Start-Sleep -Milliseconds 100
        Add-Sample $d2 "catch-up"
    }
    $fallback = Find-Log $d2 '^USN journal replay not possible'
    $path = if ($decision2 -match '^full crawl: ') { "full crawl" }
        elseif ($decision2 -match '^index already built') { "none (no change log)" }
        elseif ($fallback) { "timestamp scan" }
        else { "USN journal replay" }
    $timeout = if ($decision2 -match '^full crawl: ') { $CrawlTimeout } else { 300 }
    $allOk = Wait-Expectations $d2 $afterChanges $timeout "catch-up"
    $verifiedAt = $d2.Clock.Elapsed.TotalSeconds
    Close-Phase $d2 "catch-up"

    Say ""
    Say "---- catch-up after restart ----"
    Say "Path:         $path"
    if ($fallback) { Say "Fallback:     $fallback" }
    Say "Decision:     $decision2"
    Say ("Timing:       /health {0:N2}s, decision logged {1:N2}s, all changes visible {2}" -f `
        $d2.Ready, $decisionAt, $(if ($allOk) { "{0:N2}s" -f $verifiedAt } else { "NEVER (timed out)" }))
    Say "Checks:"
    foreach ($e in $afterChanges) {
        $mark = if ($e.Ok) { "ok  " } else { "FAIL" }
        $when = if ($e.Ok) { "at {0:N2}s" -f $e.At } else { "seen: $($e.Seen)" }
        Say ("  {0} {1,-32} {2}" -f $mark, $e.Desc, $when)
        $checkRows.Add([pscustomobject]@{ check = $e.Desc; name = $e.Name; ok = $e.Ok; at_s = $e.At; seen = $e.Seen })
    }

    Write-Host "==> Idle $IdleSeconds s"
    Wait-Idle $d2 "idle" $IdleSeconds
    Write-Host "==> Query latency after restart"
    Measure-Queries $d2 "queries"
    Write-Host "==> Stopping daemon"
    $stop2 = Stop-Daemon $d2
    $running = $null
    Say "Stop:         $stop2"

    # ---- report ------------------------------------------------------------
    Say ""
    Say "---- process (CPU % is of one core; MB are working set / private bytes) ----"
    Say ("{0,-8} {1,-14} {2,8} {3,8} {4,6} {5,9} {6,9} {7,9}" -f "run", "phase", "secs", "cpu_s", "cpu%", "peak_ws", "end_ws", "end_priv")
    foreach ($r in $phaseRows) {
        Say ("{0,-8} {1,-14} {2,8} {3,8} {4,6} {5,9} {6,9} {7,9}" -f `
            $r.run, $r.phase, $r.secs, $r.cpu_s, $r.cpu_pct, $r.peak_ws_mb, $r.end_ws_mb, $r.end_priv_mb)
    }
    $peak = ($samples | Measure-Object peak_ws_mb -Maximum).Maximum
    Say "Peak working set over the whole run: $peak MB"

    Say ""
    Say "---- query load: $Clients clients, $LoadSeconds s ----"
    Say ("Requests {0}, errors {1}, {2} req/s; latency ms p50 {3}, p95 {4}, p99 {5}, max {6}" -f `
        $load.Requests, $load.Errors, $load.Qps, $load.P50, $load.P95, $load.P99, $load.Max)

    Say ""
    Say "---- query latency ms ($Iterations iterations; client min/median/max, server avg) ----"
    Say ("{0,-8} {1,-40} {2,8} {3,8} {4,8} {5,8} {6,8}" -f "run", "query", "min", "med", "max", "server", "hits")
    foreach ($r in $queryRows) {
        $q = if ($r.query.Length -gt 40) { $r.query.Substring(0, 37) + "..." } else { $r.query }
        $line = "{0,-8} {1,-40} {2,8} {3,8} {4,8} {5,8} {6,8}" -f $r.run, $q, $r.min_ms, $r.med_ms, $r.max_ms, $r.server_ms, $r.hits
        if ($r.error) { $line += "  ERROR: $($r.error)" }
        Say $line
    }

    $failed = @($afterChanges | Where-Object { -not $_.Ok }).Count
    Say ""
    Say "Result: catch-up via $path, $($afterChanges.Count - $failed)/$($afterChanges.Count) checks passed"
    Say "======================================================================"
}
finally {
    if ($running -and -not $running.Proc.HasExited) {
        Stop-Process -Id $running.Proc.Id -Force -ErrorAction SilentlyContinue
        $running.Proc.WaitForExit(10000) | Out-Null
    }
    $samples   | Export-Csv -NoTypeInformation -Path (Join-Path $OutDir "samples.csv")
    $phaseRows | Export-Csv -NoTypeInformation -Path (Join-Path $OutDir "phases.csv")
    $queryRows | Export-Csv -NoTypeInformation -Path (Join-Path $OutDir "queries.csv")
    $checkRows | Export-Csv -NoTypeInformation -Path (Join-Path $OutDir "checks.csv")
    $report    | Set-Content -Path (Join-Path $OutDir "summary.txt") -Encoding UTF8
    Copy-Item $config $OutDir -ErrorAction SilentlyContinue
    Copy-Item (Join-Path $workDir "logs\daemon.*.log") $OutDir -ErrorAction SilentlyContinue
    if (-not $keepData) {
        Remove-Item -Recurse -Force $workDir -ErrorAction SilentlyContinue
    } else {
        # Fixture names carry a per-run tag, so old ones would only pile up
        # in the kept index; the next run catches up their removal.
        Remove-Item -Recurse -Force (Join-Path $workDir "fixture\*"), (Join-Path $workDir "outside\*") -ErrorAction SilentlyContinue
    }
    Write-Host ""
    Write-Host "Results: $OutDir"
}
