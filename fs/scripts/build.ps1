#Requires -Version 5.1
# Build fsd (with the rar2zip extension) and fs in release mode.
$ErrorActionPreference = "Stop"

Set-Location (Join-Path $PSScriptRoot "..")

Write-Host "==> Building fsd + fs (release)"
cargo build --release -p fs-server -p fs-cli --features fs-server/rar2zip
if ($LASTEXITCODE -ne 0) {
    Write-Error "cargo build failed"
    exit $LASTEXITCODE
}

Write-Host "==> Done: target\release\fsd.exe, target\release\fs.exe"
