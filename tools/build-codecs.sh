#!/usr/bin/env bash
# Build FramePlayer's bundled software-decode libraries from pinned sources.
#
#   tools/build-codecs.sh [--target T]... [--jobs N] [--clean] [--offline] [--check]
#
#   T: aarch64-unknown-linux-gnu (default) | x86_64-unknown-linux-gnu | host | all
#
# What it builds (per target, into third_party/out/<target>/):
#   * dav1d 1.5.1 (BSD-2): static, PIC, asm on (NEON on aarch64, AVX2/AVX-512
#     via nasm on x86_64). Linked into libavcodec.so (libdav1d decoder) and
#     statically into frameplayer itself (the `dav1d` crate).
#   * FFmpeg 7.1.1, LGPL-2.1 only (no --enable-gpl/--enable-nonfree/
#     --enable-version3; the script aborts if config.h says otherwise):
#     SHARED libavutil/libavcodec/libavformat/libswscale/libswresample with
#     only the decoders, parsers and demuxers fp-video uses. Shipped as .so
#     next to the binary (RUNPATH $ORIGIN/../lib) so users can swap in their
#     own build (LGPL relinking).
#
# glibc floor (aarch64): everything is compiled and linked against a glibc
# 2.31 sysroot (Ubuntu 20.04 arm64 libc6/libc6-dev/libgcc-s1 .debs, pinned by
# sha256) through a gcc wrapper, so neither the libraries nor frameplayer
# reference symbols newer than GLIBC_2.31 (= Steam Linux Runtime 3 baseline;
# SteamOS 3.x ships newer). Override with FP_SYSROOT=/path (or FP_SYSROOT=none
# to use the toolchain's own glibc).
#
# Outputs per target:
#   include/ lib/ (lib/pkgconfig, pkgconfig -> lib/pkgconfig)
#   dist-lib/      exactly what a release ships in lib/: stripped, soname-named
#                  regular files (libavcodec.so.61, …), RUNPATH $ORIGIN
#   share/licenses/ COPYING files;  BUILDINFO  versions + configure lines
#   bin/cc         (aarch64) compiler/linker wrapper honouring the sysroot
#   env.sh         `source` it before cargo: pkg-config, bindgen, linker, rpath
#
# Then:
#   source third_party/out/aarch64-unknown-linux-gnu/env.sh
#   cargo build -p fp-app --release --target aarch64-unknown-linux-gnu --features fp-app/sw-decode
#
# Host tools: curl tar xz sha256sum make pkg-config meson ninja patchelf
# (+ nasm for x86_64, + aarch64-linux-gnu-gcc/binutils and dpkg-deb for the
# aarch64 cross build). Debian/Ubuntu:
#   apt-get install -y curl xz-utils make pkg-config meson ninja-build nasm \
#       patchelf gcc-aarch64-linux-gnu binutils-aarch64-linux-gnu
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
TP=$ROOT/third_party
DL=$TP/downloads

