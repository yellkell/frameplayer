#!/usr/bin/env bash
# Build the FFmpeg libraries for FramePlayer on Meta Quest (Android arm64):
# the same LGPL, decode-only set as tools/build-ffmpeg.sh, with Android's
# MediaCodec hardware decoders in place of V4L2. With no Java VM handed to
# FFmpeg they go through the NDK's AMediaCodec, so no Java code is needed.
#
# Usage: ANDROID_NDK_HOME=<NDK r27+> tools/build-ffmpeg-android.sh
# Output: third_party/ffmpeg/android-aarch64/{include,lib}
#
# Runs on Linux (CI); kept apart from tools/build-ffmpeg.sh so the Frame's
# FFmpeg build and its CI cache stay as they are.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
FFMPEG_VER=7.1.1
DAV1D_VER=1.5.3
API=29 # Android 10, the oldest Quest system FramePlayer supports
ndk="${ANDROID_NDK_HOME:?set ANDROID_NDK_HOME to the Android NDK}"
tc="$ndk/toolchains/llvm/prebuilt/linux-x86_64/bin"
src="$root/third_party/src"
out="$root/third_party/ffmpeg/android-aarch64"
work="$root/third_party/build/android-aarch64"
mkdir -p "$src"

fetch() { # url dest
  [ -f "$2" ] || curl -fsSL --retry 4 "$1" -o "$2.part" && mv -f "$2.part" "$2" 2>/dev/null || true
  [ -f "$2" ] || { echo "download failed: $1" >&2; exit 1; }
}
fetch "http://archive.ubuntu.com/ubuntu/pool/universe/f/ffmpeg/ffmpeg_${FFMPEG_VER}.orig.tar.xz" "$src/ffmpeg-$FFMPEG_VER.tar.xz"
fetch "http://archive.ubuntu.com/ubuntu/pool/main/d/dav1d/dav1d_${DAV1D_VER}.orig.tar.xz" "$src/dav1d-$DAV1D_VER.tar.xz"

rm -rf "$work"; mkdir -p "$work" "$out"
tar -xf "$src/dav1d-$DAV1D_VER.tar.xz" -C "$work"
tar -xf "$src/ffmpeg-$FFMPEG_VER.tar.xz" -C "$work"
dav1d_dir="$(ls -d "$work"/dav1d-*/)"
ff_dir="$(ls -d "$work"/ffmpeg-*/)"
# MediaCodec's 10-bit output (P010) in FFmpeg's copy-out mode, which knows
# only the 8-bit formats: without it 10-bit HEVC fell back to software.
patch -p1 -d "$ff_dir" < "$root/tools/ffmpeg-android-p010.patch"
cc="$tc/aarch64-linux-android$API-clang"
cxx="$tc/aarch64-linux-android$API-clang++"

cat > "$work/cross.ini" <<INI
[binaries]
c = '$cc'
cpp = '$cxx'
ar = '$tc/llvm-ar'
strip = '$tc/llvm-strip'
[host_machine]
system = 'android'
cpu_family = 'aarch64'
cpu = 'aarch64'
endian = 'little'
INI

echo "== dav1d (android)"
meson setup "$work/dav1d-build" "$dav1d_dir" --cross-file "$work/cross.ini" \
  --prefix="$work/dav1d-prefix" --libdir=lib --buildtype=release \
  --default-library=static -Db_staticpic=true \
  -Denable_tools=false -Denable_tests=false -Denable_examples=false \
  >"$work/dav1d-setup.log" 2>&1 || { tail -40 "$work/dav1d-setup.log"; exit 1; }
ninja -C "$work/dav1d-build" install >"$work/dav1d-build.log" 2>&1 || { tail -40 "$work/dav1d-build.log"; exit 1; }

echo "== ffmpeg (android)"
(cd "$ff_dir" && PKG_CONFIG_LIBDIR="$work/dav1d-prefix/lib/pkgconfig" ./configure \
  --prefix="$out" \
  --enable-cross-compile --target-os=android --arch=aarch64 --cpu=armv8-a \
  --cc="$cc" --cxx="$cxx" --ar="$tc/llvm-ar" --ranlib="$tc/llvm-ranlib" \
  --nm="$tc/llvm-nm" --strip="$tc/llvm-strip" --host-cc=cc \
  --pkg-config=pkg-config --pkg-config-flags=--static \
  --enable-shared --disable-static --enable-pic \
  --disable-programs --disable-doc --disable-debug \
  --disable-autodetect --disable-network --disable-avdevice --disable-avfilter --disable-postproc \
  --disable-everything \
  --enable-libdav1d --enable-jni --enable-mediacodec \
  --enable-swscale --enable-swresample \
  --enable-protocol=file \
  --enable-demuxer=mov,matroska,mpegts,avi,flv,ogg,mp3,aac,wav,flac,srt,ass,webvtt,hls,image2,mjpeg \
  --enable-decoder=h264,hevc,vp8,vp9,av1,libdav1d,mpeg4,mpeg2video,vc1,wmv3,prores,mjpeg,png \
  --enable-decoder=h264_mediacodec,hevc_mediacodec,av1_mediacodec,vp8_mediacodec,vp9_mediacodec,mpeg4_mediacodec \
  --enable-decoder=aac,aac_latm,ac3,eac3,mp3,mp3float,mp2,opus,vorbis,flac,alac,dca,truehd \
  --enable-decoder=pcm_s16le,pcm_s24le,pcm_s32le,pcm_f32le,pcm_s16be,pcm_s24be,pcm_bluray,pcm_dvd \
  --enable-decoder=srt,subrip,ass,ssa,webvtt,mov_text,pgssub,dvdsub,dvbsub \
  --enable-parser=h264,hevc,vp8,vp9,av1,aac,aac_latm,ac3,mpegaudio,opus,vorbis,flac,mpeg4video,mpegvideo,vc1,dca,mlp,mjpeg,png \
  --enable-bsf=h264_mp4toannexb,hevc_mp4toannexb,vp9_superframe_split,extract_extradata,null \
  > "$work/configure.log" 2>&1) || { tail -30 "$work/configure.log"; tail -40 "$ff_dir/ffbuild/config.log"; exit 1; }
make -C "$ff_dir" -j"$(nproc)" >"$work/make.log" 2>&1 || { tail -40 "$work/make.log"; exit 1; }
make -C "$ff_dir" install >/dev/null
rm -rf "$out/share" "$out/lib/pkgconfig"
grep -E "^(CONFIG_LIBDAV1D|CONFIG_MEDIACODEC|CONFIG_HEVC_MEDIACODEC_DECODER)=" "$ff_dir/ffbuild/config.mak" || true
ls -la "$out/lib"
echo "FFmpeg $FFMPEG_VER for Android ready in third_party/ffmpeg/android-aarch64/"
