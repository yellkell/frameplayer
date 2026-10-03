#!/bin/sh
# FramePlayer launcher: what Steam (or Frame Control/FrameDrop) runs.
# Uses the FFmpeg libraries shipped in lib/ and logs to
# ~/.local/share/frameplayer/frameplayer.log.
here="$(cd "$(dirname "$0")" && pwd)"
export LD_LIBRARY_PATH="$here/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
# PipeWire's ALSA plugin is the default route on SteamOS; keep it.
exec "$here/frameplayer" "$@"