# ---- pinned sources ------------------------------------------------------
# Mirrors are tried in order; tarballs must match the sha256. The git
# fallback checks out the tag and verifies the commit id instead.
DAV1D_VERSION=1.5.1
DAV1D_SHA256=401813f1f89fa8fd4295805aa5284d9aed9bc7fc1fdbe554af4292f64cbabe21
DAV1D_FILE=dav1d-$DAV1D_VERSION.tar.xz
DAV1D_URLS=(
  "http://archive.ubuntu.com/ubuntu/pool/universe/d/dav1d/dav1d_${DAV1D_VERSION}.orig.tar.xz"
  "https://downloads.videolan.org/pub/videolan/dav1d/${DAV1D_VERSION}/dav1d-${DAV1D_VERSION}.tar.xz"
)
DAV1D_GIT=(https://code.videolan.org/videolan/dav1d.git https://github.com/videolan/dav1d.git)
DAV1D_COMMIT=42b2b24fb8819f1ed3643aa9cf2a62f03868e3aa

FFMPEG_VERSION=7.1.1
FFMPEG_SHA256=733984395e0dbbe5c046abda2dc49a5544e7e0e1e2366bba849222ae9e3a03b1
FFMPEG_FILE=ffmpeg-$FFMPEG_VERSION.tar.xz
FFMPEG_URLS=(
  "https://ffmpeg.org/releases/ffmpeg-${FFMPEG_VERSION}.tar.xz"
  "http://archive.ubuntu.com/ubuntu/pool/universe/f/ffmpeg/ffmpeg_${FFMPEG_VERSION}.orig.tar.xz"
)
FFMPEG_GIT=(https://github.com/FFmpeg/FFmpeg.git https://git.ffmpeg.org/ffmpeg.git)
FFMPEG_COMMIT=db69d06eeeab4f46da15030a80d539efb4503ca8

# glibc 2.31 arm64 sysroot (Ubuntu 20.04 "focal" packages).
SYSROOT_GLIBC=2.31
SYSROOT_BASE=http://ports.ubuntu.com/ubuntu-ports/pool/main
SYSROOT_DEBS=(
  "g/glibc/libc6_2.31-0ubuntu9.18_arm64.deb 4ff60d84ad78f3aa598297dc6966549fa6489f57599f197562ad91547f388da0"
  "g/glibc/libc6-dev_2.31-0ubuntu9.18_arm64.deb a690787a4ed5ad1bb9ce6aebe0bef3300e483e7ed97d11a6af3cdb107e642292"
  "l/linux/linux-libc-dev_5.4.0-218.238_arm64.deb ad74b4362617d21d9c685954cf700a2d522596d03e1675f196a451d8b18ef46f"
  "g/gcc-10/libgcc-s1_10.5.0-1ubuntu1~20.04_arm64.deb 9638c0a04a540175e34314f51bc3c5709859c6f7690cb16f2deccbf8f5f1ecc6"
)

# ---- FFmpeg component selection (keep in sync with crates/video) ---------
# Video: everything fp-video's software path maps in decode/ffmpeg.rs
# codec_id(); AV1 goes through libdav1d (FFmpeg's native av1 decoder is
# hwaccel-only). Audio: what audio_decode.rs sends to libavcodec. Subtitles
# are parsed in Rust from raw packets, so no subtitle decoders. Demuxers:
# only containers the pure-Rust demuxers don't handle (demux/mod.rs sends
# "Unknown" to libavformat); I/O is a custom AVIO context, so no protocols.
FF_DECODERS=h264,hevc,vp8,vp9,libdav1d,aac,aac_latm,ac3,eac3,opus,vorbis,flac,mp3,mp3float,mp2,mp2float
FF_DECODERS+=,pcm_s16le,pcm_s16be,pcm_s24le,pcm_s24be,pcm_s32le,pcm_f32le,pcm_f64le,pcm_u8,pcm_s8,pcm_bluray,pcm_dvd
FF_PARSERS=h264,hevc,vp8,vp9,av1,aac,aac_latm,ac3,opus,vorbis,flac,mpegaudio,dvd_nav
FF_DEMUXERS=mpegts,mpegps,avi,flv,ogg

usage() { sed -n '2,45p' "$0"; }
die() { echo "build-codecs.sh: $*" >&2; exit 1; }
log() { echo "==> $*"; }

TARGETS=()
JOBS=$(nproc 2>/dev/null || echo 4)
CLEAN=0
OFFLINE=0
CHECK_ONLY=0
while [ $# -gt 0 ]; do
  case "$1" in
    --target) TARGETS+=("$2"); shift 2 ;;
    --jobs|-j) JOBS=$2; shift 2 ;;
    --clean) CLEAN=1; shift ;;
    --offline) OFFLINE=1; shift ;;
    --check) CHECK_ONLY=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown option $1 (see --help)" ;;
  esac
done
[ ${#TARGETS[@]} -gt 0 ] || TARGETS=(aarch64-unknown-linux-gnu)
HOST_ARCH=$(uname -m)
expanded=()
for t in "${TARGETS[@]}"; do
  case "$t" in
    all) expanded+=(aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu) ;;
    host) expanded+=("$HOST_ARCH-unknown-linux-gnu") ;;
    aarch64-unknown-linux-gnu|x86_64-unknown-linux-gnu) expanded+=("$t") ;;
    *) die "unsupported target $t" ;;
  esac
done
TARGETS=("${expanded[@]}")

