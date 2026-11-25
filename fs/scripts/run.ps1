#Requires -Version 5.1
# Run fs in the foreground (direct mode, not via the Windows Service).
# Override discovery with $env:FS_BIN / $env:FS_CONFIG.
$ErrorActionPreference = "Stop"

Set-Location (Join-Path $PSScriptRoot "..")

$bin = $env:FS_BIN
if (-not $bin) {
    if (Test-Path "..\target\release\fsd.exe") {
        $bin = "..\target\release\fsd.exe"
    } elseif (Get-Command fsd.exe -ErrorAction SilentlyContinue) {
        $bin = (Get-Command fsd.exe).Source
    } else {
        Write-Error "fsd.exe not found. Run scripts\build.ps1 or scripts\install.ps1 first."
        exit 1
    }
}

$config = $env:FS_CONFIG
if (-not $config) {
    foreach ($candidate in @(".\config.toml", "$env:ProgramFiles\fs\config.toml")) {
        if (Test-Path $candidate) { $config = $candidate; break }
    }
}
if (-not $config) {
    Write-Error "No config.toml found. Run scripts\install.ps1, or set `$env:FS_CONFIG."
    exit 1
}

Write-Host "==> Binary: $bin"
Write-Host "==> Config: $config"
& $bin --config $config daemon
exit $LASTEXITCODE
