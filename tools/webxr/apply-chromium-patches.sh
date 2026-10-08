#!/usr/bin/env bash
# Apply the Steam Frame WebXR patch series to a Chromium checkout, as commits.
# Usage: tools/webxr/apply-chromium-patches.sh /path/to/chromium/src
#
# The series is FramePlayer's patches (docs/webxr/patches) plus IWFDK's Frame
# controller patch 0004 (yellkell/iwfdk, platform/chromium/patches), applied in
# file-name order: they stack (0006 and 0008 on 0004, 0011 on 0009, 0012 on
# 0008, 0013 on 0005). Written against chromium/main 2255089d4176, which
# includes e0f937bba4ff ("Reland 'vr: run the XR device service in the
# sandbox on Linux'"). IWFDK's patches are fetched from IWFDK_REPO at
# IWFDK_REF, or taken from a local directory with IWFDK_PATCHES=dir.
set -euo pipefail
src="${1:?path to chromium/src required}"
here="$(cd "$(dirname "$0")/../.." && pwd)"
series="$(mktemp -d)"
trap 'rm -rf "$series"' EXIT
cp "$here"/docs/webxr/patches/*.patch "$series/"
if [ -z "${IWFDK_PATCHES:-}" ]; then
  git clone -q --depth 1 --filter=blob:none --sparse -b "${IWFDK_REF:-feat/frame-sdk}" \
    "${IWFDK_REPO:-https://github.com/yellkell/iwfdk}" "$series/iwfdk"
  git -C "$series/iwfdk" sparse-checkout set platform/chromium/patches
  IWFDK_PATCHES="$series/iwfdk/platform/chromium/patches"
fi
# IWFDK carries a copy of FramePlayer's 0006; a same-named patch is applied once.
for p in "$IWFDK_PATCHES"/*.patch; do
  q="$series/$(basename "$p")"
  if [ -e "$q" ] && ! cmp -s "$p" "$q"; then
    echo "error: $(basename "$p") differs between FramePlayer and IWFDK" >&2; exit 1
  fi
  cp "$p" "$series/"
done
for p in "$series"/*.patch; do
  if grep -q $'\r$' "$p"; then
    echo "error: $(basename "$p") has CRLF line endings; check it out with LF" >&2; exit 1
  fi
done

cd "$src"
if [ ! -f content/services/isolated_xr_device/xr_sandbox_hook_linux.cc ]; then
  echo "warning: XR sandbox CL (e0f937bba4ff) is not in this checkout; patches 2 and 3 will not apply." >&2
fi
for p in "$series"/*.patch; do
  echo "==> $(basename "$p")"
  git apply --check --3way "$p"
  git am --3way "$p"
done
echo "done. Validate with:"
echo "  autoninja -C out/Default sandbox_linux_unittests content_unittests device_unittests"
echo "  out/Default/sandbox_linux_unittests --gtest_filter='BrokerProcess.RewriteProcSelf*'"
echo "  out/Default/content_unittests --gtest_filter='XrSandboxHookLinuxTest.*'"
echo "  out/Default/device_unittests --gtest_filter='OpenXrInteractionProfilesTest.*'"