# ---- host tool check -----------------------------------------------------
need_tools() {
  local missing=() t
  for t in "$@"; do command -v "$t" >/dev/null 2>&1 || missing+=("$t"); done
  if [ ${#missing[@]} -gt 0 ]; then
    echo "build-codecs.sh: missing host tools: ${missing[*]}" >&2
    echo "  Debian/Ubuntu: apt-get install -y curl xz-utils make pkg-config meson ninja-build nasm patchelf \\" >&2
    echo "                 gcc-aarch64-linux-gnu binutils-aarch64-linux-gnu dpkg git" >&2
    exit 1
  fi
}
common_tools=(curl tar xz sha256sum make pkg-config meson ninja patchelf git)
for t in "${TARGETS[@]}"; do
  case "$t" in
    x86_64-*) [ "$HOST_ARCH" = x86_64 ] || die "x86_64 target only builds on an x86_64 host"
              common_tools+=(nasm cc strip) ;;
    aarch64-*) if [ "$HOST_ARCH" = aarch64 ] && ! command -v aarch64-linux-gnu-gcc >/dev/null; then
                 common_tools+=(gcc strip)
               else
                 common_tools+=(aarch64-linux-gnu-gcc aarch64-linux-gnu-ar aarch64-linux-gnu-strip \
                                aarch64-linux-gnu-nm aarch64-linux-gnu-ranlib)
               fi
               [ "${FP_SYSROOT:-}" = none ] || [ -n "${FP_SYSROOT:-}" ] || common_tools+=(dpkg-deb) ;;
  esac
done
need_tools "${common_tools[@]}"
[ "$CHECK_ONLY" = 1 ] && { echo "all host tools present"; exit 0; }

# ---- downloads -----------------------------------------------------------
sha_ok() { [ -f "$1" ] && [ "$(sha256sum "$1" | cut -d' ' -f1)" = "$2" ]; }

# fetch FILE SHA URL... -> $DL/FILE (verified); returns 1 if no mirror worked
fetch() {
  local file=$1 sha=$2; shift 2
  mkdir -p "$DL"
  if sha_ok "$DL/$file" "$sha"; then return 0; fi
  [ "$OFFLINE" = 1 ] && { echo "    $file not cached (offline)" >&2; return 1; }
  local u
  for u in "$@"; do
    echo "    fetching $u" >&2
    if curl -fsSL --retry 2 --connect-timeout 20 --max-time 900 -o "$DL/$file.part" "$u"; then
      if sha_ok "$DL/$file.part" "$sha"; then
        mv "$DL/$file.part" "$DL/$file"
        echo "    ok: $u" >&2
        echo "$u" > "$DL/$file.source"
        return 0
      fi
      echo "    sha256 mismatch from $u (got $(sha256sum "$DL/$file.part" | cut -d' ' -f1))" >&2
    else
      echo "    unreachable: $u" >&2
    fi
    rm -f "$DL/$file.part"
  done
  return 1
}

# unpack_source NAME FILE SHA COMMIT DEST "URLS..." "GITS..."
# Extracts the verified tarball into DEST, or falls back to a git checkout
# of the pinned commit.
unpack_source() {
  local name=$1 file=$2 sha=$3 commit=$4 dest=$5 urls=$6 gits=$7
  rm -rf "$dest"; mkdir -p "$dest"
  # shellcheck disable=SC2086
  if fetch "$file" "$sha" $urls; then
    tar -xf "$DL/$file" -C "$dest" --strip-components=1
    echo "$(cat "$DL/$file.source" 2>/dev/null || echo cache) sha256:$sha"
    return 0
  fi
  [ "$OFFLINE" = 1 ] && die "$name: no cached tarball and --offline"
  local g
  for g in $gits; do
    echo "    $name: trying git $g" >&2
    rm -rf "$dest"
    if git -c advice.detachedHead=false clone -q --depth 1 --branch "$(git_tag "$name")" "$g" "$dest" 2>/dev/null; then
      local head
      head=$(git -C "$dest" rev-parse HEAD)
      if [ "$head" = "$commit" ]; then
        rm -rf "$dest/.git"
        echo "$g@$commit"
        return 0
      fi
      echo "    $name: $g tag resolves to $head, expected $commit" >&2
    fi
  done
  die "$name: every mirror failed (see above)"
}
git_tag() { case "$1" in dav1d) echo "$DAV1D_VERSION" ;; ffmpeg) echo "n$FFMPEG_VERSION" ;; esac; }

