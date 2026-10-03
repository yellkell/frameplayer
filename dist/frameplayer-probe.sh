#!/bin/sh
# FramePlayer self-test launcher. Lives next to frameplayer.sh at the root of
# the install dir (~/devkit-game/frameplayer), ships in every release tarball
# and is (re)written by frameplayer-install before each self-test.
#
#  * Steam runs it for the "FramePlayer Self-Test" library entry (full test
#    with the headset on; output goes to the log).
#  * frameplayer-install runs it over SSH with --headless (output streams
#    back to the PC).
#
# Runs frameplayer-probe from the version named in RELEASE (the one just
# installed, even before frameplayer.sh has adopted it), else `current`.
# Unlike frameplayer.sh it never touches trial/rollback state.
set -u

ROOT=$(cd "$(dirname "$0")" && pwd -P)
LOG_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/frameplayer/logs"
mkdir -p "$LOG_DIR"
first_line() { [ -f "$1" ] && head -n1 "$1" | tr -d '\r\n ' ; }
has_probe() { [ -n "$1" ] && [ -x "$1/bin/frameplayer-probe" ]; }

rel=$(first_line "$ROOT/RELEASE")
DIR=""
case "$rel" in
  ''|*[!0-9A-Za-z.+-]*) ;;
  *) has_probe "$ROOT/versions/$rel" && DIR="$ROOT/versions/$rel" ;;
esac
if [ -z "$DIR" ] && has_probe "$ROOT/current"; then DIR="$ROOT/current"; fi
if [ -z "$DIR" ]; then
  echo "FramePlayer self-test: frameplayer-probe is not installed in $ROOT" >&2
  exit 127
fi

export FP_INSTALL_ROOT="$ROOT"
export FP_LOG_DIR="$LOG_DIR"
export LD_LIBRARY_PATH="$DIR/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
cd "$DIR" || exit 1
case " $* " in
  *" --headless "*) exec "$DIR/bin/frameplayer-probe" "$@" ;;
esac
LOG="$LOG_DIR/probe.log"
if [ -f "$LOG" ] && [ "$(wc -c <"$LOG")" -gt 5000000 ]; then mv -f "$LOG" "$LOG.old"; fi
echo "$(date '+%Y-%m-%dT%H:%M:%S') self-test: starting $DIR/bin/frameplayer-probe $*" >>"$LOG"
exec "$DIR/bin/frameplayer-probe" "$@" >>"$LOG" 2>&1
