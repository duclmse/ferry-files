#Requires -Version 5.1
# Build filesearch and rar2zip in release mode.
$ErrorActionPreference = "Stop"

Set-Location (Join-Path $PSScriptRoot "..")

Write-Host "==> Building filesearch + rar2zip (release)"
cargo build --release --workspace
if ($LASTEXITCODE -ne 0) {
    Write-Error "cargo build failed"
    exit $LASTEXITCODE
}

Write-Host "==> Done: target\release\filesearch.exe, target\release\rar2zip.exe"