# ---- glibc sysroot (aarch64) ---------------------------------------------
make_sysroot() {
  local sr=$1 entry path sha file
  if [ -f "$sr/.complete" ]; then return 0; fi
  log "glibc $SYSROOT_GLIBC arm64 sysroot -> $sr"
  rm -rf "$sr"; mkdir -p "$sr"
  for entry in "${SYSROOT_DEBS[@]}"; do
    path=${entry% *}; sha=${entry#* }; file=$(basename "$path")
    fetch "$file" "$sha" "$SYSROOT_BASE/$path" || die "cannot fetch $file"
    dpkg-deb -x "$DL/$file" "$sr"
  done
  # Absolute symlinks (libpthread.so -> /lib/...) must resolve inside the sysroot.
  local l t
  while IFS= read -r l; do
    t=$(readlink "$l")
    ln -sfn "$(realpath -m --relative-to="$(dirname "$l")" "$sr$t")" "$l"
  done < <(find "$sr" -type l -lname '/*')
  touch "$sr/.complete"
}

# Write a gcc wrapper that compiles and links against the sysroot only.
# The Ubuntu cross gcc searches its own tooldir (/usr/aarch64-linux-gnu,
# glibc 2.39) before --sysroot, so headers, crt files and libraries are
# forced explicitly.
write_cc_wrapper() {
  local out=$1 gcc=$2 sr=$3 gccinc
  gccinc=$("$gcc" -print-file-name=include)
  mkdir -p "$(dirname "$out")"
  cat > "$out" <<EOF
#!/bin/sh
# Generated by tools/build-codecs.sh: $gcc against the glibc $SYSROOT_GLIBC sysroot.
exec $gcc --sysroot=$sr -nostdinc -isystem $gccinc \\
  -isystem $sr/usr/include/aarch64-linux-gnu -isystem $sr/usr/include \\
  -B$sr/usr/lib/aarch64-linux-gnu/ -L$sr/usr/lib/aarch64-linux-gnu -L$sr/lib/aarch64-linux-gnu "\$@"
EOF
  chmod +x "$out"
}

# ---- per-target build ----------------------------------------------------
build_target() {
  local target=$1 arch=${1%%-*}
  local out=$TP/out/$target bld=$TP/build/$target
  local cc cross_prefix="" strip ar nm ranlib sysroot="" cpu_flags
  log "target $target"
  [ "$CLEAN" = 1 ] && rm -rf "$bld" "$out"
  rm -rf "$out"
  mkdir -p "$bld" "$out/lib/pkgconfig" "$out/include" "$out/share/licenses" "$out/bin"
  ln -sfn lib/pkgconfig "$out/pkgconfig"

  if [ "$arch" = aarch64 ]; then
    if command -v aarch64-linux-gnu-gcc >/dev/null; then
      cross_prefix=aarch64-linux-gnu-
    fi
    local gcc=${cross_prefix}gcc
    strip=${cross_prefix}strip; ar=${cross_prefix}ar; nm=${cross_prefix}nm; ranlib=${cross_prefix}ranlib
    case "${FP_SYSROOT:-}" in
      none) cc=$gcc ;;
      "") sysroot=$TP/sysroot/aarch64-glibc$SYSROOT_GLIBC
          make_sysroot "$sysroot"
          write_cc_wrapper "$out/bin/cc" "$gcc" "$sysroot"
          cc=$out/bin/cc ;;
      *) sysroot=$FP_SYSROOT
         write_cc_wrapper "$out/bin/cc" "$gcc" "$sysroot"
         cc=$out/bin/cc ;;
    esac
    # Same baseline the Rust code is built for (.cargo/config.toml:
    # +fp16,+dotprod,+rcpc); every Snapdragon 8 Gen 3 core has these.
    cpu_flags="-march=armv8.2-a+fp16+dotprod+rcpc"
  else
    cc="cc"; strip="strip"; ar="ar"; nm="nm"; ranlib="ranlib"
    cpu_flags=""
  fi

  # dav1d -------------------------------------------------------------------
  log "dav1d $DAV1D_VERSION ($target)"
  local dav1d_src
  dav1d_src=$(unpack_source dav1d "$DAV1D_FILE" "$DAV1D_SHA256" "$DAV1D_COMMIT" "$bld/dav1d-src" \
               "${DAV1D_URLS[*]}" "${DAV1D_GIT[*]}")
  local meson_cross=()
  if [ "$arch" = aarch64 ] && [ -n "$cross_prefix$sysroot" ]; then
    cat > "$bld/meson-cross.ini" <<EOF
[binaries]
c = '$cc'
ar = '$(command -v "$ar")'
strip = '$(command -v "$strip")'
nm = '$(command -v "$nm")'
pkg-config = 'pkg-config'

[built-in options]
c_args = [$(printf "'%s'," $cpu_flags)]

