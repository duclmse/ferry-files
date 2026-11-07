#Requires -Version 5.1
# clean.ps1 - interactive disk cleanup for a Windows dev machine.
# Covers Node/pnpm, Docker, Rust/Go/Java/Python toolchains, NuGet/Chocolatey/
# Scoop, Visual Studio caches, and Windows system junk (Recycle Bin, Temp,
# Windows Update download cache). Not specific to this repo - it cleans up
# your whole machine, same as any other dev-machine housekeeping script.
#
# Usage:
#   .\clean.ps1              interactive mode menu
#   .\clean.ps1 -Report      scan only, never deletes anything
#   .\clean.ps1 -Confirm     per-category y/n before each delete (default when interactive)
#   .\clean.ps1 -Force       delete every "safe" item with no prompts
#
# A few categories are ALWAYS confirmed individually regardless of mode
# (Docker volumes, emptying the Recycle Bin) because they can destroy data
# that isn't just a regenerable cache. -Force does not skip these.
#
# Some items (Windows Update download cache, DISM component cleanup) need an
# elevated (Run as Administrator) PowerShell session; this script reports
# their size either way but skips the actual cleanup command if not elevated.
param(
    [switch]$Report,
    [switch]$Confirm,
    [switch]$Force
)

$ErrorActionPreference = "Continue"

# ---- search roots for node_modules / target / venv discovery ----
# Edit this list to match where your projects actually live.
$SearchRoots = @("$HOME\projects", "$HOME\source", "$HOME\Documents") |
    Where-Object { Test-Path $_ }

$modeCount = @($Report, $Confirm, $Force) | Where-Object { $_ } | Measure-Object | Select-Object -ExpandProperty Count
if ($modeCount -gt 1) {
    Write-Error "pass only one of -Report, -Confirm, -Force"
    exit 1
}

$Mode = if ($Report) { "report" } elseif ($Confirm) { "confirm" } elseif ($Force) { "force" } else { "" }

if (-not $Mode) {
    Write-Host "How should this run behave?"
    Write-Host "  1) Report only   - scan and print sizes, delete nothing"
    Write-Host "  2) Confirm each  - show what would be deleted, ask y/n per category (recommended)"
    Write-Host "  3) Force         - delete every 'safe' item automatically, no prompts"
    $choice = Read-Host "Choose [1/2/3]"
    $Mode = switch ($choice) {
        "1" { "report" }
        "2" { "confirm" }
        "3" { "force" }
        default { Write-Error "Invalid choice."; exit 1 }
    }
}

$isElevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)

Write-Host "Mode: $Mode"
Write-Host "Elevated: $isElevated"
Write-Host ""

function Show-SectionHeader($title) {
    Write-Host ""
    Write-Host "== $title =="
}

function Confirm-Action($prompt) {
    $ans = Read-Host "$prompt [y/N]"
    return $ans -match '^[Yy]$'
}

function Get-SizeDisplay($path) {
    if (-not (Test-Path $path)) { return $null }
    $bytes = (Get-ChildItem -Path $path -Recurse -Force -ErrorAction SilentlyContinue | Measure-Object -Property Length -Sum).Sum
    if (-not $bytes) { return "0B" }
    $units = "B", "KB", "MB", "GB", "TB"
    $i = 0
    while ($bytes -ge 1024 -and $i -lt $units.Length - 1) { $bytes /= 1024; $i++ }
    return "{0:N1}{1}" -f $bytes, $units[$i]
}

# Delete a path outright (dir + contents), or just its contents if $DelMode = "contents".
function Remove-Target($name, $path, $delMode = "dir") {
    if (-not (Test-Path $path)) {
        Write-Host ("  {0,-42} (not found)" -f $name)
        return
    }
    $size = Get-SizeDisplay $path
    Write-Host ("  {0,-42} {1,8}  {2}" -f $name, $size, $path)
    switch ($Mode) {
        "report" { return }
        "confirm" { if (-not (Confirm-Action "    Delete?")) { return } }
    }
    if ($delMode -eq "contents") {
        Get-ChildItem -Path $path -Force -ErrorAction SilentlyContinue |
            Remove-Item -Recurse -Force -ErrorAction SilentlyContinue
    } else {
        Remove-Item -Path $path -Recurse -Force -ErrorAction SilentlyContinue
    }
    Write-Host "    -> deleted"
}

