#!/usr/bin/env bash
# Apply the FramePlayer WebXR-on-Steam-Frame sandbox patches to a Chromium
# checkout. Usage: tools/webxr/apply-chromium-patches.sh /path/to/chromium/src
#
# The patches were generated against chromium/main as of 2026-10-02, after
# commit e0f937bba4ff ("Reland 'vr: run the XR device service in the sandbox
# on Linux'"). They need that commit to be present.
set -euo pipefail
src="${1:?path to chromium/src required}"
here="$(cd "$(dirname "$0")/../.." && pwd)"
patches="$here/docs/webxr/patches"
cd "$src"
if ! git log --oneline -1 --grep="run the XR device service in the sandbox on Linux" >/dev/null 2>&1 \
   || [ -z "$(git log --oneline --grep="run the XR device service in the sandbox on Linux" | head -1)" ]; then
  echo "warning: XR sandbox CL (e0f937bba4ff) not found in history; patches 2 and 3 will not apply." >&2
fi
for p in "$patches"/000*.patch; do
  echo "==> $(basename "$p")"
  git apply --check --3way "$p"
  git am --3way "$p"
done
echo "done. Validate with:"
echo "  autoninja -C out/Default sandbox_linux_unittests content_unittests"
echo "  out/Default/sandbox_linux_unittests --gtest_filter='BrokerProcess.RewriteProcSelf*'"
echo "  out/Default/content_unittests --gtest_filter='XrSandboxHookLinuxTest.*'"
