#Requires -Version 5.1
# Build fsd, fs and the rar2zip extension in release mode.
$ErrorActionPreference = "Stop"

Set-Location (Join-Path $PSScriptRoot "..")

Write-Host "==> Building fsd + fs + rar2zip (release)"
cargo build --release -p fs-server -p fs-cli -p fs-ext-rar2zip
if ($LASTEXITCODE -ne 0) {
    Write-Error "cargo build failed"
    exit $LASTEXITCODE
}

Write-Host "==> Done: target\release\fsd.exe, target\release\fs.exe, target\release\fs-ext-rar2zip.exe"
