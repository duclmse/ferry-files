#!/bin/bash
# Frees a phone that MTP can't open because another program still has it ("libusb_claim_interface()
# = -3", "LIBMTP PANIC: Unable to initialize device"). macOS gives a USB device, and each of its
# interfaces, to one program at a time and says which in ioreg (UsbExclusiveOwner); this shows that
# for each device and, when it's one of the programs known to grab phones (Image Capture's
# ptpcamerad, Android File Transfer, OpenMTP, mtp-* tools...), stops it so the next connect can
# claim the phone.
#
# Ferry Files itself (an earlier run, or the one running now with the phone connected) is only
# reported: ⏏ in its sidebar lets go of the phone. --force stops it too.
#
# Image Capture's helpers ignore a polite stop, so they're killed, and icdd starts ptpcamerad again
# about 1.5 s later, when it takes the phone back. Ferry Files stops them itself as it connects;
# for another app, --watch keeps stopping them until something else has the phone (or Ctrl+C).
#
# usage: scripts/free-mtp.sh [--force] [--watch] [--dry-run]
set -uo pipefail

force=0
dry=0
watch=0
for a in "$@"; do
  case "$a" in
    --force) force=1 ;;
    --dry-run|-n) dry=1 ;;
    --watch) watch=1 ;;
    -h|--help) sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $a" >&2; exit 2 ;;
  esac
done

if [ "$(uname)" != Darwin ]; then
  echo "This is for macOS. On Linux, see who has the phone with: lsof /dev/bus/usb/*/*" >&2
  exit 2
fi

# Image Capture's helpers: they ignore SIGTERM, and come back by themselves.
HELPERS='^(ptpcamerad|icdd|mscamerad-xpc)$'
# Programs that grab phones over USB, as ps shows their command (an extended regex).
GRABBERS='ptpcamerad|icdd|mscamerad-xpc|Image Capture|Android File Transfer|OpenMTP|MacDroid|Smart Switch|Kies|mtp-[a-z]+|gphoto2|PTPCamera'

# USB devices that a program (not one of macOS's own drivers) has, or has one of the interfaces of
# (an MTP session claims an interface, which is how ptpcamerad blocks it): one line per program,
# name, vendor:product, pid, owner.
owned() {
  ioreg -r -c IOUSBHostDevice -l -w0 | awk '
    function flush(   i) {
      for (i = 1; i <= n; i++) printf "%s\t%04x:%04x\t%s\t%s\n", name, vid, prod, pids[i], whos[i]
      n = 0; name = ""; vid = 0; prod = 0; split("", got)
    }
    /\+-o .*<class IOUSB(Host)?Device[,>]/ { flush(); name = $0; sub(/.*\+-o /, "", name); sub(/@.*/, "", name); dev = 1; next }
    /\+-o / { dev = 0 }
    dev && /"USB Product Name" = / { v = $0; sub(/.*= "/, "", v); sub(/".*/, "", v); name = v }
    dev && /"idVendor" = / { v = $0; sub(/.*= /, "", v); vid = v + 0 }
    dev && /"idProduct" = / { v = $0; sub(/.*= /, "", v); prod = v + 0 }
    /"UsbExclusiveOwner" = "pid [0-9]+/ {
      o = $0; sub(/.*= "pid /, "", o); sub(/"$/, "", o)
      pid = o; sub(/[^0-9].*/, "", pid)
      who = o; sub(/^[0-9]+, */, "", who)
      if (!(pid in got)) { got[pid] = 1; n++; pids[n] = pid; whos[n] = who }
    }
    END { flush() }'
}

# Stops a process: asks it to quit, then makes it after $3 seconds (3 if not given; 0 kills it
# at once).
stop() {
  local pid="$1" what="$2" wait="${3:-3}" i
  if [ "$dry" = 1 ]; then
    echo "  would stop $what (pid $pid)"
    return
  fi
  echo "  stopping $what (pid $pid)"
  if [ "$wait" = 0 ]; then
    kill -9 "$pid" 2>/dev/null || echo "  couldn't signal pid $pid (try with sudo)"
    return
  fi
  kill "$pid" 2>/dev/null || { echo "  couldn't signal pid $pid (try with sudo)"; return; }
  for ((i = 0; i < wait * 2; i++)); do
    kill -0 "$pid" 2>/dev/null || return
    sleep 0.5
  done
  echo "  still running; killing it"
  kill -9 "$pid" 2>/dev/null
}

