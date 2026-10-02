#!/usr/bin/env bash
# Regenerate crates/fp-ffmpeg-sys/src/bindings_<arch>.rs from the headers of
# our own FFmpeg build (tools/build-ffmpeg.sh). Run after changing FFmpeg.
# Needs bindgen-cli, libclang, and for aarch64 the libc6-dev-arm64-cross headers.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
crate="$root/crates/fp-ffmpeg-sys"
gen() { # arch clang-target sysroot-args...
  local arch="$1" target="$2"; shift 2
  bindgen "$crate/wrapper.h" -o "$crate/src/bindings_$arch.rs" \
    --allowlist-function '(av|avformat|avcodec|avio|avsubtitle|swr|sws|av_bsf)_.*' \
    --allowlist-type '(AV|Swr|Sws).*' \
    --allowlist-var '(AV_|AVERROR_|AVFMT_|AVIO_|AVSEEK_|SWS_|FF_|LIBAV).*' \
    --default-enum-style consts --no-prepend-enum-name \
    --no-layout-tests --no-doc-comments --use-core \
    -- -I"$root/third_party/ffmpeg/$arch/include" --target="$target" "$@"
  echo "generated bindings_$arch.rs ($(wc -l < "$crate/src/bindings_$arch.rs") lines)"
}
gen x86_64 x86_64-linux-gnu
gen aarch64 aarch64-linux-gnu --sysroot=/usr/aarch64-linux-gnu -isystem /usr/aarch64-linux-gnu/include
