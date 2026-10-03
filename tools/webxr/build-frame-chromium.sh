#!/bin/bash
# Build the Steam Frame WebXR browser: arm64 Chromium with immersive WebXR
# through SteamVR, plus FramePlayer's patches (docs/webxr/patches: 0001-0003
# sandbox, 0005 rendering) and IWFDK's Frame controller patch (0004).
#
# Adapted from saphid/chromium-webxr-steam-frame build/build.sh (BSD-3).
# Differences: the OpenXR-on-Linux CL it pinned (8132979) has merged, so this
# builds chromium/main at BASE, the commit the four patches were written
# against; saphid's SO_PEERCRED patch is replaced by 0003; and the disk guard
# also watches the Windows drive under WSL, where df on the distro's own
# filesystem reports the VHDX's 1 TB virtual size rather than real space.
#
# Runs on x86-64 Linux (WSL2 is fine), no root needed:
#   tmux new -d -s chromium-xr 'tools/webxr/build-frame-chromium.sh > ~/chromium-xr/build.log 2>&1'
#   tail -F ~/chromium-xr/stage
#
# Re-running resumes. The result is $W/chromium-xr-arm64.tar.xz, installable
# on the Frame with saphid's frame/install.sh. With BUILD_TESTS=1 (default)
# it then builds the arm64 unit tests for the patches, to run on the Frame.
set -euo pipefail

