#Requires -Version 5.1
# worktree.ps1 - create and manage git worktrees for this repo, plus a few
# helpers for the rebase/merge conflicts that come with juggling several of
# them. Worktrees live as siblings of the repo, under
# ..\<repo-name>-worktrees\<branch-sanitized>, so they never pollute the
# main checkout or get walked by scripts\bench.ps1 et al.
#
# Usage:
#   scripts\worktree.ps1 add <branch> [base]
#   scripts\worktree.ps1 list
#   scripts\worktree.ps1 remove <branch> [-Force] [-DeleteBranch]
#   scripts\worktree.ps1 prune
#   scripts\worktree.ps1 status
#   scripts\worktree.ps1 sync <branch> [-Onto <ref>] [-Merge]
#   scripts\worktree.ps1 conflicts [path]
#   scripts\worktree.ps1 continue
#   scripts\worktree.ps1 abort
#
# `add` creates a new local branch off `base` (default: current HEAD) if
# <branch> doesn't already exist, otherwise it checks the existing branch
# out into a new worktree. `remove` refuses a worktree with uncommitted
# changes unless -Force is passed; -DeleteBranch also deletes the local
# branch afterward (requires it to be fully merged, unless -Force is also
# given). `sync` defaults to rebase against the branch's upstream, or
# -Onto <ref> to rebase/merge onto a specific ref; on conflict it stops and
# tells you to resolve with `conflicts` then `continue`/`abort`.
param(
    [Parameter(Position = 0, Mandatory = $true)]
    [ValidateSet("add", "list", "remove", "prune", "status", "sync", "conflicts", "continue", "abort")]
    [string]$Command,

    [Parameter(Position = 1)]
    [string]$Arg1 = "",

    [Parameter(Position = 2)]
    [string]$Arg2 = "",

    [switch]$Force,
    [switch]$DeleteBranch,
    [string]$Onto = "",
    [switch]$Merge
)

$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")
$RepoRoot = (Get-Location).Path
$RepoName = Split-Path -Leaf $RepoRoot
$WorktreeRoot = Join-Path (Split-Path -Parent $RepoRoot) "$RepoName-worktrees"

function ConvertTo-DirName($branch) {
    return $branch -replace "/", "-"
}

function Get-WorktreeEntries {
    $raw = git worktree list --porcelain
    $entries = @()
    $path = $null
    $branch = $null
    foreach ($line in $raw) {
        if ($line -match '^worktree (.+)$') {
            $path = $Matches[1]
        } elseif ($line -match '^branch refs/heads/(.+)$') {
            $branch = $Matches[1]
            $entries += [PSCustomObject]@{ Path = $path; Branch = $branch }
            $path = $null; $branch = $null
        } elseif ($line -match '^detached$') {
            $entries += [PSCustomObject]@{ Path = $path; Branch = "(detached)" }
            $path = $null
        }
    }
    return $entries
}

function Resolve-WorktreePath($target) {
    $entries = Get-WorktreeEntries
    $match = $entries | Where-Object { $_.Path -eq $target -or $_.Branch -eq $target } | Select-Object -First 1
    if (-not $match) {
        Write-Error "no worktree matches: $target"
        exit 1
    }
    return $match.Path
}

function Get-InProgressKind($path) {
    $gitDir = (git -C $path rev-parse --git-dir).Trim()
    $gitDirFull = if ([System.IO.Path]::IsPathRooted($gitDir)) { $gitDir } else { Join-Path $path $gitDir }
    if ((Test-Path (Join-Path $gitDirFull "rebase-merge")) -or (Test-Path (Join-Path $gitDirFull "rebase-apply"))) {
        return "rebase"
    } elseif (Test-Path (Join-Path $gitDirFull "MERGE_HEAD")) {
        return "merge"
    }
    return ""
}

