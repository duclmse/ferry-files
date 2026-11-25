#Requires -Version 5.1
# Build fsd, fs and the rar2zip and sftpd extensions in release mode.
#
# A running fsd.exe from target\release locks it and its extensions, so the
# build stops that fsd first (scripts\stop.ps1 -Force) and starts it again
# after with the same config (scripts\start.ps1). -NoRestart leaves it
# stopped.
param(
    [switch]$NoRestart
)

$ErrorActionPreference = "Stop"

Set-Location (Join-Path $PSScriptRoot "..")

$release = Join-Path (Get-Location) "target\release"
$running = @(Get-CimInstance Win32_Process -Filter "Name = 'fsd.exe'" |
    Where-Object { $_.ExecutablePath -and $_.ExecutablePath.StartsWith("$release\", "OrdinalIgnoreCase") })
$config = $null
if ($running.Count -gt 0) {
    if ($running[0].CommandLine -match '(?:--config|-c)\s+(?:"([^"]+)"|(\S+))') {
        $config = if ($Matches[1]) { $Matches[1] } else { $Matches[2] }
    }
    Write-Host "==> fsd from target\release is running; stopping it for the build"
    & (Join-Path $PSScriptRoot "stop.ps1") -Force -ProcessId ($running | ForEach-Object ProcessId)
    if ($LASTEXITCODE -ne 0) {
        Write-Error "Could not stop fsd; the build would fail on its locked files"
        exit 1
    }
}

Write-Host "==> Building fsd + fs + rar2zip + sftpd (release)"
cargo build --release -p fs-server -p fs-cli -p fs-ext-rar2zip -p fs-ext-sftpd
$code = $LASTEXITCODE

if ($running.Count -gt 0 -and -not $NoRestart) {
    Write-Host "==> Starting fsd again"
    $env:FS_BIN = Join-Path $release "fsd.exe"
    if ($config) { $env:FS_CONFIG = $config }
    & (Join-Path $PSScriptRoot "start.ps1")
}

if ($code -ne 0) {
    Write-Error "cargo build failed"
    exit $code
}

Write-Host "==> Done: target\release\fsd.exe, target\release\fs.exe, target\release\fs-ext-rar2zip.exe, target\release\fs-ext-sftpd.exe"
