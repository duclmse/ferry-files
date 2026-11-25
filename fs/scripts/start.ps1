#Requires -Version 5.1
# Start fsd unless it is already running.
#
#   - Installed as the FsDaemon service: starts the service if it is stopped.
#   - Otherwise: starts `fsd daemon` in the background, with no window, and
#     waits until it answers over IPC.
#
# Override discovery with $env:FS_BIN / $env:FS_CONFIG, as for run.ps1. With
# neither $env:FS_CONFIG nor a config.toml, it uses scripts\config.default.toml.
# Stop it with scripts\stop.ps1 (or `fsd --config <config> stop`), which
# lets it save its index first.
param(
    [int]$WaitSeconds = 30
)

$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

$service = Get-Service FsDaemon -ErrorAction SilentlyContinue
if ($service) {
    if ($service.Status -eq "Running") {
        Write-Host "==> fsd is running as the FsDaemon service"
        exit 0
    }
    Write-Host "==> Starting the FsDaemon service (it is $($service.Status))"
    try {
        Start-Service FsDaemon
        $service.WaitForStatus("Running", [TimeSpan]::FromSeconds($WaitSeconds))
    } catch {
        Write-Error "Could not start FsDaemon: $($_.Exception.Message). Starting a service needs an elevated PowerShell."
        exit 1
    }
    Write-Host "==> FsDaemon is running"
    exit 0
}

$running = Get-Process fsd -ErrorAction SilentlyContinue
if ($running) {
    Write-Host "==> fsd is already running (PID $(($running | ForEach-Object Id) -join ', '))"
    exit 0
}

$bin = $env:FS_BIN
if (-not $bin) {
    foreach ($candidate in @("..\target\release\fsd.exe", "$env:ProgramFiles\fs\fsd.exe")) {
        if (Test-Path $candidate) { $bin = $candidate; break }
    }
    if (-not $bin -and (Get-Command fsd.exe -ErrorAction SilentlyContinue)) {
        $bin = (Get-Command fsd.exe).Source
    }
}
if (-not $bin) {
    Write-Error "fsd.exe not found. Run scripts\build.ps1 or scripts\install.ps1 first."
    exit 1
}
$bin = (Resolve-Path $bin).Path

$config = $env:FS_CONFIG
if (-not $config) {
    foreach ($candidate in @(".\config.toml", "$env:ProgramFiles\fs\config.toml", "scripts\config.default.toml")) {
        if (Test-Path $candidate) { $config = $candidate; break }
    }
}
if (-not $config) {
    Write-Error "No config found. Run scripts\install.ps1, or set `$env:FS_CONFIG."
    exit 1
}
$config = (Resolve-Path $config).Path

& $bin --config $config validate-config | Out-Null
if ($LASTEXITCODE -ne 0) {
    Write-Error "$config is not valid; see: $bin --config $config validate-config"
    exit 1
}

# fsd logs to the config's log_dir; these catch only what it prints before
# its logger is up, such as a config it cannot read.
$out = Join-Path $env:TEMP "fsd.out.txt"
$err = Join-Path $env:TEMP "fsd.err.txt"
Write-Host "==> Starting $bin with $config"
$proc = Start-Process -FilePath $bin -ArgumentList "--config", "`"$config`"", "daemon" `
    -WindowStyle Hidden -PassThru -RedirectStandardOutput $out -RedirectStandardError $err

$fs = Join-Path (Split-Path $bin) "fs.exe"
$deadline = (Get-Date).AddSeconds($WaitSeconds)
while ((Get-Date) -lt $deadline) {
    Start-Sleep -Milliseconds 500
    if ($proc.HasExited) {
        Write-Error "fsd exited with code $($proc.ExitCode). See $err and the config's log_dir."
        exit 1
    }
    if (-not (Test-Path $fs)) { break }
    & $fs --config $config stats *> $null
    if ($LASTEXITCODE -eq 0) {
        Write-Host "==> fsd is running (PID $($proc.Id)) and answering"
        exit 0
    }
}
if (Test-Path $fs) {
    # IPC may be off in the config (api.ipc_enabled = false).
    Write-Warning "fsd is running (PID $($proc.Id)) but did not answer over IPC within $WaitSeconds s"
} else {
    Write-Host "==> fsd is running (PID $($proc.Id))"
}
exit 0