# Stops the helpers holding a device in `owned`'s output ($1); says whether there were any.
stop_helpers() {
  local any=1 name ids pid who
  while IFS=$'\t' read -r name ids pid who; do
    if [ -n "$pid" ] && echo "$who" | grep -Eq "$HELPERS"; then
      kill -9 "$pid" 2>/dev/null && echo "  $who took $name back (pid $pid): stopped it again"
      any=0
    fi
  done <<< "$1"
  return $any
}

# --watch: stops Image Capture's helpers whenever they take a device, until another program has
# it (then says which) or Ctrl+C.
keep_off() {
  local left
  echo
  echo "Keeping Image Capture's helpers off the phone: connect it now (Ctrl+C to stop)."
  while :; do
    sleep 0.25
    left="$(owned)"
    stop_helpers "$left" && continue
    if [ -n "$left" ]; then
      echo "$left" | awk -F'\t' '{ printf "%s (%s) is now held by %s, pid %s.\n", $1, $2, $4, $3 }'
      exit 0
    fi
  done
}

devices="$(owned)"
if [ -z "$devices" ]; then
  echo "No USB device is held by a program: the phone is free."
  echo "If connecting still fails, unplug and replug it, unlock it, and choose File transfer."
  [ "$watch" = 1 ] && [ "$dry" = 0 ] && keep_off
  exit 0
fi

freed=0
blocked=0
while IFS=$'\t' read -r name ids pid who; do
  cmd="$(ps -o command= -p "$pid" 2>/dev/null)"
  echo "$name ($ids) is held by $who, pid $pid"
  if [ -z "$cmd" ]; then
    echo "  that process has ended, but macOS still lists it: unplug and replug the phone."
    blocked=1
    continue
  fi
  started="$(ps -o lstart= -p "$pid" 2>/dev/null | sed 's/  */ /g; s/ *$//')"
  parent="$(ps -o ppid= -p "$pid" | tr -d ' ')"
  echo "  $cmd"
  echo "  started $started"
  case "$cmd" in
    *ferry-files*)
      pcmd="$(ps -o command= -p "$parent" 2>/dev/null)"
      case "$pcmd" in
        *tauri\ dev*) echo "  this is the running cargo tauri dev app (pid $parent runs it)." ;;
        *) echo "  this is a Ferry Files app that's still running (parent: ${pcmd:-pid $parent})." ;;
      esac
      if [ "$force" = 1 ]; then
        # It quits as from its menu, closing the phone's session; that can take up to 10 s.
        stop "$pid" "Ferry Files" 12
        freed=1
      else
        echo "  If it has the phone connected, ⏏ next to the phone in its sidebar (or quitting it) frees it."
        echo "  If it doesn't, a connect that failed still holds it: unplug and replug the phone."
        echo "  Run with --force to stop it instead."
        blocked=1
      fi
      ;;
    *)
      if echo "$who" | grep -Eq "$HELPERS"; then
        stop "$pid" "$who" 0
        freed=1
      elif echo "$cmd $who" | grep -Eq "$GRABBERS"; then
        stop "$pid" "$who"
        freed=1
      elif [ "$force" = 1 ]; then
        stop "$pid" "$who"
        freed=1
      else
        echo "  not a program this script knows to grab phones; quit it, or run with --force."
        blocked=1
      fi
      ;;
  esac
done <<< "$devices"

if [ "$freed" = 1 ] && [ "$dry" = 0 ]; then
  # Android File Transfer's agent opens the app again when a phone is plugged in.
  if pkill -x "Android File Transfer Agent" 2>/dev/null; then
    echo "Stopped Android File Transfer Agent too, which opens Android File Transfer when a phone is"
    echo "plugged in (to keep it from starting at login: System Settings > General > Login Items)."
  fi
  [ "$watch" = 1 ] && keep_off
  # ptpcamerad is back about 1.5 s after it's stopped.
  sleep 2.5
  left="$(owned)"
  echo
  if [ -z "$left" ]; then
    echo "Done: no program holds a USB device now. Connect the phone again."
  else
    echo "Held again:"
    echo "$left" | awk -F'\t' '{ printf "  %s (%s) by %s, pid %s\n", $1, $2, $4, $3 }'
    if echo "$left" | cut -f4 | grep -Eq "$HELPERS"; then
      echo "Image Capture's helper came back, as it does. Ferry Files stops it itself when it connects;"
      echo "for another app, run this with --watch and connect while it runs."
    fi
    exit 1
  fi
fi
[ "$blocked" = 1 ] && exit 1
exit 0
