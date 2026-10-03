#!/usr/bin/env bash
# Build a FramePlayer release: aarch64 tarball, SHA-256, delta patches,
# signed update manifests and the website install manifest.
#
#   tools/release.sh [--version X.Y.Z] [--channel stable|beta]
#                    [--prev-tarball OLD.tar.gz]... [--bin PATH] [--probe-bin PATH] [--no-build]
#                    [--out DIR] [--download-base URL] [--min-steamos V]
#                    [--notes FILE] [--require-trusted]
#
# Signing uses $FP_SIGNING_KEY (hex) or --key-file; without a key the
# manifests are written unsigned and a warning is printed.
#
# Output ($OUT, default dist/out/<version>):
#   frameplayer-<v>-aarch64.tar.gz (+ .sha256)
#     frameplayer.sh, frameplayer-probe.sh, RELEASE,
#     versions/<v>/bin/{frameplayer,frameplayer-probe}, lib/, share/
#   frameplayer-<v>-from-<old>.fpd       delta patches
#   site/                                 GitHub Pages tree:
#     index.html frameplayer.json frameplayer.schema.json art/*.png
#     updates/<channel>.json(.sig)        (stable releases also write beta.json)
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

TARGET=aarch64-unknown-linux-gnu
VERSION=""
CHANNEL=stable
PREV=()
BIN=""
PROBE_BIN=""
BUILD=1
OUT=""
REPO_URL=${REPO_URL:-https://github.com/yellkell/frameplayer}
DOWNLOAD_BASE=""
MIN_STEAMOS=""
NOTES=""
KEY_FILE=""
REQUIRE_TRUSTED=""

die() { echo "release.sh: $*" >&2; exit 1; }
while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION=$2; shift 2 ;;
    --channel) CHANNEL=$2; shift 2 ;;
    --prev-tarball) PREV+=("$2"); shift 2 ;;
    --bin) BIN=$2; BUILD=0; shift 2 ;;
    --probe-bin) PROBE_BIN=$2; shift 2 ;;
    --no-build) BUILD=0; shift ;;
    --out) OUT=$2; shift 2 ;;
    --download-base) DOWNLOAD_BASE=$2; shift 2 ;;
    --min-steamos) MIN_STEAMOS=$2; shift 2 ;;
    --notes) NOTES=$2; shift 2 ;;
    --key-file) KEY_FILE=$2; shift 2 ;;
    --require-trusted) REQUIRE_TRUSTED=--require-trusted; shift ;;
    -h|--help) sed -n '2,22p' "$0"; exit 0 ;;
    *) die "unknown option $1" ;;
  esac
done

if [ -z "$VERSION" ]; then
  VERSION=$(sed -n '/^\[workspace.package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p}' Cargo.toml | head -n1)
fi
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || die "bad version $VERSION"
case "$CHANNEL" in stable|beta) ;; *) die "channel must be stable or beta" ;; esac
if [ "$CHANNEL" = stable ] && [[ "$VERSION" == *-* ]]; then die "pre-release $VERSION cannot go to stable"; fi
OUT=${OUT:-dist/out/$VERSION}
DOWNLOAD_BASE=${DOWNLOAD_BASE:-$REPO_URL/releases/download/v$VERSION}
NAME=frameplayer-$VERSION-aarch64
TARBALL=$OUT/$NAME.tar.gz
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct 2>/dev/null || date +%s)}

echo "==> FramePlayer $VERSION ($CHANNEL) -> $OUT"
rm -rf "$OUT"
mkdir -p "$OUT"

# Host-side release tool.
cargo build --release -q -p fp-installer
FPI=$ROOT/target/release/frameplayer-install
[ -n "${CARGO_TARGET_DIR:-}" ] && FPI=$CARGO_TARGET_DIR/release/frameplayer-install

# The self-test (crates/probe, binary frameplayer-probe) ships in every
# tarball once the crate exists; frameplayer-install runs it after installing.
HAVE_PROBE=0
[ -f crates/probe/Cargo.toml ] && HAVE_PROBE=1
TDIR=${CARGO_TARGET_DIR:-$ROOT/target}/$TARGET/release
if [ "$BUILD" = 1 ]; then
  pkgs=(-p fp-app)
  if [ "$HAVE_PROBE" = 1 ]; then pkgs+=(-p fp-probe); else echo "warning: crates/probe missing; tarball will not contain the self-test" >&2; fi
  echo "==> cargo build --release --target $TARGET ${pkgs[*]}"
  cargo build --release --target "$TARGET" "${pkgs[@]}"
  BIN=$TDIR/frameplayer
  [ "$HAVE_PROBE" = 1 ] && PROBE_BIN=${PROBE_BIN:-$TDIR/frameplayer-probe}
fi
BIN=${BIN:-$TDIR/frameplayer}
[ -x "$BIN" ] || die "binary $BIN not found (build first or pass --bin)"
if [ -z "$PROBE_BIN" ] && [ -x "$TDIR/frameplayer-probe" ] && [ "$BUILD" = 0 ] && [ "$BIN" = "$TDIR/frameplayer" ]; then
  PROBE_BIN=$TDIR/frameplayer-probe
fi
if [ -n "$PROBE_BIN" ] && [ ! -x "$PROBE_BIN" ]; then die "probe binary $PROBE_BIN not found"; fi
[ -n "$PROBE_BIN" ] || echo "warning: no frameplayer-probe binary; the tarball will not contain the self-test" >&2
if command -v file >/dev/null; then
  file -b "$BIN" | grep -q 'ARM aarch64' || echo "warning: $BIN is not an aarch64 ELF: $(file -b "$BIN")" >&2