# Show size at $sizePath (if it exists) then run a cleanup scriptblock instead of Remove-Item.
function Invoke-CleanupCommand($name, $sizePath, $cmdText, [scriptblock]$action, [bool]$requiresElevation = $false) {
    $size = "-"
    if ($sizePath -and (Test-Path $sizePath)) { $size = Get-SizeDisplay $sizePath }
    Write-Host ("  {0,-42} {1,8}" -f $name, $size)
    Write-Host "    cmd: $cmdText"
    if ($requiresElevation -and -not $isElevated) {
        Write-Host "    (skipped: requires an elevated/Administrator PowerShell session)"
        return
    }
    switch ($Mode) {
        "report" { return }
        "confirm" { if (-not (Confirm-Action "    Run it?")) { return } }
    }
    & $action
}

# List every path in $paths, one combined y/n, then bulk-delete.
function Remove-BulkTargets($title, [string[]]$paths) {
    Show-SectionHeader $title
    if ($paths.Count -eq 0) {
        Write-Host "  none found under: $($SearchRoots -join ', ')"
        return
    }
    foreach ($p in $paths) {
        Write-Host ("  {0,8}  {1}" -f (Get-SizeDisplay $p), $p)
    }
    Write-Host "  $($paths.Count) item(s) found."
    switch ($Mode) {
        "report" { return }
        "confirm" { if (-not (Confirm-Action "  Delete ALL $($paths.Count) listed above?")) { return } }
    }
    foreach ($p in $paths) { Remove-Item -Path $p -Recurse -Force -ErrorAction SilentlyContinue }
    Write-Host "  -> deleted $($paths.Count) item(s)"
}

$sysDrive = (Get-PSDrive -Name ($env:SystemDrive.TrimEnd(':')))
$freeBefore = "{0:N2} GB" -f ($sysDrive.Free / 1GB)

# =====================================================================
Show-SectionHeader "Dev tool caches"
# =====================================================================

if (Get-Command npm -ErrorAction SilentlyContinue) {
    Invoke-CleanupCommand "npm cache" "$env:APPDATA\npm-cache" "npm cache clean --force" { npm cache clean --force }
}
if (Get-Command pnpm -ErrorAction SilentlyContinue) {
    $pnpmStore = (pnpm store path 2>$null)
    Invoke-CleanupCommand "pnpm store" $pnpmStore "pnpm store prune" { pnpm store prune }
}
if (Get-Command yarn -ErrorAction SilentlyContinue) {
    $yarnCache = (yarn cache dir 2>$null)
    Invoke-CleanupCommand "yarn cache" $yarnCache "yarn cache clean" { yarn cache clean }
}
if (Get-Command choco -ErrorAction SilentlyContinue) {
    Invoke-CleanupCommand "Chocolatey cache" "$env:ChocolateyInstall\cache" "choco cache remove" { choco cache remove } -requiresElevation $true
}
if (Get-Command scoop -ErrorAction SilentlyContinue) {
    Invoke-CleanupCommand "Scoop cache" "$HOME\scoop\cache" "scoop cache rm *" { scoop cache rm * }
}
if (Get-Command dotnet -ErrorAction SilentlyContinue) {
    Invoke-CleanupCommand "NuGet caches" "$HOME\.nuget\packages" "dotnet nuget locals all --clear" { dotnet nuget locals all --clear }
}
Remove-Target "Visual Studio ComponentModelCache" "$env:LOCALAPPDATA\Microsoft\VisualStudio" contents

