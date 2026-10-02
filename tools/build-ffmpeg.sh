#!/usr/bin/env bash
# Build the FFmpeg libraries FramePlayer bundles: LGPL, decode-only, no
# network protocols (all I/O goes through Rust), with dav1d for software AV1
# and the V4L2 memory-to-memory decoders for Qualcomm hardware decode.
#
# Usage: tools/build-ffmpeg.sh [aarch64|x86_64]...   (default: both)
# Output: third_party/ffmpeg/<arch>/{include,lib}
#
# aarch64 is cross-compiled with zig against glibc 2.28, like the app.
# x86_64 is built with the host compiler for development and tests only.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
FFMPEG_VER=7.1.1
DAV1D_VER=1.5.3
src="$root/third_party/src"
mkdir -p "$src"

fetch() { # url dest
  [ -f "$2" ] || curl -fsSL --retry 4 "$1" -o "$2.part" && mv -f "$2.part" "$2" 2>/dev/null || true
  [ -f "$2" ] || { echo "download failed: $1" >&2; exit 1; }
}
fetch "http://archive.ubuntu.com/ubuntu/pool/universe/f/ffmpeg/ffmpeg_${FFMPEG_VER}.orig.tar.xz" "$src/ffmpeg-$FFMPEG_VER.tar.xz"
fetch "http://archive.ubuntu.com/ubuntu/pool/main/d/dav1d/dav1d_${DAV1D_VER}.orig.tar.xz" "$src/dav1d-$DAV1D_VER.tar.xz"

zig_wrappers() { # dir
  mkdir -p "$1"
  printf '#!/bin/sh\nexec python3 -m ziglang cc -target aarch64-linux-gnu.2.28 "$@"\n' > "$1/cc"
  printf '#!/bin/sh\nexec python3 -m ziglang c++ -target aarch64-linux-gnu.2.28 "$@"\n' > "$1/cxx"
  printf '#!/bin/sh\nexec python3 -m ziglang ar "$@"\n' > "$1/ar"
  printf '#!/bin/sh\nexec python3 -m ziglang ranlib "$@"\n' > "$1/ranlib"
  chmod +x "$1"/*
}

build_arch() {
  local arch="$1" out="$root/third_party/ffmpeg/$1" work="$root/third_party/build/$1"
  rm -rf "$work"; mkdir -p "$work" "$out"
  tar -xf "$src/dav1d-$DAV1D_VER.tar.xz" -C "$work"
  tar -xf "$src/ffmpeg-$FFMPEG_VER.tar.xz" -C "$work"
  local dav1d_dir; dav1d_dir="$(ls -d "$work"/dav1d-*/)"
  local ff_dir; ff_dir="$(ls -d "$work"/ffmpeg-*/)"

  local cc=cc ar=ar ranlib=ranlib cross_args=() meson_cross=()
  if [ "$arch" = aarch64 ]; then
    zig_wrappers "$work/zig"
    cc="$work/zig/cc"; ar="$work/zig/ar"; ranlib="$work/zig/ranlib"
    cat > "$work/cross.ini" <<INI
[binaries]
c = '$work/zig/cc'
cpp = '$work/zig/cxx'
ar = '$work/zig/ar'
strip = 'true'
[host_machine]
system = 'linux'
cpu_family = 'aarch64'
cpu = 'aarch64'
endian = 'little'
INI
    meson_cross=(--cross-file "$work/cross.ini")
    cross_args=(--enable-cross-compile --arch=aarch64 --target-os=linux
                --cc="$cc" --cxx="$work/zig/cxx" --ar="$ar" --ranlib="$ranlib"
                --nm="python3 -m ziglang nm" --strip=true --host-cc=cc)
  fi

  echo "== dav1d ($arch)"
  local x86asm=()
  [ "$arch" = x86_64 ] && ! command -v nasm >/dev/null && x86asm=(-Denable_asm=false)
  meson setup "$work/dav1d-build" "$dav1d_dir" "${meson_cross[@]}" \
    --prefix="$work/dav1d-prefix" --libdir=lib --buildtype=release \
    --default-library=static -Db_staticpic=true \
    -Denable_tools=false -Denable_tests=false -Denable_examples=false "${x86asm[@]}" >/dev/null
  ninja -C "$work/dav1d-build" install >/dev/null

  echo "== ffmpeg ($arch)"
  local asm=()
  [ "$arch" = x86_64 ] && ! command -v nasm >/dev/null && asm=(--disable-x86asm)
  (cd "$ff_dir" && PKG_CONFIG_LIBDIR="$work/dav1d-prefix/lib/pkgconfig" ./configure \
    --prefix="$out" "${cross_args[@]}" "${asm[@]}" \
    --pkg-config=pkg-config --pkg-config-flags=--static \
    --enable-shared --disable-static --enable-pic \
    --disable-programs --disable-doc --disable-debug \
    --disable-autodetect --disable-network --disable-avdevice --disable-avfilter --disable-postproc \
    --disable-everything \
    --enable-libdav1d --enable-v4l2-m2m \
    --enable-swscale --enable-swresample \
    --enable-protocol=file \
    --enable-demuxer=mov,matroska,mpegts,avi,flv,ogg,mp3,aac,wav,flac,srt,ass,webvtt,hls,image2,mjpeg \
    --enable-decoder=h264,hevc,vp8,vp9,av1,libdav1d,mpeg4,mpeg2video,prores,mjpeg,png \
    --enable-decoder=h264_v4l2m2m,hevc_v4l2m2m,vp8_v4l2m2m,vp9_v4l2m2m,mpeg4_v4l2m2m \
    --enable-decoder=aac,aac_latm,ac3,eac3,mp3,mp3float,mp2,opus,vorbis,flac,alac,dca,truehd \
    --enable-decoder=pcm_s16le,pcm_s24le,pcm_s32le,pcm_f32le,pcm_s16be,pcm_s24be \
    --enable-decoder=srt,subrip,ass,ssa,webvtt,mov_text,pgssub,dvdsub,dvbsub \
    --enable-parser=h264,hevc,vp8,vp9,av1,aac,aac_latm,ac3,mpegaudio,opus,vorbis,flac,mpeg4video,mpegvideo,dca,mjpeg,png \
    --enable-bsf=h264_mp4toannexb,hevc_mp4toannexb,vp9_superframe_split,extract_extradata,null \
    --extra-ldflags="-Wl,-rpath,\\\$\$ORIGIN" \
    > "$work/configure.log" 2>&1) || { tail -30 "$work/configure.log"; tail -40 "$ff_dir/ffbuild/config.log"; exit 1; }
  # zig's glibc headers have no <sys/sysctl.h> (removed from glibc 2.32), but
  # configure's link test still finds the symbol; FFmpeg never needs it on Linux.
  sed -i 's/^#define HAVE_SYSCTL 1$/#define HAVE_SYSCTL 0/' "$ff_dir/config.h"
  make -C "$ff_dir" -j"$(nproc)" >"$work/make.log" 2>&1 || { tail -40 "$work/make.log"; exit 1; }
  make -C "$ff_dir" install >/dev/null
  rm -rf "$out/share" "$out/lib/pkgconfig"
  grep -E "^(libdav1d|v4l2_m2m|hevc_v4l2m2m_decoder)" "$ff_dir/ffbuild/config.mak" | head -5 || true
  ls -la "$out/lib"
}

archs=("$@"); [ ${#archs[@]} -eq 0 ] && archs=(x86_64 aarch64)
for a in "${archs[@]}"; do build_arch "$a"; done
echo "FFmpeg $FFMPEG_VER ready in third_party/ffmpeg/"
