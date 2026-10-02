#!/bin/sh
# FramePlayer launcher: the command Steam (and Frame Control / FrameDrop)
# runs. It lives at the root of the install dir (~/devkit-game/frameplayer)
# and is shipped inside every release tarball.
#
# Responsibilities, kept in lock-step with crates/updater/src/layout.rs:
#  1. Adopt a version delivered by a flat tarball install: if RELEASE names a
#     version we have not adopted (.installed-release), point `current` at
#     versions/<RELEASE> (old one becomes `previous`, new one goes on trial).
#  2. Count launches of a version on trial ("<ver> <attempts> <max>" in
#     `trial`); once attempts exceed max without the app marking itself
#     healthy, roll back to `previous` and record the bad version in `blocked`.
#     Counting here (not only in the app) catches builds that cannot start.
#  3. Exec the current binary with bundled libs and logs redirected.
set -u

ROOT=$(cd "$(dirname "$0")" && pwd -P)
MAX_ATTEMPTS=${FP_MAX_ATTEMPTS:-3}
LOG_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/frameplayer/logs"
mkdir -p "$LOG_DIR"
LOG="$LOG_DIR/launcher.log"
if [ -f "$LOG" ] && [ "$(wc -c <"$LOG")" -gt 5000000 ]; then mv -f "$LOG" "$LOG.old"; fi

log() { echo "$(date '+%Y-%m-%dT%H:%M:%S') launcher: $*" >>"$LOG"; }
first_line() { [ -f "$1" ] && head -n1 "$1" | tr -d '\r\n ' ; }
valid_ver() {
  case "$1" in ''|*[!0-9A-Za-z.+-]*) return 1 ;; esac
  [ -d "$ROOT/versions/$1" ]
}
link_ver() { # current|previous -> version or empty
  t=$(readlink "$ROOT/$1" 2>/dev/null) || return 0
  case "$t" in versions/*) echo "${t#versions/}" ;; esac
}
write_file() { # path content (atomic)
  printf '%s\n' "$2" >"$1.tmp-$$" && mv -f "$1.tmp-$$" "$1"
}
swap_link() { # name version  (atomic rename(2) of a temp symlink)
  tmp="$ROOT/.$1.tmp-$$"
  rm -f "$tmp"
  ln -s "versions/$2" "$tmp" && mv -Tf "$tmp" "$ROOT/$1"
}
activate() {
  old=$(link_ver current)
  if [ "$old" != "$1" ]; then
    if [ -n "$old" ]; then
      swap_link previous "$old"
      write_file "$ROOT/trial" "$1 0 $MAX_ATTEMPTS"
    else
      rm -f "$ROOT/trial"
    fi
    swap_link current "$1"
    log "activated $1 (previous: ${old:-none})"
  fi
  write_file "$ROOT/.installed-release" "$1"
}

# 1. Adopt flat installs.
release=$(first_line "$ROOT/RELEASE")
installed=$(first_line "$ROOT/.installed-release")
if valid_ver "$release" && { [ "$release" != "$installed" ] || [ -z "$(link_ver current)" ]; }; then
  activate "$release"
fi
if [ -z "$(link_ver current)" ]; then
  # No RELEASE file (hand-copied build): use the newest version present.
  newest=$(ls -1 "$ROOT/versions" 2>/dev/null | sort -V | tail -n1)
  if valid_ver "$newest"; then activate "$newest"; fi
fi

# 2. Trial accounting and rollback.
if [ -f "$ROOT/trial" ]; then
  read -r tver tatt tmax <"$ROOT/trial" || true
  cur=$(link_ver current)
  if [ "${tver:-}" != "$cur" ]; then
    rm -f "$ROOT/trial"
  else
    tatt=$(( ${tatt:-0} + 1 ))
    if [ "$tatt" -gt "${tmax:-$MAX_ATTEMPTS}" ]; then
      prev=$(link_ver previous)
      if valid_ver "$prev" && [ "$prev" != "$cur" ]; then
        grep -qx "$cur" "$ROOT/blocked" 2>/dev/null || echo "$cur" >>"$ROOT/blocked"
        swap_link current "$prev"
        log "version $cur never became healthy after ${tmax:-$MAX_ATTEMPTS} launches; rolled back to $prev"
      fi
      rm -f "$ROOT/trial"
    else
      write_file "$ROOT/trial" "$tver $tatt ${tmax:-$MAX_ATTEMPTS}"
    fi
  fi
fi

# 3. Run.
cur=$(link_ver current)
if ! valid_ver "$cur" || [ ! -x "$ROOT/current/bin/frameplayer" ]; then
  log "no runnable version installed in $ROOT"
  echo "FramePlayer: no runnable version installed in $ROOT" >&2
  exit 1
fi
export FP_INSTALL_ROOT="$ROOT"
export FP_LAUNCHER_COUNTED=1
export FP_LOG_DIR="$LOG_DIR"
export LD_LIBRARY_PATH="$ROOT/current/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
log "starting $cur"
cd "$ROOT/current" || exit 1
exec "$ROOT/current/bin/frameplayer" "$@" >>"$LOG" 2>&1