W=${CHROMIUM_XR_DIR:-$HOME/chromium-xr}
FP=$(cd "$(dirname "$0")/../.." && pwd)
# chromium/main 2026-10-03 00:27 UTC; includes CL 8441736 (XR sandbox) and
# CL 8132979 (OpenXR device on Linux), and is the base of patches 0001-0004.
BASE=${BASE:-2255089d4176f1957c31201354aef8b21b78e799}
IWFDK_REPO=${IWFDK_REPO:-https://github.com/yellkell/iwfdk}
IWFDK_REF=${IWFDK_REF:-feat/frame-sdk}
BUILD_TESTS=${BUILD_TESTS:-1}
MIN_FREE_GB=${MIN_FREE_GB:-10}
# Extra filesystems to keep free space on: the drive holding the WSL VHDX.
GUARD_PATHS=${GUARD_PATHS:-$( [ -d /mnt/d ] && echo /mnt/d )}

mkdir -p "$W"
W=$(cd "$W" && pwd)
cd "$W"
stage() { echo "$(date -Is) $*" | tee -a "$W/stage"; }
guard() {
  for p in "$W" $GUARD_PATHS; do
    avail=$(df --output=avail -BG "$p" | tail -n 1 | tr -dc 0-9)
    if [ "$avail" -lt "$MIN_FREE_GB" ]; then stage "ABORT: only ${avail}G free on $p"; return 3; fi
  done
}

# Patches, in file-name order: FramePlayer's from this checkout (0001-0003
# sandbox, 0005 rendering), IWFDK's 0004 (Frame controllers).
P="$W/patches"
rm -rf "$P" "$W/iwfdk"
mkdir -p "$P"
cp "$FP"/docs/webxr/patches/*.patch "$P/"
git clone -q --depth 1 --filter=blob:none --sparse -b "$IWFDK_REF" "$IWFDK_REPO" "$W/iwfdk"
git -C "$W/iwfdk" sparse-checkout set platform/chromium/patches
cp "$W"/iwfdk/platform/chromium/patches/*.patch "$P/"
stage "patches: $(cd "$P" && ls | tr '\n' ' ')(iwfdk $(git -C "$W/iwfdk" rev-parse --short HEAD), frameplayer $(git -C "$FP" rev-parse --short HEAD 2>/dev/null || echo '?'))"

if [ ! -x depot_tools/gclient ]; then
  rm -rf depot_tools
  git clone -q https://chromium.googlesource.com/chromium/tools/depot_tools.git
fi
export PATH="$W/depot_tools:$PATH" DEPOT_TOOLS_UPDATE=1 DEPOT_TOOLS_METRICS=0

if [ ! -f .gclient ]; then
  cat > .gclient <<'G'
solutions = [{ "name": "src", "url": "https://chromium.googlesource.com/chromium/src.git",
  "managed": False, "custom_deps": {}, "custom_vars": { "checkout_nacl": False } }]
target_os = ["linux"]
target_cpu = ["arm64"]
G
fi

if ! git -C src rev-parse -q --verify HEAD >/dev/null 2>&1 ||
    [ "$(cat "$W/base" 2>/dev/null)" != "$BASE" ]; then
  stage "fetch src at $BASE"
  mkdir -p src
  [ -d src/.git ] || git -C src init -q
  git -C src remote get-url origin >/dev/null 2>&1 ||
    git -C src remote add origin https://chromium.googlesource.com/chromium/src.git
  # By SHA first; if the server won't serve that, a shallow main that reaches it.
  git -C src fetch -q --depth=1 origin "$BASE" ||
    git -C src fetch -q --shallow-since=2026-10-01 origin main
  git -C src checkout -q --force "$BASE"
  echo "$BASE" > "$W/base"
fi
guard
stage "src at $(git -C src log -1 --format='%h %cI %s')"

# Fail now, not after hours of syncing, if a patch doesn't fit this checkout.
# Each patch touches its own files, so checking them one by one is enough.
for p in "$P"/*.patch; do
  git -C src apply --reverse --check "$p" 2>/dev/null || git -C src apply --check "$p" ||
    { stage "ABORT: $(basename "$p") does not apply to $BASE"; exit 4; }
done
stage "all patches apply"

rev=$(git -C src rev-parse HEAD)
# Sync once per revision; gclient sync refuses a checkout with applied patches.
if [ "$(cat "$W/synced" 2>/dev/null)" != "$rev" ]; then
  stage "gclient sync"
  gclient sync --nohooks --no-history -D --shallow --revision "src@$rev" -j 16
  guard
  stage "runhooks"
  gclient runhooks
  src/build/linux/sysroot_scripts/install-sysroot.py --arch=arm64
  echo "$rev" > "$W/synced"
fi
guard

cd src
for p in "$P"/*.patch; do
  if ! git apply --reverse --check "$p" 2>/dev/null; then
    git apply "$p"
    stage "applied $(basename "$p")"
  fi
done

mkdir -p out/XR
# Rewritten on every run: change build settings here, not in out/XR/args.gn.
cat > out/XR/args.gn <<'A'
target_os = "linux"
target_cpu = "arm64"
is_debug = false
is_official_build = false
is_component_build = false
dcheck_always_on = false
symbol_level = 0
blink_symbol_level = 0
v8_symbol_level = 0
proprietary_codecs = true
ffmpeg_branding = "Chrome"
enable_nacl = false
use_remoteexec = false
use_siso = true
treat_warnings_as_errors = false
A
stage "gn gen"
gn gen out/XR
gn args out/XR --list=enable_openxr --short | tee -a "$W/stage"

( while sleep 300; do guard || { pkill -u "$(id -u)" -f "(siso|ninja).*out/XR"; exit 3; }; done ) &
GUARD=$!
trap 'kill $GUARD 2>/dev/null || true' EXIT

stage "build chrome"
autoninja -C out/XR chrome chrome_sandbox chrome_crashpad_handler

stage "package"
cp chrome/app/theme/chromium/product_logo_256.png out/XR/product_logo_256.png
{
  echo "chromium/src $BASE"
  echo "built $(date -Is) on $(uname -m)"
  echo "patches:"; (cd "$P" && sha256sum *.patch)
} > out/XR/BUILD-INFO.txt
cd out/XR
files=(chrome chrome_sandbox chrome_crashpad_handler *.pak *.bin icudtl.dat locales product_logo_256.png BUILD-INFO.txt)
for f in libEGL.so libGLESv2.so libvk_swiftshader.so libvulkan.so.1 vk_swiftshader_icd.json; do
  [ -e "$f" ] && files+=("$f")
done
tar -cf - "${files[@]}" | xz -T0 -6 > "$W/chromium-xr-arm64.tar.xz"
stage "DONE $(ls -la "$W/chromium-xr-arm64.tar.xz")"
cd ../..

if [ "$BUILD_TESTS" = 1 ]; then
  # arm64 binaries: run them on the Frame (see docs/webxr/README.md section 5).
  stage "build tests"
  autoninja -C out/XR device_unittests sandbox_linux_unittests
  stage "TESTS BUILT out/XR/device_unittests out/XR/sandbox_linux_unittests"
fi
