#Requires -Version 5.1
# Build fsd, fs and the rar2zip and sftpd extensions in release mode.
$ErrorActionPreference = "Stop"

Set-Location (Join-Path $PSScriptRoot "..")

Write-Host "==> Building fsd + fs + rar2zip + sftpd (release)"
cargo build --release -p fs-server -p fs-cli -p fs-ext-rar2zip -p fs-ext-sftpd
if ($LASTEXITCODE -ne 0) {
    Write-Error "cargo build failed"
    exit $LASTEXITCODE
}

Write-Host "==> Done: target\release\fsd.exe, target\release\fs.exe, target\release\fs-ext-rar2zip.exe, target\release\fs-ext-sftpd.exe"
