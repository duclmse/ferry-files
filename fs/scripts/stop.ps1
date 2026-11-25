#Requires -Version 5.1
# Stop fsd, cleanly, wherever it runs.
#
#   - The FsDaemon service: stops the service (needs an elevated PowerShell).
#   - A background fsd (scripts\start.ps1): runs `fsd --config <its config>
#     stop` for each one, which lets it save its index first.
#
# -ProcessId stops only those fsd processes (the service too, if it is one).
# -Force kills an fsd that does not stop that way: one built before
# `fsd stop` existed, or one that is still busy after -WaitSeconds. A killed
# memory store loses what changed since its last snapshot and catches up on
# the next start.
param(
    [int]$WaitSeconds = 60,
    [int[]]$ProcessId = @(),
    [switch]$Force
)

$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

$service = Get-Service FsDaemon -ErrorAction SilentlyContinue
if ($service -and $ProcessId.Count -gt 0) {
    $servicePid = (Get-CimInstance Win32_Service -Filter "Name = 'FsDaemon'").ProcessId
    if ($ProcessId -notcontains $servicePid) { $service = $null }
}
if ($service -and $service.Status -ne "Stopped") {
    Write-Host "==> Stopping the FsDaemon service"
    try {
        Stop-Service FsDaemon
        $service.WaitForStatus("Stopped", [TimeSpan]::FromSeconds($WaitSeconds))
    } catch {
        Write-Error "Could not stop FsDaemon: $($_.Exception.Message). Stopping a service needs an elevated PowerShell."
        exit 1
    }
    Write-Host "==> FsDaemon is stopped"
}

$procs = @(Get-CimInstance Win32_Process -Filter "Name = 'fsd.exe'" |
    Where-Object { $ProcessId.Count -eq 0 -or $ProcessId -contains $_.ProcessId })
if ($procs.Count -eq 0) {
    if (-not $service) { Write-Host "==> fsd is not running" }
    exit 0
}

# The config is read from each one's command line, so `fsd stop` finds its
# data folder. A relative one is read from the repository root; start.ps1
# and the service pass absolute paths.
function Get-ConfigArg([string]$commandLine) {
    if ($commandLine -match '(?:--config|-c)\s+(?:"([^"]+)"|(\S+))') {
        if ($Matches[1]) { return $Matches[1] } else { return $Matches[2] }
    }
    return "config.toml"
}

$failed = $false
foreach ($p in $procs) {
    $exe = $p.ExecutablePath
    $config = Get-ConfigArg $p.CommandLine
    Write-Host "==> Stopping fsd (PID $($p.ProcessId)) with $config"
    $stopped = $false
    if ($exe -and (Test-Path $exe)) {
        & $exe --config $config stop --wait $WaitSeconds
        $stopped = ($LASTEXITCODE -eq 0) -or -not (Get-Process -Id $p.ProcessId -ErrorAction SilentlyContinue)
    }
    if (-not $stopped) {
        if ($Force) {
            Write-Warning "Killing fsd (PID $($p.ProcessId))"
            Stop-Process -Id $p.ProcessId -Force
            Wait-Process -Id $p.ProcessId -Timeout 10 -ErrorAction SilentlyContinue
        } else {
            Write-Warning "fsd (PID $($p.ProcessId)) did not stop; rerun with -Force to kill it"
            $failed = $true
        }
    }
}
if ($failed) { exit 1 }
Write-Host "==> fsd is stopped"
exit 0
