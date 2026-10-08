#!/bin/bash
# Chromium XR for the Steam Frame, seccomp filter off (works without the
# FramePlayer sandbox patches). Frame Control / Steam runs this file.
export CHROMIUM_XR_SANDBOXED=0
exec "$(dirname "$(readlink -f "$0")")/chromium/launch.sh" "$@"
