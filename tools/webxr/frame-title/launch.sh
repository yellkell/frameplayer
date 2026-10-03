#!/bin/bash
# Starts Chromium XR from the Frame Control / Steam devkit title. The title's
# top-level chromium-xr.sh and chromium-xr-sandboxed.sh call this with
# CHROMIUM_XR_SANDBOXED=0 or 1. Steam passes no arguments, so it opens the
# page in ~/.config/chromium-xr-frame/home-url, else Fish & Chips.
# Arguments, if any, go to Chromium: `chromium-xr.sh URL`.
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

# The page to open when started without one (from the Steam library or by
# FramePlayer through Steam). A running browser opens it in a new tab.
home_url=https://yellkell.com/fac
home_file=$HOME/.config/chromium-xr-frame/home-url
if [[ -s $home_file ]]; then
  read -r saved <"$home_file" || true
  [[ $saved == http://* || $saved == https://* ]] && home_url=$saved
fi
(( $# )) || set -- "$home_url"

# SteamVR runs each app at its own per-app video settings and overrides what
# the app asks for through OpenXR, so set this Steam app's defaults unless
# they were already chosen in SteamVR's per-app video settings:
# - 90 Hz (SteamVR's default is 72 Hz). CHROMIUM_XR_REFRESH_RATE=0 leaves it alone.
# - 2160 pixels per eye, the Frame's panel resolution (SteamVR's default is
#   1728, 80%). CHROMIUM_XR_RESOLUTION=0 leaves it alone; lower it if a page
#   stutters.
vrcmd=/opt/steamvr/bin/linuxarm64/vrcmd
vrsettings=$HOME/.config/openvr/config/steamvr.vrsettings
app_default() { # KEY TYPE VALUE
  [[ -n ${SteamAppId:-} && $3 != 0 && -x $vrcmd ]] || return 0
  python3 -c 'import json, sys
s = json.load(open(sys.argv[1])).get("steam.app." + sys.argv[2], {})
sys.exit(0 if sys.argv[3] in s else 1)' "$vrsettings" "$SteamAppId" "$1" 2>/dev/null && return 0
  LD_LIBRARY_PATH=${vrcmd%/*} "$vrcmd" --set-settings-"$2" "steam.app.$SteamAppId.$1" "$3" >/dev/null 2>&1 || true
  echo "set SteamVR $1=$3 for steam.app.$SteamAppId"
}
app_default preferredRefreshRate float "${CHROMIUM_XR_REFRESH_RATE:-90}"
app_default resolutionOverride int "${CHROMIUM_XR_RESOLUTION:-2160}"

flags=(
  --user-data-dir="$HOME/.config/$name"
  --enable-features=OpenXR       # the Linux OpenXR device is off by default
  --ozone-platform=x11           # gamescope's X display, where each app is a panel
  --no-first-run --no-default-browser-check
  --password-store=basic         # a keyring prompt would be invisible in the headset
  --enable-logging=stderr
  # The Linux OpenXR backend can't composite WebXR layers, but pages are still
  # offered XRWebGLBinding.createProjectionLayer, and three.js (r16x+) always
  # draws into a projection layer when offered one: the headset stayed black.
  # Without the API three.js falls back to an XRWebGLLayer, which works.
  --disable-blink-features=WebXRLayers
)
# Without the FramePlayer sandbox patches working, SteamVR refuses the session
# with the seccomp filter on; the non-sandboxed title keeps it off.
# --test-type hides the "unsupported command-line flag" bar the flag below
# would put over every page (the flag is deliberate; see docs/webxr).
[[ $sandboxed == 1 ]] || flags+=(--disable-seccomp-filter-sandbox --test-type)
# Steam Frame controller models where pages ask for Quest Touch ones: the
# extension made by tools/webxr/frame-models, shipped in the title. Chromium
# only loads extensions from the command line with this feature off.
models="$here/../frame-models"
if [[ -f $models/manifest.json ]]; then
  flags+=(--load-extension="$(cd "$models" && pwd)"
    --disable-features=DisableLoadExtensionCommandLineSwitch)
fi
# DevTools on loopback only while ~/.config/chromium-xr-frame-devtools exists,
# for remote debugging over SSH (ssh -L 9223:127.0.0.1:9223). It has no
# authentication, so remove the file when done.
[[ -f $HOME/.config/chromium-xr-frame-devtools ]] && flags+=(--remote-debugging-port=9223)

echo "flags: ${flags[*]} $*"
status=0
"$here/chrome" "${flags[@]}" "$@" || status=$?
echo "$(date -Is) exit $status"
exit "$status"
