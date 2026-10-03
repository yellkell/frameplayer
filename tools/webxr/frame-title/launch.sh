#!/bin/bash
# Starts Chromium XR from the Frame Control / Steam devkit title. The title's
# top-level chromium-xr.sh and chromium-xr-sandboxed.sh call this with
# CHROMIUM_XR_SANDBOXED=0 or 1; Steam passes no arguments, so it opens the
# bundled start page (the WebXR check, served on localhost so WebXR is
# allowed). Arguments, if any, go to Chromium: `chromium-xr.sh URL`.
#
# Based on saphid/chromium-webxr-steam-frame frame/chromium-xr (BSD-3).
set -euo pipefail

here=$(cd "$(dirname "$(readlink -f "$0")")" && pwd)
sandboxed=${CHROMIUM_XR_SANDBOXED:-0}
name=chromium-xr-frame
[[ $sandboxed == 1 ]] && name=chromium-xr-frame-sandboxed
logs=$HOME/.local/state/chromium-xr-frame
mkdir -p "$logs"
log=$logs/$name.log
[[ -f $log ]] && mv -f "$log" "$log.1"
exec >>"$log" 2>&1
echo "$(date -Is) $name starting: $(head -n 1 "$here/BUILD-INFO.txt" 2>/dev/null)"

# Steam preloads its overlay (gameoverlayrenderer.so) into everything it
# launches; it crashes Chromium's zygote. Keep anything else preloaded.
if [[ -n "${LD_PRELOAD:-}" ]]; then
  keep=()
  IFS=' :' read -r -d '' -a libs < <(printf '%s' "$LD_PRELOAD") || true
  for lib in ${libs[@]+"${libs[@]}"}; do
    [[ -z "$lib" || "$lib" == *gameoverlayrenderer.so ]] || keep+=("$lib")
  done
  if (( ${#keep[@]} )); then
    LD_PRELOAD=${keep[0]}
    for lib in "${keep[@]:1}"; do LD_PRELOAD+=":$lib"; done
    export LD_PRELOAD
  else
    unset LD_PRELOAD
  fi
fi

# Start page, on loopback only. A second launch finds the port taken and
# reuses the first one's server.
port=8765
server=
if ! (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
  python3 -m http.server "$port" --bind 127.0.0.1 --directory "$here/start" >/dev/null 2>&1 &
  server=$!
  for _ in 1 2 3 4 5 6 7 8 9 10; do (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null && break; sleep 0.2; done
fi
(( $# )) || set -- "http://localhost:$port/?title=$name"

flags=(
  --user-data-dir="$HOME/.config/$name"
  --enable-features=OpenXR       # the Linux OpenXR device is off by default
  --ozone-platform=x11           # gamescope's X display, where each app is a panel
  --no-first-run --no-default-browser-check
  --password-store=basic         # a keyring prompt would be invisible in the headset
  --enable-logging=stderr
)
# Without the FramePlayer sandbox patches working, SteamVR refuses the session
# with the seccomp filter on; the non-sandboxed title keeps it off.
[[ $sandboxed == 1 ]] || flags+=(--disable-seccomp-filter-sandbox)

echo "flags: ${flags[*]} $*"
status=0
"$here/chrome" "${flags[@]}" "$@" || status=$?
[[ -n $server ]] && kill "$server" 2>/dev/null
echo "$(date -Is) exit $status"
exit "$status"
