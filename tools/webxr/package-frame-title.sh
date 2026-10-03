#!/bin/bash
# Package a finished build-frame-chromium.sh build as a Frame Control title:
# one .zip that installs from a link, plus one install manifest per title.
#
#   TAG=chromium-xr-frame-<version>-<n> tools/webxr/package-frame-title.sh
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
cd "$B"
files=(chrome chrome_crashpad_handler *.pak *.bin icudtl.dat locales product_logo_256.png BUILD-INFO.txt)
for f in libEGL.so libGLESv2.so libvk_swiftshader.so libvulkan.so.1 vk_swiftshader_icd.json; do
  [[ -e $f ]] && files+=("$f")
done
cp -r "${files[@]}" "$stage/chromium/"
cp -r "$FP/tools/webxr/frame-webxr-check" "$stage/chromium/start"
# Bundle three.js so the start page works offline on the headset.
three=$(grep -o 'https://cdn.jsdelivr.net/npm/three@[0-9.]*/build/three.module.js' "$stage/chromium/start/index.html")
curl -fsSL "$three" -o "$stage/chromium/start/three.module.js"
curl -fsSL "${three%/build/three.module.js}/LICENSE" -o "$stage/chromium/start/three.LICENSE"
sed -i "s#$three#./three.module.js#" "$stage/chromium/start/index.html"
grep -q '"three": "./three.module.js"' "$stage/chromium/start/index.html"
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

link() { python3 -c 'import sys, urllib.parse as u; print("frame-control://install?manifest=" + u.quote(sys.argv[1], safe=""))' "$1"; }
base="https://github.com/$REPO/releases/download/$TAG"
cat > "$OUT/INSTALL.md" <<EOF
Chromium with immersive WebXR for the Steam Frame, built from chromium/main
\`$(sed -n 's/^chromium\/src //p' "$B/BUILD-INFO.txt" | cut -c1-12)\` with FramePlayer's sandbox patches
(docs/webxr/patches 0001-0003) and IWFDK's Frame controller patch (0004).
Unofficial and experimental; based on
[saphid/chromium-webxr-steam-frame](https://github.com/saphid/chromium-webxr-steam-frame).

## Install with Frame Control

Two Steam titles from the same zip. Install either or both:

| Title | Seccomp filter | Install link |
|---|---|---|
| **Chromium XR** | off (known to work) | \`$(link "$base/chromium-xr.json")\` |
| **Chromium XR Sandboxed** | on (tests the sandbox patches) | \`$(link "$base/chromium-xr-sandboxed.json")\` |

Or give Frame Control the zip directly and pick \`chromium-xr.sh\` or
\`chromium-xr-sandboxed.sh\` as the program: $url

Each opens a start page with the **Frame WebXR check** (press Enter VR, press
every control, press Menu, hold both triggers and grips 2 s, then copy the
report) and links to Fish & Chips and other WebXR pages.
Logs: \`~/.local/state/chromium-xr-frame/\`.

sha256 \`$sha\`, $size bytes.

$(sed -n '/^patches:/,$p' "$B/BUILD-INFO.txt" | sed 's/^/    /')
EOF
ls -la "$OUT"
