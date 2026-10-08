#!/bin/bash
# Chromium XR for the Steam Frame with the seccomp filter ON: tests the
# FramePlayer sandbox patches (docs/webxr/patches 0001-0003).
export CHROMIUM_XR_SANDBOXED=1
exec "$(dirname "$(readlink -f "$0")")/chromium/launch.sh" "$@"
