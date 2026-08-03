#Requires -Version 5.1
# Run filesearch in the foreground (direct mode, not via the Windows Service).
# Override discovery with $env:FILESEARCH_BIN / $env:FILESEARCH_CONFIG.
$ErrorActionPreference = "Stop"

Set-Location (Join-Path $PSScriptRoot "..")

$bin = $env:FILESEARCH_BIN
if (-not $bin) {
    if (Test-Path "target\release\filesearch.exe") {
        $bin = "target\release\filesearch.exe"
    } elseif (Get-Command filesearch.exe -ErrorAction SilentlyContinue) {
        $bin = (Get-Command filesearch.exe).Source
    } else {
        Write-Error "filesearch.exe not found. Run scripts\build.ps1 or scripts\install.ps1 first."
        exit 1
    }
}

$config = $env:FILESEARCH_CONFIG
if (-not $config) {
    foreach ($candidate in @(".\config.toml", "$env:ProgramFiles\filesearch\config.toml")) {
        if (Test-Path $candidate) { $config = $candidate; break }
    }
}
if (-not $config) {
    Write-Error "No config.toml found. Run scripts\install.ps1, or set `$env:FILESEARCH_CONFIG."
    exit 1
}

Write-Host "==> Binary: $bin"
Write-Host "==> Config: $config"
& $bin --config $config daemon
exit $LASTEXITCODE