# =====================================================================
Show-SectionHeader "Java / Rust / Go / Python package caches"
# =====================================================================

Remove-Target "Maven repository (~\.m2\repository)" "$HOME\.m2\repository" contents

Remove-Target "Cargo registry cache" "$HOME\.cargo\registry\cache" contents
Remove-Target "Cargo registry src"   "$HOME\.cargo\registry\src"   contents
Remove-Target "Cargo git checkouts"  "$HOME\.cargo\git"            contents

if (Get-Command go -ErrorAction SilentlyContinue) {
    $goCache    = (go env GOCACHE 2>$null)
    $goModCache = (go env GOMODCACHE 2>$null)
    Invoke-CleanupCommand "Go build cache"  $goCache    "go clean -cache"    { go clean -cache }
    Invoke-CleanupCommand "Go module cache" $goModCache "go clean -modcache" { go clean -modcache }
}
if (Get-Command pip -ErrorAction SilentlyContinue) {
    $pipCache = (pip cache dir 2>$null)
    Invoke-CleanupCommand "pip cache" $pipCache "pip cache purge" { pip cache purge }
}
if (Get-Command conda -ErrorAction SilentlyContinue) {
    Invoke-CleanupCommand "conda caches (pkgs, tarballs)" "$HOME\.conda\pkgs" "conda clean --all --yes" { conda clean --all --yes }
}
if (Get-Command gem -ErrorAction SilentlyContinue) {
    Invoke-CleanupCommand "gem cache" "$HOME\.gem" "gem cleanup" { gem cleanup }
}

# =====================================================================
Show-SectionHeader "Docker"
# =====================================================================

if ((Get-Command docker -ErrorAction SilentlyContinue) -and (docker info *>$null; $LASTEXITCODE -eq 0)) {
    Write-Host "  docker system df:"
    (docker system df 2>$null) | ForEach-Object { "    $_" } | Write-Host

    Invoke-CleanupCommand "Dangling images"    "-" "docker image prune -f"     { docker image prune -f }
    Invoke-CleanupCommand "Stopped containers" "-" "docker container prune -f" { docker container prune -f }
    Invoke-CleanupCommand "Build cache"        "-" "docker builder prune -f"   { docker builder prune -f }
    Invoke-CleanupCommand "Unused networks"    "-" "docker network prune -f"   { docker network prune -f }

    Write-Host ""
    Write-Host "  ! Docker volumes may hold a project's local Postgres/Redis data."
    Write-Host "    Unused (unattached) volumes only - attached ones are never touched - but"
    Write-Host "    always requires an explicit confirmation, in every mode:"
    (docker volume ls -f dangling=true 2>$null) | ForEach-Object { "    $_" } | Write-Host
    if (Confirm-Action "  Prune dangling Docker volumes?") {
        docker volume prune -f
    } else {
        Write-Host "  skipped"
    }
} else {
    Write-Host "  docker not running or not installed, skipping"
}

# =====================================================================
Show-SectionHeader "node_modules / target / venv across $($SearchRoots -join ', ')"
# =====================================================================

# Get-ChildItem -Recurse has no equivalent to find's -prune: it keeps
# descending into a matched directory looking for further matches, which is
# wasted work inside a multi-GB node_modules/target tree. Walk manually
# instead, and stop descending as soon as a directory name matches.
function Find-PrunedDirs {
    param([string[]]$Roots, [string[]]$Names)
    $result = New-Object System.Collections.Generic.List[string]
    $stack  = New-Object System.Collections.Generic.Stack[string]
    foreach ($r in $Roots) { $stack.Push($r) }
    while ($stack.Count -gt 0) {
        $dir = $stack.Pop()
        $children = Get-ChildItem -Path $dir -Directory -Force -ErrorAction SilentlyContinue
        foreach ($c in $children) {
            if ($Names -contains $c.Name) {
                $result.Add($c.FullName)
            } else {
                $stack.Push($c.FullName)
            }
        }
    }
    return $result
}

