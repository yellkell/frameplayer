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

# 90 Hz. SteamVR runs each app at its per-app preferredRefreshRate (72 Hz
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

exec "$here/frameplayer" "$@"
