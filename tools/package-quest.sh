#!/usr/bin/env bash
# Build and package FramePlayer for Meta Quest as an APK.
#
# Usage: tools/package-quest.sh [OUT_DIR]          (default: dist-quest)
#
# Needs: cargo-ndk and the aarch64-linux-android Rust target, the Android SDK
# ($ANDROID_HOME, else the default Android Studio location) with build-tools
# and a platform >= 32, the NDK ($ANDROID_NDK_HOME), Java for apksigner, and
# FFmpeg from tools/build-ffmpeg-android.sh in third_party/ffmpeg/android-aarch64.
# Works on Linux and in Git Bash on Windows.
#
# Signing: $QUEST_KEYSTORE (+ $QUEST_KEYSTORE_PASS, $QUEST_KEY_ALIAS), else the
# Android debug keystore. Version code: $QUEST_VERSION_CODE, else the number
# of commits, which only grows.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
out="${1:-$root/dist-quest}"
work="$root/target/quest-apk"
API=29        # oldest Android (Quest system) supported
TARGET_API=32 # what Meta's store asks for
LOADER_VER=1.1.65
LOADER_SHA256=a298c326085b09181c85235fb04d37510939bd30938ed4322c093c09780e8643

py="$(command -v python3 || command -v python)"
exe="" bat=""
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) exe=.exe bat=.bat ;; esac

sdk="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-${LOCALAPPDATA:-$HOME}/Android/Sdk}}"
[ -d "$sdk" ] || { echo "Android SDK not found (set ANDROID_HOME)" >&2; exit 1; }
bt="$(ls -d "$sdk"/build-tools/*/ | sort -V | tail -1)"
platform="$(ls -d "$sdk"/platforms/android-*/ | sort -V | tail -1)"
[ -f "$platform/android.jar" ] || { echo "no Android platform in $sdk/platforms" >&2; exit 1; }
: "${ANDROID_NDK_HOME:?set ANDROID_NDK_HOME to the Android NDK}"
export ANDROID_NDK_HOME

version="$(grep -m1 '^version' "$root/crates/frameplayer/Cargo.toml" | cut -d'"' -f2)"
code="${QUEST_VERSION_CODE:-$(git -C "$root" rev-list --count HEAD)}"
ffmpeg="$root/third_party/ffmpeg/android-aarch64/lib"
[ -f "$ffmpeg/libavcodec.so" ] || { echo "FFmpeg for Android missing: run tools/build-ffmpeg-android.sh" >&2; exit 1; }

rm -rf "$work"; mkdir -p "$work/lib/arm64-v8a" "$work/res/mipmap" "$out"

echo "== native library"
(cd "$root" && cargo ndk -t arm64-v8a -P "$API" -o "$work/lib" build --release -p frameplayer-quest)
cp "$ffmpeg"/lib*.so "$work/lib/arm64-v8a/"

echo "== OpenXR loader $LOADER_VER"
aar="$root/third_party/src/openxr_loader_for_android-$LOADER_VER.aar"
mkdir -p "$root/third_party/src"
[ -f "$aar" ] || curl -fsSL --retry 4 -o "$aar" \
  "https://repo1.maven.org/maven2/org/khronos/openxr/openxr_loader_for_android/$LOADER_VER/openxr_loader_for_android-$LOADER_VER.aar"
echo "$LOADER_SHA256  $aar" | sha256sum -c --quiet - || { rm -f "$aar"; echo "OpenXR loader checksum mismatch" >&2; exit 1; }
"$py" -I -c "import sys, zipfile; open(sys.argv[2], 'wb').write(zipfile.ZipFile(sys.argv[1]).read('jni/arm64-v8a/libopenxr_loader.so'))" \
  "$aar" "$work/lib/arm64-v8a/libopenxr_loader.so"

echo "== APK $version ($code)"
cp "$root/assets/steam/icon.png" "$work/res/mipmap/ic_launcher.png"
"$bt/aapt2$exe" compile --dir "$work/res" -o "$work/res.zip"
"$bt/aapt2$exe" link -o "$work/unsigned.apk" -I "$platform/android.jar" \
  --manifest "$root/packaging/quest/AndroidManifest.xml" \
  --min-sdk-version "$API" --target-sdk-version "$TARGET_API" \
  --version-code "$code" --version-name "$version" \
  "$work/res.zip"
"$py" -I - "$work/unsigned.apk" "$work/lib" <<'PY'
import os, sys, zipfile
apk, lib = sys.argv[1], sys.argv[2]
with zipfile.ZipFile(apk, "a", zipfile.ZIP_DEFLATED) as z:
    for name in sorted(os.listdir(os.path.join(lib, "arm64-v8a"))):
        z.write(os.path.join(lib, "arm64-v8a", name), "lib/arm64-v8a/" + name)
PY
"$bt/zipalign$exe" -p -f 4 "$work/unsigned.apk" "$work/aligned.apk"

ks="${QUEST_KEYSTORE:-$HOME/.android/debug.keystore}"
pass="${QUEST_KEYSTORE_PASS:-android}"
alias_="${QUEST_KEY_ALIAS:-androiddebugkey}"
apk="$out/frameplayer-quest-$version.apk"
"$bt/apksigner$bat" sign --ks "$ks" --ks-pass "pass:$pass" --ks-key-alias "$alias_" \
  --out "$apk" "$work/aligned.apk"
"$bt/apksigner$bat" verify "$apk"
ls -la "$apk"