switch ($Command) {
    "add" {
        if (-not $Arg1) { Write-Error "usage: worktree.ps1 add <branch> [base]"; exit 1 }
        $branch = $Arg1
        $base = if ($Arg2) { $Arg2 } else { "HEAD" }
        $dir = Join-Path $WorktreeRoot (ConvertTo-DirName $branch)

        if (Test-Path $dir) {
            Write-Error "worktree dir already exists: $dir"
            exit 1
        }
        New-Item -ItemType Directory -Force -Path $WorktreeRoot | Out-Null

        git show-ref --verify --quiet "refs/heads/$branch"
        if ($LASTEXITCODE -eq 0) {
            Write-Host "==> checking out existing branch '$branch' into $dir"
            git worktree add $dir $branch
        } else {
            Write-Host "==> creating new branch '$branch' from '$base' at $dir"
            git worktree add -b $branch $dir $base
        }
        Write-Host $dir
    }

    "list" {
        $entries = Get-WorktreeEntries
        foreach ($e in $entries) {
            $headFull = git -C $e.Path rev-parse HEAD
            $head = $headFull.Substring(0, 10)
            $porcelain = git -C $e.Path status --porcelain
            $dirty = if ($porcelain) { "dirty" } else { "clean" }
            "{0,-10} {1,-30} {2,-10} {3}" -f $head, $e.Branch, $dirty, $e.Path
        }
    }

    "remove" {
        if (-not $Arg1) { Write-Error "usage: worktree.ps1 remove <branch|path> [-Force] [-DeleteBranch]"; exit 1 }
        $path = Resolve-WorktreePath $Arg1
        $branch = (git -C $path rev-parse --abbrev-ref HEAD).Trim()

        $porcelain = git -C $path status --porcelain
        if ($porcelain -and -not $Force) {
            Write-Error "worktree has uncommitted changes: $path (use -Force to remove anyway)"
            exit 1
        }

        Write-Host "==> removing worktree $path"
        if ($Force) {
            git worktree remove --force $path
        } else {
            git worktree remove $path
        }

        if ($DeleteBranch -and $branch -ne "(detached)") {
            Write-Host "==> deleting branch $branch"
            if ($Force) {
                git branch -D $branch
            } else {
                git branch -d $branch
            }
        }
    }

    "prune" {
        git worktree prune -v
    }

    "status" {
        $entries = Get-WorktreeEntries
        foreach ($e in $entries) {
            Write-Host "== $($e.Path) =="
            git -C $e.Path status --short --branch
            Write-Host ""
        }
    }

    "sync" {
        if (-not $Arg1) { Write-Error "usage: worktree.ps1 sync <branch> [-Onto <ref>] [-Merge]"; exit 1 }
        $branch = $Arg1
        $path = Resolve-WorktreePath $branch

        $target = $Onto
        if (-not $target) {
            $target = (git -C $path rev-parse --abbrev-ref --symbolic-full-name '@{u}' 2>$null)
            if (-not $target) {
                Write-Error "branch '$branch' has no upstream; pass -Onto <ref>"
                exit 1
            }
            $target = $target.Trim()
        }

        Write-Host "==> fetching origin"
        git -C $path fetch origin

        if ($Merge) {
            Write-Host "==> merging $target into $branch"
            git -C $path merge $target
            if ($LASTEXITCODE -ne 0) {
                Write-Host ""
                Write-Host "merge conflict - resolve with:"
                Write-Host "  scripts\worktree.ps1 conflicts $path"
                Write-Host "  scripts\worktree.ps1 continue"
                exit 1
            }
        } else {
            Write-Host "==> rebasing $branch onto $target"
            git -C $path rebase $target
            if ($LASTEXITCODE -ne 0) {
                Write-Host ""
                Write-Host "rebase conflict - resolve with:"
                Write-Host "  scripts\worktree.ps1 conflicts $path"
                Write-Host "  scripts\worktree.ps1 continue"
                Write-Host "or scripts\worktree.ps1 abort to bail out"
                exit 1
            }
        }
        Write-Host "==> $branch is up to date with $target"
    }

    "conflicts" {
        $path = if ($Arg1) { $Arg1 } else { "." }
        $kind = Get-InProgressKind $path
        if (-not $kind) {
            Write-Host "no rebase/merge in progress in $path"
        }
        $files = git -C $path diff --name-only --diff-filter=U
        if (-not $files) {
            Write-Host "no conflicted files in $path"
            return
        }
        Write-Host "conflicted files in $path:"
        foreach ($f in $files) {
            $full = Join-Path $path $f
            $n = 0
            if (Test-Path $full) {
                $n = (Select-String -Path $full -Pattern '^<<<<<<<' -AllMatches).Matches.Count
            }
            "  {0,-50} {1} marker(s)" -f $f, $n
        }
    }

    "continue" {
        $kind = Get-InProgressKind "."
        switch ($kind) {
            "rebase" {
                if (git diff --name-only --diff-filter=U) {
                    Write-Error "unresolved conflicts remain - fix them and 'git add' before continuing"
                    exit 1
                }
                git rebase --continue
            }
            "merge" {
                if (git diff --name-only --diff-filter=U) {
                    Write-Error "unresolved conflicts remain - fix them and 'git add' before continuing"
                    exit 1
                }
                git commit --no-edit
            }
            default {
                Write-Error "no rebase/merge in progress here"
                exit 1
            }
        }
    }

    "abort" {
        $kind = Get-InProgressKind "."
        switch ($kind) {
            "rebase" { git rebase --abort }
            "merge"  { git merge --abort }
            default {
                Write-Error "no rebase/merge in progress here"
                exit 1
            }
        }
    }
}
