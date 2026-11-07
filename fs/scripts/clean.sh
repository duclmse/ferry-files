#!/usr/bin/env bash
#
# clean.sh - interactive disk cleanup for a dev machine (macOS + Linux).
# Covers Node/pnpm, Docker, Rust/Go/Java/Python toolchains, and (on macOS)
# Xcode/CocoaPods/Simulators, or (on Linux) the system package manager cache
# and journal logs. Not specific to this repo - it cleans up your whole
# machine, same as any other dev-machine housekeeping script.
#
# Usage:
#   ./clean.sh              interactive mode menu
#   ./clean.sh --report     scan only, never deletes anything
#   ./clean.sh --confirm    per-category y/n before each delete (default when interactive)
#   ./clean.sh --force      delete every "safe" item with no prompts
#
# A few categories are ALWAYS confirmed individually regardless of mode
# (Docker volumes, erasing all iOS Simulators, thinning Time Machine
# snapshots) because they can destroy data that isn't just a regenerable
# cache. --force does not skip these.
set -o pipefail

# ---- search roots for node_modules / target / venv discovery ----
# Edit this list to match where your projects actually live.
SEARCH_ROOTS=("$HOME/projects" "$HOME/tools" "$HOME/go/src" "$HOME/Documents")
EXISTING_ROOTS=()
for r in "${SEARCH_ROOTS[@]}"; do
    [[ -d "$r" ]] && EXISTING_ROOTS+=("$r")
done

OS="$(uname -s)"

# ---------------------------------------------------------------------------
MODE=""
for arg in "$@"; do
    case "$arg" in
        --report)  MODE="report" ;;
        --confirm) MODE="confirm" ;;
        --force)   MODE="force" ;;
        *) echo "Unknown option: $arg (use --report, --confirm, or --force)"; exit 1 ;;
    esac
done

if [[ -z "$MODE" ]]; then
    echo "How should this run behave?"
    echo "  1) Report only   - scan and print sizes, delete nothing"
    echo "  2) Confirm each  - show what would be deleted, ask y/n per category (recommended)"
    echo "  3) Force         - delete every 'safe' item automatically, no prompts"
    read -r -p "Choose [1/2/3]: " choice
    case "$choice" in
        1) MODE="report" ;;
        2) MODE="confirm" ;;
        3) MODE="force" ;;
        *) echo "Invalid choice."; exit 1 ;;
    esac
fi
echo "Platform: $OS"
echo "Mode: $MODE"
echo

section_header() { printf "\n== %s ==\n" "$1"; }

confirm() {
    # $1 = prompt text. Returns 0 (yes) or 1 (no).
    local ans
    read -r -p "$1 [y/N] " ans
    [[ "$ans" =~ ^[Yy]$ ]]
}

# Delete a path outright (dir + contents), or just its contents if $3=contents.
process_target() {
    local name="$1" path="$2" del_mode="${3:-dir}"
    if [[ ! -e "$path" ]]; then
        printf "  %-42s %s\n" "$name" "(not found)"
        return
    fi
    local size
    size=$(du -sh "$path" 2>/dev/null | awk '{print $1}')
    printf "  %-42s %8s  %s\n" "$name" "$size" "$path"
    case "$MODE" in
        report) return ;;
        confirm) confirm "    Delete?" || return ;;
    esac
    if [[ "$del_mode" == "contents" ]]; then
        find "$path" -mindepth 1 -maxdepth 1 -exec rm -rf {} + 2>/dev/null
    else
        rm -rf "$path"
    fi
    echo "    -> deleted"
}

# Show size at $2 (if it exists) then run a cleanup command instead of rm -rf.
process_cmd_target() {
    local name="$1" size_path="$2" cmd="$3"
    local size="-"
    [[ -n "$size_path" && -e "$size_path" ]] && size=$(du -sh "$size_path" 2>/dev/null | awk '{print $1}')
    printf "  %-42s %8s\n" "$name" "$size"
    echo "    cmd: $cmd"
    case "$MODE" in
        report) return ;;
        confirm) confirm "    Run it?" || return ;;
    esac
    eval "$cmd"
}

