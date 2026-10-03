#!/bin/bash
# FramePlayer launcher: what Steam (or Frame Control/FrameDrop) runs.
#
# Uses the FFmpeg libraries shipped in lib/.
here="$(cd "$(dirname "$0")" && pwd)"
export LD_LIBRARY_PATH="$here/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

# Optional environment for runs started from Steam, which can't pass any:
# KEY=value lines, e.g. FRAMEPLAYER_DEBUG_INPUT=1 or RUST_LOG=debug.
envfile=${XDG_CONFIG_HOME:-$HOME/.config}/frameplayer/env
if [[ -f $envfile ]]; then
  set -a
  # shellcheck disable=SC1090
  . "$envfile"
  set +a
fi

# SteamVR runs each app at its own per-app video settings and overrides what
# the app asks for through OpenXR, so set this Steam app's defaults unless
# they were already chosen in SteamVR's per-app video settings:
# - 90 Hz (SteamVR's default is 72 Hz). FRAMEPLAYER_REFRESH_RATE=0 leaves it alone.
# - 2160 pixels per eye, the Frame's panel resolution (SteamVR's default is
#   1728, 80%). FRAMEPLAYER_RESOLUTION=0 leaves it alone; lower it if playback
#   stutters.
vrcmd=/opt/steamvr/bin/linuxarm64/vrcmd
vrsettings=$HOME/.config/openvr/config/steamvr.vrsettings
app_default() { # KEY TYPE VALUE
  [[ -n ${SteamAppId:-} && $3 != 0 && -x $vrcmd ]] || return 0
  python3 -c 'import json, sys
s = json.load(open(sys.argv[1])).get("steam.app." + sys.argv[2], {})
sys.exit(0 if sys.argv[3] in s else 1)' "$vrsettings" "$SteamAppId" "$1" 2>/dev/null && return 0
  LD_LIBRARY_PATH=${vrcmd%/*} "$vrcmd" --set-settings-"$2" "steam.app.$SteamAppId.$1" "$3" >/dev/null 2>&1 || true
}
app_default preferredRefreshRate float "${FRAMEPLAYER_REFRESH_RATE:-90}"
app_default resolutionOverride int "${FRAMEPLAYER_RESOLUTION:-2160}"

exec "$here/frameplayer" "$@"