if ($SearchRoots.Count -gt 0) {
    $nmDirs = Find-PrunedDirs -Roots $SearchRoots -Names @("node_modules")
    Remove-BulkTargets "node_modules folders (reinstallable via pnpm/npm/yarn install)" $nmDirs

    $targetDirs = Find-PrunedDirs -Roots $SearchRoots -Names @("target") |
        Where-Object { (Test-Path (Join-Path (Split-Path $_ -Parent) "Cargo.toml")) -or (Test-Path (Join-Path (Split-Path $_ -Parent) "pom.xml")) }
    Remove-BulkTargets "Rust/Maven 'target' build output dirs" $targetDirs

    $venvDirs = Find-PrunedDirs -Roots $SearchRoots -Names @(".venv", "venv")
    Remove-BulkTargets "Python venv folders" $venvDirs
} else {
    Write-Host "  none of `$SearchRoots exist on this machine - edit `$SearchRoots at the top of this script"
}

# =====================================================================
Show-SectionHeader "Windows system junk"
# =====================================================================

Write-Host "  Recycle Bin:"
try {
    $shell = New-Object -ComObject Shell.Application
    $recycleBin = $shell.Namespace(0xA)
    $items = $recycleBin.Items()
    $sizeBytes = ($items | ForEach-Object { $_.ExtendedProperty("Size") } | Measure-Object -Sum).Sum
    Write-Host ("    {0} item(s), {1:N2} GB" -f $items.Count, ($sizeBytes / 1GB))
} catch {
    Write-Host "    (unable to enumerate Recycle Bin)"
}
switch ($Mode) {
    "report" { }
    default {
        if (Confirm-Action "  Empty the Recycle Bin?") {
            Clear-RecycleBin -Force -ErrorAction SilentlyContinue
            Write-Host "    -> emptied"
        } else {
            Write-Host "  skipped"
        }
    }
}

Remove-Target "User Temp (%TEMP%)" $env:TEMP contents
Remove-Target "Windows Temp (C:\Windows\Temp)" "$env:SystemRoot\Temp" contents
Remove-Target "Windows Error Reporting queue" "$env:LOCALAPPDATA\Microsoft\Windows\WER" contents
Remove-Target "Windows Explorer thumbnail cache" "$env:LOCALAPPDATA\Microsoft\Windows\Explorer" contents

Invoke-CleanupCommand "Windows Update download cache" "$env:SystemRoot\SoftwareDistribution\Download" `
    "net stop wuauserv; Remove-Item ...\SoftwareDistribution\Download\*; net start wuauserv" `
    {
        net stop wuauserv | Out-Null
        Remove-Item -Path "$env:SystemRoot\SoftwareDistribution\Download\*" -Recurse -Force -ErrorAction SilentlyContinue
        net start wuauserv | Out-Null
    } -requiresElevation $true

Invoke-CleanupCommand "DISM component store cleanup (superseded WinSxS files)" $null `
    "Dism.exe /Online /Cleanup-Image /StartComponentCleanup" `
    { Dism.exe /Online /Cleanup-Image /StartComponentCleanup } -requiresElevation $true

if (Test-Path "$env:SystemDrive\Windows.old") {
    Write-Host ""
    Write-Host "  Windows.old (previous Windows install, kept for ~10 days after an upgrade):"
    Write-Host ("    {0}" -f (Get-SizeDisplay "$env:SystemDrive\Windows.old"))
    Write-Host "  ! Only removable via Disk Cleanup / 'Cleanup system files' (Settings > Storage) or"
    Write-Host "    'Dism.exe /Online /Cleanup-Image /SPSuperseded'; not deleted directly by this script."
}

# =====================================================================
$freeAfter = "{0:N2} GB" -f ((Get-PSDrive -Name ($env:SystemDrive.TrimEnd(':'))).Free / 1GB)
Show-SectionHeader "Done"
Write-Host "  Free space before: $freeBefore"
Write-Host "  Free space after:  $freeAfter"
