#Requires -RunAsAdministrator
<#
.SYNOPSIS
    Install or uninstall the rar2zip Windows Service.

.PARAMETER BinaryPath
    Path to the rar2zip.exe binary. Defaults to .\rar2zip.exe.

.PARAMETER ConfigPath
    Path to the TOML configuration file. Defaults to .\config.toml.

.PARAMETER ServiceName
    Windows Service name. Defaults to "rar2zip".

.PARAMETER Uninstall
    Stop and delete the service instead of creating it.

.EXAMPLE
    # Install
    .\install-service.ps1 -BinaryPath "C:\tools\rar2zip.exe" -ConfigPath "C:\tools\config.toml"

.EXAMPLE
    # Uninstall
    .\install-service.ps1 -Uninstall
#>
param(
    [string]$BinaryPath  = ".\rar2zip.exe",
    [string]$ConfigPath  = ".\config.toml",
    [string]$ServiceName = "rar2zip",
    [switch]$Uninstall
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

if ($Uninstall) {
    Write-Host "Stopping service '$ServiceName'..."
    sc.exe stop $ServiceName | Out-Null
    Start-Sleep -Seconds 2
    Write-Host "Deleting service '$ServiceName'..."
    sc.exe delete $ServiceName
    Write-Host "Service '$ServiceName' removed."
    exit 0
}

$BinaryPath = (Resolve-Path $BinaryPath).Path
$ConfigPath = (Resolve-Path $ConfigPath).Path

$binPath = "`"$BinaryPath`" run --service --config `"$ConfigPath`""

Write-Host "Creating service '$ServiceName'..."
sc.exe create $ServiceName `
    binPath= $binPath `
    start= auto `
    DisplayName= "RAR to ZIP Transfer Service"

Write-Host "Configuring restart policy..."
sc.exe failure $ServiceName reset= 60 actions= restart/5000/restart/10000/restart/30000

Write-Host "Starting service '$ServiceName'..."
sc.exe start $ServiceName

Write-Host "Done. Use 'sc query $ServiceName' to check status."