[host_machine]
system = 'linux'
cpu_family = 'aarch64'
cpu = 'armv8.2-a'
endian = 'little'
EOF
    meson_cross=(--cross-file "$bld/meson-cross.ini")
  fi
  CC=$cc meson setup "$bld/dav1d-build" "$bld/dav1d-src" "${meson_cross[@]}" \
    --prefix="$out" --libdir=lib --buildtype=release --default-library=static \
    -Db_staticpic=true -Denable_asm=true -Denable_tools=false -Denable_tests=false \
    -Denable_examples=false -Dbitdepths=8,16 -Dlogging=false >"$bld/dav1d-meson.log" 2>&1 \
    || { tail -40 "$bld/dav1d-meson.log"; die "dav1d meson setup failed"; }
  grep -E "Host machine cpu family|asm" "$bld/dav1d-meson.log" | sed 's/^/    /' || true
  ninja -C "$bld/dav1d-build" -j "$JOBS" install >"$bld/dav1d-ninja.log" 2>&1 \
    || { tail -40 "$bld/dav1d-ninja.log"; die "dav1d build failed"; }
  cp "$bld/dav1d-src/COPYING" "$out/share/licenses/dav1d-COPYING"

  # FFmpeg ------------------------------------------------------------------
  log "FFmpeg $FFMPEG_VERSION ($target)"
  local ff_src
  ff_src=$(unpack_source ffmpeg "$FFMPEG_FILE" "$FFMPEG_SHA256" "$FFMPEG_COMMIT" "$bld/ffmpeg-src" \
             "${FFMPEG_URLS[*]}" "${FFMPEG_GIT[*]}")
  local ff_cross=()
  if [ "$arch" = aarch64 ]; then
    ff_cross=(--arch=aarch64 --target-os=linux --enable-neon)
    [ -n "$cross_prefix" ] && ff_cross+=(--enable-cross-compile --cross-prefix="$cross_prefix")
    ff_cross+=(--cc="$cc" --ar="$ar" --nm="$nm" --ranlib="$ranlib" --strip="$strip")
  else
    ff_cross=(--enable-x86asm --x86asmexe=nasm)
  fi
  local ff_flags=(
    --prefix="$out" --libdir="$out/lib" --shlibdir="$out/lib" --incdir="$out/include"
    --enable-shared --disable-static --enable-pic
    --disable-autodetect --disable-everything
    --disable-programs --disable-doc --disable-debug
    --disable-avdevice --disable-avfilter --disable-postproc --disable-network
    --enable-avcodec --enable-avformat --enable-avutil --enable-swscale --enable-swresample
    --enable-pthreads --enable-runtime-cpudetect
    --enable-libdav1d
    --enable-decoder="$FF_DECODERS"
    --enable-parser="$FF_PARSERS"
    --enable-demuxer="$FF_DEMUXERS"
    --pkg-config=pkg-config --pkg-config-flags=--static
    --extra-cflags="-O2 $cpu_flags"
    --extra-ldflags="-Wl,-z,relro -Wl,-z,now"
    "${ff_cross[@]}"
  )
  mkdir -p "$bld/ffmpeg-build"
  ( cd "$bld/ffmpeg-build" &&
    PKG_CONFIG_PATH="$out/lib/pkgconfig" PKG_CONFIG_LIBDIR="$out/lib/pkgconfig" \
      "$bld/ffmpeg-src/configure" "${ff_flags[@]}" >"$bld/ffmpeg-configure.log" 2>&1 ) \
    || { tail -40 "$bld/ffmpeg-configure.log"; tail -30 "$bld/ffmpeg-build/ffbuild/config.log" 2>/dev/null; die "FFmpeg configure failed"; }
  # Licence gate (ADR 0002): LGPL-2.1 only.
  local h=$bld/ffmpeg-build/config.h
  grep -q '^#define CONFIG_GPL 0' "$h" && grep -q '^#define CONFIG_NONFREE 0' "$h" \
    && grep -q '^#define CONFIG_VERSION3 0' "$h" \
    || die "FFmpeg configured with GPL/nonfree/version3 components; refusing (ADR 0002)"
  grep -q '^#define CONFIG_LIBDAV1D_DECODER 1' "$bld/ffmpeg-build/config_components.h" || die "libdav1d decoder not enabled (dav1d not found by configure?)"
  ( cd "$bld/ffmpeg-build" && make -j "$JOBS" >"$bld/ffmpeg-make.log" 2>&1 && make install >>"$bld/ffmpeg-make.log" 2>&1 ) \
    || { tail -40 "$bld/ffmpeg-make.log"; die "FFmpeg build failed"; }
  cp "$bld/ffmpeg-src/COPYING.LGPLv2.1" "$out/share/licenses/ffmpeg-COPYING.LGPLv2.1"
  cp "$bld/ffmpeg-src/LICENSE.md" "$out/share/licenses/ffmpeg-LICENSE.md"

  # Shipping set: stripped, soname-named files with RUNPATH $ORIGIN (so the
  # libraries find each other even when loaded via dlopen or LD_PRELOAD).
  rm -rf "$out/dist-lib"; mkdir -p "$out/dist-lib"
  local so soname
  for so in "$out"/lib/lib*.so; do
    soname=$(readelf -d "$so" | sed -n 's/.*(SONAME).*\[\(.*\)\]/\1/p')
    [ -n "$soname" ] || die "$so has no SONAME"
    # The soname symlink in lib/ points at the versioned file; ship a real file.
    install -m 0644 "$(readlink -f "$so")" "$out/dist-lib/$soname"
    "$strip" --strip-unneeded "$out/dist-lib/$soname"
    patchelf --set-rpath '$ORIGIN' "$out/dist-lib/$soname"
    # Development tree: absolute RUNPATH so host tests run without LD_LIBRARY_PATH.
    patchelf --set-rpath "$out/lib" "$(readlink -f "$so")"
  done

  # BUILDINFO (LGPL: exact versions, sources and configure lines).
  {
    echo "target: $target"
    echo "glibc sysroot: ${sysroot:-toolchain default}"
    echo "dav1d $DAV1D_VERSION ($dav1d_src) BSD-2-Clause, static"
    echo "ffmpeg $FFMPEG_VERSION ($ff_src) LGPL-2.1-or-later, shared"
    echo "ffmpeg configure: ${ff_flags[*]}"
    echo "built: $(date -u +%Y-%m-%dT%H:%M:%SZ) by tools/build-codecs.sh"
  } > "$out/BUILDINFO"

  write_env "$target" "$out" "$sysroot" "$cc"
  log "done: $out"
  ls -l "$out/dist-lib" | sed 's/^/    /'
  echo "    total: $(du -cb "$out"/dist-lib/* | tail -n1 | cut -f1) bytes"
}

