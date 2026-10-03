#!/usr/bin/env bash
# Builds the FramePlayer release for the Steam Frame and writes everything
# needed to publish it into dist/:
#
#   dist/frameplayer-<version>-aarch64.zip   the install archive
#   dist/manifest.json                       update manifest (sign it!)
#   dist/framedrop.json                      Frame Control / FrameDrop manifest
#
# Usage: tools/package.sh [--url-base URL] [--notes FILE] [--key PREFIX.key]
#   --url-base  where the zip will be downloadable (default: the GitHub
#               release download URL for this version)
#   --key       sign manifest.json with this fp-release key
#
# Needs: the aarch64 FFmpeg build (tools/build-ffmpeg.sh aarch64) and the
# cross toolchain from tools/build-frame.sh.
set -euo pipefail
cd "$(dirname "$0")/.."
root="$PWD"
version="$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"]=="frameplayer"))')"
url_base="https://github.com/yellkell/frameplayer/releases/download/v$version"
notes=""
key=""
while [ $# -gt 0 ]; do
  case "$1" in
    --url-base) url_base="$2"; shift 2 ;;
    --notes) notes="$2"; shift 2 ;;
    --key) key="$2"; shift 2 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done

target=aarch64-unknown-linux-gnu
ffmpeg="$root/third_party/ffmpeg/aarch64/lib"
[ -f "$ffmpeg/libavcodec.so" ] || { echo "missing $ffmpeg; run tools/build-ffmpeg.sh aarch64" >&2; exit 1; }

echo "== building FramePlayer $version for the Steam Frame"
tools/build-frame.sh -p frameplayer
cargo build --release -p fp-updater --bin fp-release

stage="$root/dist/stage/frameplayer"
rm -rf "$root/dist/stage" && mkdir -p "$stage/lib" "$stage/licenses" "$stage/assets/steam"
cp "target/$target/release/frameplayer" "$stage/"
llvm-strip --strip-all "$stage/frameplayer" 2>/dev/null || true
for l in avformat.so.61 avcodec.so.61 avutil.so.59 swresample.so.5 swscale.so.8; do
  cp -L "$ffmpeg/lib$l" "$stage/lib/lib$l"
done
cp packaging/frameplayer.sh packaging/README.txt "$stage/"
cp packaging/licenses/* "$stage/licenses/"
cp assets/steam/*.png "$stage/assets/steam/"
echo "$version" > "$stage/VERSION"
# Licences of the Rust crates compiled into the binary.
cargo metadata --format-version 1 --filter-platform "$target" | python3 -c '
import json, sys
m = json.load(sys.stdin)
pkgs = {p["id"]: p for p in m["packages"]}
nodes = {n["id"]: n for n in m["resolve"]["nodes"]}
root = next(i for i, p in pkgs.items() if p["name"] == "frameplayer")
seen, todo = set(), [root]
while todo:
    i = todo.pop()
    if i in seen: continue
    seen.add(i)
    todo += [d["pkg"] for d in nodes[i]["deps"] if any(k["kind"] in (None, "normal") for k in d["dep_kinds"])]
for p in sorted((pkgs[i] for i in seen if pkgs[i]["source"]), key=lambda p: p["name"]):
    print("%s %s: %s" % (p["name"], p["version"], p.get("license") or "see crate"))
' > "$stage/licenses/rust-crates.txt"

# Sanity: right architecture, glibc <= 2.28, libraries resolvable.
file "$stage/frameplayer" | grep -q "ARM aarch64" || { echo "not an aarch64 binary" >&2; exit 1; }
max_glibc="$(objdump -T "$stage/frameplayer" "$stage"/lib/*.so* 2>/dev/null | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1)"
echo "highest glibc symbol version: $max_glibc"
case "$max_glibc" in GLIBC_2.2[0-8]|GLIBC_2.1*|GLIBC_2.[0-9]) ;; *) echo "needs newer glibc than 2.28" >&2; exit 1 ;; esac
readelf -d "$stage/frameplayer" | grep -qE '(RPATH|RUNPATH).*\$ORIGIN/lib' || { echo "missing \$ORIGIN/lib rpath" >&2; exit 1; }

zip="frameplayer-$version-aarch64.zip"
rm -f "dist/$zip"
(cd "$root/dist/stage" && find frameplayer -print0 | sort -z | xargs -0 touch -d "@${SOURCE_DATE_EPOCH:-0}" && zip -qrX9 "$root/dist/$zip" frameplayer)
echo "== $(du -h "dist/$zip" | cut -f1) dist/$zip"

release="target/release/fp-release"
args=(manifest --version "$version" --zip "dist/$zip" --url-base "$url_base" --out dist/manifest.json)
[ -n "$notes" ] && args+=(--notes "$notes")
"$release" "${args[@]}"
"$release" framedrop --name FramePlayer --zip "dist/$zip" --url "$url_base/$zip" --out dist/framedrop.json \
  --manifest-url "$url_base/framedrop.json"
if [ -n "$key" ]; then
  "$release" sign dist/manifest.json --key "$key"
else
  echo "note: dist/manifest.json is unsigned; run: fp-release sign dist/manifest.json --key PREFIX.key"
fi
rm -rf "$root/dist/stage"
ls -la dist
