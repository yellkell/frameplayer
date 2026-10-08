#!/usr/bin/env bash
# Capture what SteamVR's runtime does inside Chromium's XR utility process on a
# Steam Frame, to confirm (or refute) the /proc/self + SO_PEERCRED diagnosis
# before the patches are sent upstream.
#
# Run this ON THE HEADSET (Desktop Mode terminal, or over the devkit SSH
# pairing used by Frame Control / FrameDrop) while a WebXR page is about to
# enter an immersive session. Needs an arm64 strace binary; SteamOS does not
# ship one, so drop a static build in ~/bin/strace (e.g. from the strace
# GitHub releases, or built with `./configure LDFLAGS=-static` on any arm64 box).
#
# Usage:
#   frame-xr-trace.sh <chromium-binary-dir> [seconds]
# Output:
#   ~/frame-xr-trace/<timestamp>/{strace.txt,proc-paths.txt,sockopts.txt,vrserver-tail.txt}
set -euo pipefail
chrome_dir="${1:?chromium install dir (contains 'chrome')}"
secs="${2:-40}"
out="$HOME/frame-xr-trace/$(date +%Y%m%d-%H%M%S)"
mkdir -p "$out"
strace_bin="$(command -v strace || echo "$HOME/bin/strace")"
[ -x "$strace_bin" ] || { echo "strace not found; see header comment" >&2; exit 1; }

echo "Launching Chromium WITH the seccomp sandbox (no --disable-seccomp-filter-sandbox)."
echo "Open a WebXR page and press 'Enter VR' within $secs s."
# -f follows the XR utility process and its broker child; %file catches
# open/stat/readlink families; getsockopt is the SO_PEERCRED check.
"$strace_bin" -f -tt -s 256 -o "$out/strace.txt" \
  -e trace=%file,getsockopt,setsockopt,socket,connect,bind,kill,sched_setscheduler \
  "$chrome_dir/chrome" --enable-features=OpenXR --password-store=basic \
  --enable-logging=stderr --v=1 2>"$out/chrome-stderr.txt" &
pid=$!
sleep "$secs"
kill "$pid" 2>/dev/null || true
wait "$pid" 2>/dev/null || true

# Everything the runtime touched under /proc, with the pid that asked.
grep -E '"/proc/' "$out/strace.txt" | sed -E 's/^([0-9]+) .*("\/proc[^"]*").*/\1 \2/' | sort | uniq -c | sort -rn > "$out/proc-paths.txt"
grep -E 'getsockopt|setsockopt' "$out/strace.txt" > "$out/sockopts.txt" || true
# SIGSYS deaths show the syscall the policy rejected.
grep -E 'SIGSYS|killed by' "$out/strace.txt" > "$out/sigsys.txt" || true
tail -n 200 "$HOME/.local/share/Steam/logs/vrserver.txt" > "$out/vrserver-tail.txt" 2>/dev/null || true

echo
echo "== /proc paths requested (count, pid, path) =="; cat "$out/proc-paths.txt"
echo; echo "== SIGSYS =="; cat "$out/sigsys.txt"
echo; echo "== vrserver registered pids =="; grep -iE 'pid|process' "$out/vrserver-tail.txt" | tail -n 20
echo; echo "Saved under $out"
echo "A bare readlink(\"/proc/self\") or stat(\"/proc/self\") in proc-paths.txt confirms patch 0001;"
echo "any other /proc/self/<entry> not in {cmdline,comm,exe,stat,status} must be added to patch 0002."
