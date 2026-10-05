#!/bin/bash
# Package a finished build-frame-chromium.sh build as a Frame Control title:
# one .zip that installs from a link, plus one install manifest per title.
#
#   TAG=chromium-xr-frame-<version>-<n> FRAME_MODELS_EXT=<extension dir> #     tools/webxr/package-frame-title.sh
#
# Output in $W/release:
#   ChromiumXR-Frame-arm64.zip         Chromium in chromium/, launchers on top
#   chromium-xr.json                   "Chromium XR" (seccomp off)
#   chromium-xr-sandboxed.json         "Chromium XR Sandboxed" (seccomp on)
#   INSTALL.md                         release notes with the install links
# The manifests point at the zip as an asset of GitHub release $TAG in $REPO.
set -euo pipefail

W=${CHROMIUM_XR_DIR:-$HOME/chromium-xr}
FP=$(cd "$(dirname "$0")/../.." && pwd)
TAG=${TAG:?set TAG, the GitHub release tag the zip will be uploaded to}
REPO=${REPO:-yellkell/frameplayer}
B=$W/src/out/XR
OUT=$W/release
ZIP=ChromiumXR-Frame-arm64.zip
[[ -x $B/chrome && -f $B/BUILD-INFO.txt ]] || { echo "no finished build in $B" >&2; exit 1; }

stage=$W/title
rm -rf "$stage" "$OUT"
mkdir -p "$stage/chromium" "$OUT"
install -m 755 "$FP"/tools/webxr/frame-title/chromium-xr.sh "$FP"/tools/webxr/frame-title/chromium-xr-sandboxed.sh "$stage/"
install -m 755 "$FP"/tools/webxr/frame-title/launch.sh "$stage/chromium/"
# Steam Frame controller models for pages that ask for Quest Touch ones: the
# extension made by tools/webxr/frame-models (bake_touch_glb.py, then
# make_extension.py) from an extraction on a Frame. The launcher loads it.
FRAME_MODELS_EXT=${FRAME_MODELS_EXT:?set FRAME_MODELS_EXT to the controller models extension}
[[ -f $FRAME_MODELS_EXT/manifest.json ]] || { echo "no manifest.json in $FRAME_MODELS_EXT" >&2; exit 1; }
cp -r "$FRAME_MODELS_EXT" "$stage/frame-models"
# Steam library artwork (tools/webxr/make-chromium-xr-artwork.py), where
# frame-apps-install.py and Frame Control look for it.
mkdir -p "$stage/assets/steam"
cp "$FP"/tools/webxr/frame-title/steam/*.png "$stage/assets/steam/"
cd "$B"
files=(chrome chrome_crashpad_handler *.pak *.bin icudtl.dat locales product_logo_256.png BUILD-INFO.txt)
for f in libEGL.so libGLESv2.so libvk_swiftshader.so libvulkan.so.1 vk_swiftshader_icd.json; do
  [[ -e $f ]] && files+=("$f")
done
cp -r "${files[@]}" "$stage/chromium/"
cp "$W/src/LICENSE" "$stage/chromium/LICENSE.chromium"

# Files at the zip root (no single top folder), so the manifests' "exe"
# paths are relative to the title folder.
(cd "$stage" && zip -q -r -X "$OUT/$ZIP" .)
sha=$(sha256sum "$OUT/$ZIP" | cut -d' ' -f1)
size=$(stat -c %s "$OUT/$ZIP")
url="https://github.com/$REPO/releases/download/$TAG/$ZIP"

manifest() {  # name exe file
  python3 - "$1" "$2" "$url" "$sha" "$size" > "$OUT/$3" <<'P'
import json, sys
name, exe, url, sha, size = sys.argv[1:]
print(json.dumps({"schema": "frame-control.install/v1", "name": name,
  "files": [{"url": url, "sha256": sha, "size": int(size), "exe": exe}]}, indent=2))
P
}
manifest "Chromium XR" chromium-xr.sh chromium-xr.json
manifest "Chromium XR Sandboxed" chromium-xr-sandboxed.sh chromium-xr-sandboxed.json

# The patches' unit tests, for tools/webxr/frame-install.sh or an arm64 runner.
if [[ -x $B/device_unittests && -x $B/sandbox_linux_unittests ]]; then
  (cd "$B" && tar -cf - device_unittests sandbox_linux_unittests | xz -T0 -6 > "$OUT/chromium-xr-tests-arm64.tar.xz")
fi
cp "$W/chromium-xr-arm64.tar.xz" "$OUT/"

link() { python3 -c 'import sys, urllib.parse as u; print("frame-control://install?manifest=" + u.quote(sys.argv[1], safe=""))' "$1"; }
base="https://github.com/$REPO/releases/download/$TAG"
cat > "$OUT/INSTALL.md" <<EOF
Chromium with immersive WebXR for the Steam Frame, built from chromium/main
\`$(sed -n 's/^chromium\/src //p' "$B/BUILD-INFO.txt" | cut -c1-12)\` with the patches listed below.
Unofficial and experimental; based on
[saphid/chromium-webxr-steam-frame](https://github.com/saphid/chromium-webxr-steam-frame).

What works on the Frame:

- Opens Fish & Chips (https://yellkell.com/fac) at 90 Hz and 2160 pixels per eye
  (SteamVR per-app settings, unless you chose your own). To start on another
  page, put its address in \`~/.config/chromium-xr-frame/home-url\`.
- Both eyes render (patch 0005; WebXR layers are off, they render black).
- The controllers work like Quest Touch controllers in games made for Quest
  (patch 0006), and look like Steam Frame controllers: pages that load the
  Quest Touch models get the Frame's, with trigger, grip and stick moving.
- Controller vibration (\`gamepad.vibrationActuator\`, patch 0008).
- No "unsupported command-line flag" bar.

## Install with Frame Control

Two Steam titles from the same zip. Install either or both:

| Title | Seccomp filter | Install link |
|---|---|---|
| **Chromium XR** | off (known to work) | \`$(link "$base/chromium-xr.json")\` |
| **Chromium XR Sandboxed** | on (tests the sandbox patches) | \`$(link "$base/chromium-xr-sandboxed.json")\` |

Or give Frame Control the zip directly and pick \`chromium-xr.sh\` or
\`chromium-xr-sandboxed.sh\` as the program: $url

Logs: \`~/.local/state/chromium-xr-frame/\`.

The Steam Frame controller models (\`frame-models/\`) are Valve's, from SteamVR,
converted for WebXR pages by FramePlayer's tools/webxr/frame-models.

sha256 \`$sha\`, $size bytes.

$(sed -n '/^patches:/,$p' "$B/BUILD-INFO.txt" | sed 's/^/    /')
EOF
ls -la "$OUT"
