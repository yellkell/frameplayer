#!/bin/bash
# FramePlayer launcher: what Steam (or Frame Control/FrameDrop) runs.
#
# Uses the FFmpeg libraries shipped in lib/. Also runs WebXR hand-offs:
# when FramePlayer exits with code 75 it has written a browser command to
# ~/.local/share/frameplayer/handoff; that browser gets the headset, and
# FramePlayer starts again when it closes. Steam sees one app throughout.
# Exit code 76 is the Web XR tab's embedded browser: a page asked for the
# headset, so wait for its VR session to end (the browser writes xr-ended)
# and start FramePlayer again on the Web XR tab.
here="$(cd "$(dirname "$0")" && pwd)"
export LD_LIBRARY_PATH="$here/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export FRAMEPLAYER_LAUNCHER=1

# Optional environment for runs started from Steam, which can't pass any:
# KEY=value lines, e.g. FRAMEPLAYER_DEBUG_INPUT=1 or RUST_LOG=debug.
envfile=${XDG_CONFIG_HOME:-$HOME/.config}/frameplayer/env
if [[ -f $envfile ]]; then
  set -a
  # shellcheck disable=SC1090
  . "$envfile"
  set +a
fi

# 90 Hz, for FramePlayer and the WebXR browser it hands over to (Steam sees
# one app). SteamVR runs each app at its per-app preferredRefreshRate (72 Hz
# unless set) and overrides OpenXR requests, so set it for this Steam app
# unless one was chosen in SteamVR's per-app video settings.
# FRAMEPLAYER_REFRESH_RATE=0 leaves it alone.
rate=${FRAMEPLAYER_REFRESH_RATE:-90}
vrcmd=/opt/steamvr/bin/linuxarm64/vrcmd
vrsettings=$HOME/.config/openvr/config/steamvr.vrsettings
if [[ -n ${SteamAppId:-} && $rate != 0 && -x $vrcmd ]] &&
    ! python3 -c 'import json, sys
s = json.load(open(sys.argv[1])).get("steam.app." + sys.argv[2], {})
sys.exit(0 if "preferredRefreshRate" in s else 1)' "$vrsettings" "$SteamAppId" 2>/dev/null; then
  LD_LIBRARY_PATH=${vrcmd%/*} "$vrcmd" --set-settings-float \
    "steam.app.$SteamAppId.preferredRefreshRate" "$rate" >/dev/null 2>&1 || true
fi

handoff="${XDG_DATA_HOME:-$HOME/.local/share}/frameplayer/handoff"
log="${XDG_DATA_HOME:-$HOME/.local/share}/frameplayer/handoff.log"

without_steam_overlay() {
  # Steam preloads its overlay into everything it starts; it crashes
  # Chromium's zygote. Keep any other preloaded library.
  local keep=() lib
  IFS=' :' read -r -a libs <<<"${LD_PRELOAD:-}"
  for lib in "${libs[@]}"; do
    [[ -z "$lib" || "$lib" == *gameoverlayrenderer.so ]] || keep+=("$lib")
  done
  if ((${#keep[@]})); then
    (IFS=:; LD_PRELOAD="${keep[*]}" "$@")
  else
    env -u LD_PRELOAD "$@"
  fi
}

webxr="${XDG_DATA_HOME:-$HOME/.local/share}/frameplayer/web-xr"
webpid="${XDG_DATA_HOME:-$HOME/.local/share}/frameplayer/web/frameplayer-browser.pid"

# Until the embedded browser's VR session ends, or the browser is gone.
wait_for_web_xr() {
  local pid
  pid=$(cat "$webpid" 2>/dev/null)
  while [[ ! -e "$webxr/xr-ended" ]]; do
    [[ -n "$pid" ]] && ! kill -0 "$pid" 2>/dev/null && break
    sleep 0.25
  done
  rm -f "$webxr/xr-ended" "$webxr/xr-ready"
}

while :; do
  rm -f "$handoff"
  "$here/frameplayer" "$@"
  rc=$?
  if [[ $rc -eq 76 ]]; then
    # FramePlayer has exited; give SteamVR a moment to drop its session,
    # then tell the browser (it waits up to 8 s for this).
    sleep 0.5
    mkdir -p "$webxr" && : >"$webxr/xr-ready"
    echo "$(date -Is) web view page has the headset" >>"$log"
    wait_for_web_xr
    echo "$(date -Is) web view page's VR session ended" >>"$log"
    set --
    export FRAMEPLAYER_RESUMED=web
    continue
  fi
  [[ $rc -eq 75 && -s "$handoff" ]] || exit $rc
  mapfile -d '' -t argv <"$handoff"
  rm -f "$handoff"
  ((${#argv[@]})) || exit 1
  echo "$(date -Is) running ${argv[*]}" >>"$log"
  without_steam_overlay "${argv[@]}" >>"$log" 2>&1
  echo "$(date -Is) browser exited ($?)" >>"$log"
  # Back to the library, not to whatever file FramePlayer was started with,
  # waiting for SteamVR if it is still busy with the browser's session.
  set --
  export FRAMEPLAYER_RESUMED=1
done
