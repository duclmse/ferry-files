#!/usr/bin/env bash
# worktree.sh - create and manage git worktrees for this repo, plus a few
# helpers for the rebase/merge conflicts that come with juggling several of
# them. Worktrees live as siblings of the repo, under
# ../<repo-name>-worktrees/<branch-sanitized>, so they never pollute the
# main checkout or get walked by scripts/bench.sh et al.
#
# Usage:
#   scripts/worktree.sh add <branch> [base]        create/checkout a worktree
#   scripts/worktree.sh list                        list worktrees + status
#   scripts/worktree.sh remove <branch> [-f] [-d]   remove a worktree
#   scripts/worktree.sh prune                        clean stale admin files
#   scripts/worktree.sh status                        git status, all worktrees
#   scripts/worktree.sh sync <branch> [-onto REF] [--merge]   fetch + rebase/merge
#   scripts/worktree.sh conflicts [path]              list conflicted files
#   scripts/worktree.sh continue                      continue paused rebase/merge
#   scripts/worktree.sh abort                          abort paused rebase/merge
#
# `add` creates a new local branch off `base` (default: current HEAD) if
# <branch> doesn't already exist, otherwise it checks the existing branch
# out into a new worktree. `remove` refuses a worktree with uncommitted
# changes unless -f/--force is passed; -d/--delete-branch also deletes the
# local branch afterward (requires it to be fully merged, unless -f is also
# given). `sync` defaults to rebase against the branch's upstream, or
# -onto REF to rebase/merge onto a specific ref; on conflict it stops and
# tells you to resolve with `conflicts` then `continue`/`abort`.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
REPO_ROOT="$(pwd)"
REPO_NAME="$(basename "$REPO_ROOT")"
WORKTREE_ROOT="$(cd "$REPO_ROOT/.." && pwd)/${REPO_NAME}-worktrees"

sanitize() {
    # Branch names can contain slashes (feature/foo) - flatten for a dir name.
    echo "${1//\//-}"
}

# Resolves <branch|path> to an actual worktree path via `git worktree list`.
resolve_worktree_path() {
    local target="$1"
    local line path branch
    while IFS= read -r line; do
        path="${line%%|*}"
        branch="${line#*|}"
        if [ "$target" = "$path" ] || [ "$target" = "$branch" ]; then
            echo "$path"
            return 0
        fi
    done < <(git worktree list --porcelain | awk '
        /^worktree /  { path=$2 }
        /^branch /    { b=$2; sub("refs/heads/", "", b); print path "|" b }
        /^detached/   { print path "|(detached)" }
    ')
    return 1
}

cmd_add() {
    local branch="${1:?usage: worktree.sh add <branch> [base]}"
    local base="${2:-HEAD}"
    local dir="$WORKTREE_ROOT/$(sanitize "$branch")"

    if [ -e "$dir" ]; then
        echo "worktree dir already exists: $dir" >&2
        exit 1
    fi
    mkdir -p "$WORKTREE_ROOT"

    if git show-ref --verify --quiet "refs/heads/$branch"; then
        echo "==> checking out existing branch '$branch' into $dir"
        git worktree add "$dir" "$branch"
    else
        echo "==> creating new branch '$branch' from '$base' at $dir"
        git worktree add -b "$branch" "$dir" "$base"
    fi
    echo "$dir"
}

cmd_list() {
    git worktree list --porcelain | awk '
        /^worktree / { path=$2 }
        /^HEAD /     { head=substr($2,1,10) }
        /^branch /   { b=$2; sub("refs/heads/", "", b); print path "\t" head "\t" b; path="" }
        /^detached/  { print path "\t" head "\t(detached)"; path="" }
    ' | while IFS=$'\t' read -r path head branch; do
        local dirty=""
        if [ -n "$(git -C "$path" status --porcelain 2>/dev/null)" ]; then
            dirty="dirty"
        else
            dirty="clean"
        fi
        printf '%-10s %-30s %-10s %s\n' "$head" "$branch" "$dirty" "$path"
    done
}

cmd_remove() {
    local target="${1:?usage: worktree.sh remove <branch|path> [-f|--force] [-d|--delete-branch]}"
    shift
    local force=0 delete_branch=0
    while [ $# -gt 0 ]; do
        case "$1" in
            -f|--force) force=1 ;;
            -d|--delete-branch) delete_branch=1 ;;
            *) echo "unknown flag: $1" >&2; exit 1 ;;
        esac
        shift
    done

    local path branch
    path="$(resolve_worktree_path "$target")" || {
        echo "no worktree matches: $target" >&2
        exit 1
    }
    branch="$(git -C "$path" rev-parse --abbrev-ref HEAD)"

    if [ -n "$(git -C "$path" status --porcelain 2>/dev/null)" ] && [ "$force" -eq 0 ]; then
        echo "worktree has uncommitted changes: $path (use -f/--force to remove anyway)" >&2
        exit 1
    fi

    local rm_flags=()
    [ "$force" -eq 1 ] && rm_flags+=(--force)
    echo "==> removing worktree $path"
    git worktree remove "${rm_flags[@]}" "$path"

    if [ "$delete_branch" -eq 1 ] && [ "$branch" != "(detached)" ]; then
        local del_flag="-d"
        [ "$force" -eq 1 ] && del_flag="-D"
        echo "==> deleting branch $branch"
        git branch "$del_flag" "$branch"
    fi
}