# List every path in $paths[@], one combined y/n, then bulk-delete.
bulk_delete() {
    local title="$1"; shift
    local paths=("$@")
    section_header "$title"
    if [[ ${#paths[@]} -eq 0 ]]; then
        echo "  none found under: ${EXISTING_ROOTS[*]}"
        return
    fi
    for p in "${paths[@]}"; do
        printf "  %8s  %s\n" "$(du -sh "$p" 2>/dev/null | awk '{print $1}')" "$p"
    done
    echo "  ${#paths[@]} item(s) found."
    case "$MODE" in
        report) return ;;
        confirm) confirm "  Delete ALL ${#paths[@]} listed above?" || return ;;
    esac
    for p in "${paths[@]}"; do rm -rf "$p"; done
    echo "  -> deleted ${#paths[@]} item(s)"
}

DF_BEFORE=$(df -h / | awk 'NR==2{print $4}')

# =====================================================================
section_header "Dev tool caches"
# =====================================================================

if command -v brew &>/dev/null; then
    echo "  Homebrew: 'brew cleanup -n' dry-run listing:"
    brew cleanup -n 2>/dev/null | sed 's/^/    /'
    process_cmd_target "Homebrew cache/old versions" "$(brew --cache 2>/dev/null)" "brew cleanup -s"
fi

command -v npm  &>/dev/null && process_cmd_target "npm cache"  "$HOME/.npm"                     "npm cache clean --force"
command -v pnpm &>/dev/null && process_cmd_target "pnpm store" "$(pnpm store path 2>/dev/null)"  "pnpm store prune"
command -v yarn &>/dev/null && process_cmd_target "yarn cache" "$(yarn cache dir 2>/dev/null)"   "yarn cache clean"

if command -v watchman &>/dev/null; then
    process_cmd_target "Watchman watches" "-" "watchman watch-del-all"
fi
process_target "Misc ~/.cache (pip, misc CLIs, etc.)" "$HOME/.cache" contents

# =====================================================================
section_header "Java / Rust / Go / Python package caches"
# =====================================================================

process_target "Maven repository (~/.m2/repository)" "$HOME/.m2/repository" contents

process_target "Cargo registry cache"  "$HOME/.cargo/registry/cache" contents
process_target "Cargo registry src"    "$HOME/.cargo/registry/src"   contents
process_target "Cargo git checkouts"   "$HOME/.cargo/git"            contents

if command -v go &>/dev/null; then
    GOCACHE=$(go env GOCACHE 2>/dev/null)
    GOMODCACHE=$(go env GOMODCACHE 2>/dev/null)
    process_cmd_target "Go build cache"  "$GOCACHE"    "go clean -cache"
    process_cmd_target "Go module cache" "$GOMODCACHE" "go clean -modcache"
fi

if command -v pip3 &>/dev/null; then
    PIPCACHE=$(pip3 cache dir 2>/dev/null)
    process_cmd_target "pip cache" "$PIPCACHE" "pip3 cache purge"
fi
if command -v conda &>/dev/null; then
    process_cmd_target "conda caches (pkgs, tarballs)" "$HOME/.conda/pkgs" "conda clean --all --yes"
fi
if command -v gem &>/dev/null; then
    process_cmd_target "gem cache" "$HOME/.gem" "gem cleanup"
fi

# =====================================================================
section_header "Docker"
# =====================================================================

if command -v docker &>/dev/null && docker info &>/dev/null; then
    echo "  docker system df:"
    docker system df 2>/dev/null | sed 's/^/    /'
    process_cmd_target "Dangling images"    "-" "docker image prune -f"
    process_cmd_target "Stopped containers" "-" "docker container prune -f"
    process_cmd_target "Build cache"        "-" "docker builder prune -f"
    process_cmd_target "Unused networks"    "-" "docker network prune -f"

    echo
    echo "  ! Docker volumes may hold a project's local Postgres/Redis data."
    echo "    Unused (unattached) volumes only - attached ones are never touched - but"
    echo "    always requires an explicit confirmation, in every mode:"
    docker volume ls -f dangling=true 2>/dev/null | sed 's/^/    /'
    if confirm "  Prune dangling Docker volumes?"; then
        docker volume prune -f
    else
        echo "  skipped"
    fi
else
    echo "  docker not running or not installed, skipping"
fi

# =====================================================================
section_header "node_modules / target / venv across ${EXISTING_ROOTS[*]:-<none found>}"
# =====================================================================

if [[ ${#EXISTING_ROOTS[@]} -gt 0 ]]; then
    NM_DIRS=()
    while IFS= read -r d; do NM_DIRS+=("$d"); done < <(find "${EXISTING_ROOTS[@]}" -xdev -type d -name node_modules -prune -print 2>/dev/null)
    bulk_delete "node_modules folders (reinstallable via pnpm/npm/yarn install)" "${NM_DIRS[@]}"

    TARGET_DIRS=()
    while IFS= read -r d; do TARGET_DIRS+=("$d"); done < <(find "${EXISTING_ROOTS[@]}" -xdev -type d -name target -prune -print 2>/dev/null)
    bulk_delete "Rust/Maven 'target' build output dirs" "${TARGET_DIRS[@]}"

    VENV_DIRS=()
    while IFS= read -r d; do VENV_DIRS+=("$d"); done < <(find "${EXISTING_ROOTS[@]}" -xdev -type d \( -name ".venv" -o -name "venv" \) -prune -print 2>/dev/null)
    bulk_delete "Python venv folders" "${VENV_DIRS[@]}"
else
    echo "  none of SEARCH_ROOTS exist on this machine - edit SEARCH_ROOTS at the top of this script"
fi

if [[ "$OS" == "Darwin" ]]; then
    # =====================================================================
    section_header "macOS-specific caches"
    # =====================================================================

    process_target "Xcode DerivedData"       "$HOME/Library/Developer/Xcode/DerivedData" contents
    process_target "Xcode iOS DeviceSupport" "$HOME/Library/Developer/Xcode/iOS DeviceSupport" contents
    process_target "CocoaPods cache"         "$HOME/Library/Caches/CocoaPods" contents
    process_target "Gradle caches"           "$HOME/.gradle/caches" contents
    process_target "Gradle wrapper distros"  "$HOME/.gradle/wrapper/dists" contents
    process_target "Android build cache"     "$HOME/.android/cache" contents
    process_target "Expo cache"              "$HOME/.expo/cache" contents

    METRO_TMP=()
    while IFS= read -r d; do METRO_TMP+=("$d"); done < <(find "${TMPDIR:-/tmp}" -maxdepth 1 \( -name "metro-*" -o -name "haste-map-*" -o -name "react-*" \) 2>/dev/null)
    bulk_delete "Metro/Haste/RN tmp cache" "${METRO_TMP[@]}"

    # =====================================================================
    section_header "macOS system junk"
    # =====================================================================

    process_target "Trash" "$HOME/.Trash" contents
    process_target "~/Library/Caches (per-app caches)" "$HOME/Library/Caches" contents
    process_target "~/Library/Logs" "$HOME/Library/Logs" contents

    if command -v xcrun &>/dev/null; then
        echo "  Removing unavailable (already-deleted-OS) iOS Simulator runtimes - always safe:"
        xcrun simctl delete unavailable 2>/dev/null
        echo "  Current simulator device data:"
        du -sh "$HOME/Library/Developer/CoreSimulator/Devices" 2>/dev/null | sed 's/^/    /'
        echo "  ! 'erase all' wipes every simulator's data/settings (not just unavailable ones)."
        if confirm "  Erase ALL iOS Simulators now?"; then
            xcrun simctl erase all
        else
            echo "  skipped"
        fi
    fi

    echo
    echo "  Old installers in ~/Downloads (*.dmg, *.pkg, *.zip, older than 30 days):"
    OLD_DOWNLOADS=()
    while IFS= read -r d; do OLD_DOWNLOADS+=("$d"); done < <(find "$HOME/Downloads" -maxdepth 1 -type f \( -iname "*.dmg" -o -iname "*.pkg" -o -iname "*.zip" \) -mtime +30 2>/dev/null)
    bulk_delete "Old installers in ~/Downloads" "${OLD_DOWNLOADS[@]}"

    if command -v tmutil &>/dev/null; then
        echo
        echo "  Time Machine local snapshots (safe to thin, these are not your only backup copy):"
        tmutil listlocalsnapshots / 2>/dev/null | sed 's/^/    /'
        if confirm "  Thin local Time Machine snapshots to free space?"; then
            sudo tmutil thinlocalsnapshots / 999999999999 4
        else
            echo "  skipped"
        fi
    fi

    echo
    echo "  iOS device backups (never auto-deleted - check manually if huge):"
    if [[ -e "$HOME/Library/Application Support/MobileSync/Backup" ]]; then
        du -sh "$HOME/Library/Application Support/MobileSync/Backup" 2>/dev/null | sed 's/^/    /'
    else
        echo "    none found"
    fi

elif [[ "$OS" == "Linux" ]]; then
    # =====================================================================
    section_header "Linux-specific caches"
    # =====================================================================

    if command -v apt-get &>/dev/null; then
        process_cmd_target "apt package cache (/var/cache/apt/archives)" "/var/cache/apt/archives" "sudo apt-get clean"
    fi
    if command -v dnf &>/dev/null; then
        process_cmd_target "dnf package cache" "/var/cache/dnf" "sudo dnf clean all"
    fi
    if command -v pacman &>/dev/null; then
        process_cmd_target "pacman package cache (/var/cache/pacman/pkg)" "/var/cache/pacman/pkg" "sudo pacman -Sc --noconfirm"
    fi
    if command -v snap &>/dev/null; then
        process_cmd_target "disabled snap revisions" "-" \
            "snap list --all 2>/dev/null | awk '/disabled/{print \$1, \$3}' | while read -r n r; do sudo snap remove \"\$n\" --revision=\"\$r\"; done"
    fi
    if command -v flatpak &>/dev/null; then
        process_cmd_target "unused flatpak runtimes" "-" "flatpak uninstall --unused -y"
    fi
    if command -v journalctl &>/dev/null; then
        process_cmd_target "systemd journal logs (kept: last 200MB)" "-" "sudo journalctl --vacuum-size=200M"
    fi

    # =====================================================================
    section_header "Linux system junk"
    # =====================================================================

    process_target "Trash (~/.local/share/Trash)" "$HOME/.local/share/Trash" contents
    process_target "Thumbnail cache (~/.cache/thumbnails)" "$HOME/.cache/thumbnails" contents

else
    echo
    echo "Unrecognized platform '$OS'; skipping OS-specific sections."
fi

# =====================================================================
DF_AFTER=$(df -h / | awk 'NR==2{print $4}')
section_header "Done"
echo "  Free space before: $DF_BEFORE"
echo "  Free space after:  $DF_AFTER"