fi

echo "==> staging tree"
STAGE=$OUT/stage
V=$STAGE/versions/$VERSION
mkdir -p "$V/bin" "$V/lib" "$V/share/steam" "$V/share/frameplayer"
install -m 0755 "$BIN" "$V/bin/frameplayer"
install -m 0755 dist/frameplayer.sh "$STAGE/frameplayer.sh"
install -m 0755 dist/frameplayer-probe.sh "$STAGE/frameplayer-probe.sh"
[ -n "$PROBE_BIN" ] && install -m 0755 "$PROBE_BIN" "$V/bin/frameplayer-probe"
printf '%s\n' "$VERSION" > "$STAGE/RELEASE"
# Bundled shared libraries (normally none: everything is static except glibc
# and the system Vulkan loader). Drop extras in dist/lib-aarch64/.
if [ -d dist/lib-aarch64 ]; then cp -a dist/lib-aarch64/. "$V/lib/"; fi
if [ -d assets ]; then
  (cd assets && tar --exclude=./steam -cf - .) | tar -xf - -C "$V/share/frameplayer"
fi
for art in grid grid_horizontal hero logo icon; do
  if [ -f "assets/steam/$art.png" ]; then
    cp "assets/steam/$art.png" "$V/share/steam/$art.png"
  else
    echo "warning: assets/steam/$art.png missing; the library entry will lack that artwork" >&2
  fi
done
for f in LICENSE LICENSE-MIT LICENSE-APACHE README.md; do [ -f "$f" ] && cp "$f" "$V/"; done
if command -v aarch64-linux-gnu-readelf >/dev/null || command -v readelf >/dev/null; then
  RE=$(command -v aarch64-linux-gnu-readelf || command -v readelf)
  echo "    dynamic deps: $("$RE" -d "$V/bin/frameplayer" 2>/dev/null | sed -n 's/.*Shared library: \[\(.*\)\]/\1/p' | tr '\n' ' ')"
  echo "    max GLIBC symbol: $("$RE" -V "$V/bin/frameplayer" 2>/dev/null | grep -o 'GLIBC_[0-9.]*' | sort -Vu | tail -n1)"
fi

echo "==> $TARBALL"
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$SOURCE_DATE_EPOCH" \
    --format=gnu -C "$STAGE" -cf - frameplayer.sh frameplayer-probe.sh RELEASE versions | gzip -9n > "$TARBALL"
(cd "$OUT" && sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256")
echo "    sha256 $(cut -d' ' -f1 "$TARBALL.sha256")  size $(wc -c < "$TARBALL")"

DELTA_ARGS=()
for old in ${PREV[@]+"${PREV[@]}"}; do
  ov=$(tar -xOzf "$old" RELEASE | tr -d '\r\n ')
  [ -n "$ov" ] || die "$old has no RELEASE"
  patch=$NAME-from-$ov.fpd
  "$FPI" make-delta --from "$old" --to "$TARBALL" --out "$OUT/$patch"
  DELTA_ARGS+=(--delta "$ov=$OUT/$patch=$DOWNLOAD_BASE/$patch")
done

SITE=$OUT/site
mkdir -p "$SITE/updates" "$SITE/art"
cp dist/site/index.html "$SITE/index.html"
cp dist/frameplayer.schema.json "$SITE/"
for art in grid grid_horizontal hero logo icon; do
  [ -f "assets/steam/$art.png" ] && cp "assets/steam/$art.png" "$SITE/art/"
done

channels=("$CHANNEL")
[ "$CHANNEL" = stable ] && channels+=(beta)
for ch in "${channels[@]}"; do
  m=$SITE/updates/$ch.json
  args=(make-manifest --version "$VERSION" --channel "$ch" --tarball "$TARBALL"
        --url "$DOWNLOAD_BASE/$NAME.tar.gz" --notes-url "$REPO_URL/releases/tag/v$VERSION" --out "$m")
  [ -n "$MIN_STEAMOS" ] && args+=(--min-steamos "$MIN_STEAMOS")
  [ -n "$NOTES" ] && args+=(--notes-file "$NOTES")
  "$FPI" "${args[@]}" ${DELTA_ARGS[@]+"${DELTA_ARGS[@]}"}
  if [ -n "$KEY_FILE" ]; then
    "$FPI" sign-manifest "$m" --key-file "$KEY_FILE" $REQUIRE_TRUSTED
  elif [ -n "${FP_SIGNING_KEY:-}" ]; then
    "$FPI" sign-manifest "$m" $REQUIRE_TRUSTED
  else
    echo "warning: no signing key (FP_SIGNING_KEY / --key-file); $m is UNSIGNED" >&2
  fi
done

if [ "$CHANNEL" = stable ]; then
  "$FPI" site-manifest --template dist/frameplayer.json --version "$VERSION" \
    --tarball "$TARBALL" --url "$DOWNLOAD_BASE/$NAME.tar.gz" --out "$SITE/frameplayer.json"
else
  # Beta releases don't move the website's one-click install.
  cp dist/frameplayer.json "$SITE/frameplayer.json"
fi
rm -rf "$STAGE"

echo "==> done"
find "$OUT" -maxdepth 3 -type f | sort | sed 's/^/    /'
