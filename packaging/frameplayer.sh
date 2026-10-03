#!/bin/bash
# FramePlayer launcher: what Steam (or Frame Control/FrameDrop) runs.
#
# Uses the FFmpeg libraries shipped in lib/. Also runs WebXR hand-offs:
# when FramePlayer exits with code 75 it has written a browser command to
# ~/.local/share/frameplayer/handoff; that browser gets the headset, and
# FramePlayer starts again when it closes. Steam sees one app throughout.
here="$(cd "$(dirname "$0")" && pwd)"
export LD_LIBRARY_PATH="$here/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export FRAMEPLAYER_LAUNCHER=1
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

while :; do
  rm -f "$handoff"
  "$here/frameplayer" "$@"
  rc=$?
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
