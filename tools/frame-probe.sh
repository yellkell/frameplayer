#!/usr/bin/env bash
# Run frame-probe on a paired Steam Frame over SSH and copy the report back.
#
# Prerequisites on the headset: Developer Mode on, and SSH pairing done once
# with Frame Control, FrameDrop, or Valve's SteamOS Devkit Client. Frame
# Control writes a `Host frame` entry to ~/.ssh/config, which is the default
# here; set FRAME_HOST for anything else (e.g. FRAME_HOST=deck@192.168.1.50).
#
# Put the headset on (or at least wake it) so SteamVR has an active system,
# otherwise the OpenXR answers come back UNKNOWN.
#
# Usage: tools/frame-probe.sh [--build] [extra frame-probe args, e.g. --no-session]
set -euo pipefail
cd "$(dirname "$0")/.."
host="${FRAME_HOST:-frame}"
bin=target/aarch64-unknown-linux-gnu/release/frame-probe

if [[ "${1:-}" == "--build" || ! -x "$bin" ]]; then
  [[ "${1:-}" == "--build" ]] && shift
  tools/build-frame.sh -p frame-probe
fi

echo "==> copying probe to $host"
ssh "$host" 'mkdir -p ~/frameplayer-probe'
scp -q "$bin" "$host:frameplayer-probe/frame-probe"

echo "==> running on $host"
# Over SSH there is no graphical session environment; point at the user's
# runtime dir so anything that expects it (PipeWire, Wayland) resolves.
ssh -t "$host" 'export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"; ~/frameplayer-probe/frame-probe '"$*"

echo "==> fetching reports"
mkdir -p probe-reports
scp -q "$host:frameplayer-probe/probe-*" probe-reports/
ls -1t probe-reports | head -2
echo "Paste the newest .txt into docs/platform-notes.md (see that file)."