write_env() {
  local target=$1 out=$2 sysroot=$3 cc=$4
  local t_=${target//-/_} T
  T=$(echo "$t_" | tr '[:lower:]' '[:upper:]')
  {
    echo "# Generated by tools/build-codecs.sh for $target. Source before cargo."
    echo "# Variables are target-suffixed so sourcing several env.sh files is fine."
    echo "export FP_CODECS_${T}=$out"
    echo "export PKG_CONFIG_PATH_${t_}=$out/lib/pkgconfig"
    echo "export PKG_CONFIG_ALLOW_CROSS_${t_}=1"
    # ffmpeg-sys-next falls back to pkg-config when FFMPEG_DIR is unset (and
    # FFMPEG_DIR is not target-specific, so leave it alone).
    echo "unset FFMPEG_DIR"
    # dav1d crate: link libdav1d.a into the binary (no extra .so to ship).
    echo "export SYSTEM_DEPS_DAV1D_LINK=static"
    if [ "${target%%-*}" = aarch64 ]; then
      if [ -n "$sysroot" ]; then
        echo "export BINDGEN_EXTRA_CLANG_ARGS_${t_}=\"--sysroot=$sysroot\""
        echo "export CARGO_TARGET_${T}_LINKER=$cc"
        echo "export CC_${t_}=$cc"
      fi
      # Release layout: bin/frameplayer + lib/*.so.  Merged with the
      # rustflags in .cargo/config.toml (env + config arrays are joined).
      echo "export CARGO_TARGET_${T}_RUSTFLAGS=\"-C link-arg=-Wl,-rpath,\\\$ORIGIN/../lib\""
    else
      echo "export CARGO_TARGET_${T}_RUSTFLAGS=\"-C link-arg=-Wl,-rpath,$out/lib\""
    fi
  } > "$out/env.sh"
  echo "    env: source $out/env.sh"
  sed 's/^/      /' "$out/env.sh"
}

for t in "${TARGETS[@]}"; do build_target "$t"; done