cmd_prune() {
    git worktree prune -v
}

cmd_status() {
    git worktree list --porcelain | awk '/^worktree /{print $2}' | while IFS= read -r path; do
        echo "== $path =="
        git -C "$path" status --short --branch
        echo ""
    done
}

# Detects an in-progress rebase or merge in the current worktree.
in_progress_kind() {
    local gitdir
    gitdir="$(git rev-parse --git-dir)"
    if [ -d "$gitdir/rebase-merge" ] || [ -d "$gitdir/rebase-apply" ]; then
        echo "rebase"
    elif [ -f "$gitdir/MERGE_HEAD" ]; then
        echo "merge"
    else
        echo ""
    fi
}

cmd_sync() {
    local branch="${1:?usage: worktree.sh sync <branch> [-onto REF] [--merge]}"
    shift
    local onto="" use_merge=0
    while [ $# -gt 0 ]; do
        case "$1" in
            -onto) shift; onto="${1:?-onto requires a ref}" ;;
            --merge) use_merge=1 ;;
            *) echo "unknown flag: $1" >&2; exit 1 ;;
        esac
        shift
    done

    local path
    path="$(resolve_worktree_path "$branch")" || {
        echo "no worktree for branch: $branch (create one with 'worktree.sh add')" >&2
        exit 1
    }

    if [ -z "$onto" ]; then
        onto="$(git -C "$path" rev-parse --abbrev-ref --symbolic-full-name '@{u}' 2>/dev/null || true)"
        if [ -z "$onto" ]; then
            echo "branch '$branch' has no upstream; pass -onto REF" >&2
            exit 1
        fi
    fi

    echo "==> fetching origin"
    git -C "$path" fetch origin

    if [ "$use_merge" -eq 1 ]; then
        echo "==> merging $onto into $branch"
        if ! git -C "$path" merge "$onto"; then
            echo "" >&2
            echo "merge conflict - resolve with:" >&2
            echo "  scripts/worktree.sh conflicts $path" >&2
            echo "  scripts/worktree.sh continue" >&2
            exit 1
        fi
    else
        echo "==> rebasing $branch onto $onto"
        if ! git -C "$path" rebase "$onto"; then
            echo "" >&2
            echo "rebase conflict - resolve with:" >&2
            echo "  scripts/worktree.sh conflicts $path" >&2
            echo "  scripts/worktree.sh continue" >&2
            echo "or scripts/worktree.sh abort to bail out" >&2
            exit 1
        fi
    fi
    echo "==> $branch is up to date with $onto"
}

cmd_conflicts() {
    local path="${1:-.}"
    local kind
    kind="$(cd "$path" && in_progress_kind)"
    if [ -z "$kind" ]; then
        echo "no rebase/merge in progress in $path" >&2
    fi
    local files
    files="$(git -C "$path" diff --name-only --diff-filter=U)"
    if [ -z "$files" ]; then
        echo "no conflicted files in $path"
        return 0
    fi
    echo "conflicted files in $path:"
    while IFS= read -r f; do
        local n
        n="$(grep -c '^<<<<<<<' "$path/$f" 2>/dev/null || true)"
        printf '  %-50s %s marker(s)\n' "$f" "${n:-0}"
    done <<< "$files"
}

cmd_continue() {
    local kind
    kind="$(in_progress_kind)"
    case "$kind" in
        rebase)
            if [ -n "$(git diff --name-only --diff-filter=U)" ]; then
                echo "unresolved conflicts remain - fix them and 'git add' before continuing" >&2
                exit 1
            fi
            git rebase --continue
            ;;
        merge)
            if [ -n "$(git diff --name-only --diff-filter=U)" ]; then
                echo "unresolved conflicts remain - fix them and 'git add' before continuing" >&2
                exit 1
            fi
            git commit --no-edit
            ;;
        *)
            echo "no rebase/merge in progress here" >&2
            exit 1
            ;;
    esac
}

cmd_abort() {
    local kind
    kind="$(in_progress_kind)"
    case "$kind" in
        rebase) git rebase --abort ;;
        merge)  git merge --abort ;;
        *)
            echo "no rebase/merge in progress here" >&2
            exit 1
            ;;
    esac
}

CMD="${1:-}"
[ $# -gt 0 ] && shift || true
case "$CMD" in
    add)       cmd_add "$@" ;;
    list)      cmd_list "$@" ;;
    remove)    cmd_remove "$@" ;;
    prune)     cmd_prune "$@" ;;
    status)    cmd_status "$@" ;;
    sync)      cmd_sync "$@" ;;
    conflicts) cmd_conflicts "$@" ;;
    continue)  cmd_continue "$@" ;;
    abort)     cmd_abort "$@" ;;
    *)
        echo "Usage: scripts/worktree.sh {add|list|remove|prune|status|sync|conflicts|continue|abort} ..." >&2
        exit 1
        ;;
esac
