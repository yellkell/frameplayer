#!/usr/bin/env bash
# Cross-compile workspace binaries for the Steam Frame (Linux ARM64).
#
# Targets glibc 2.28 so the binaries run on any SteamOS image, inside or
# outside the Steam Linux Runtime container. Uses zig as the cross linker, so
# no ARM64 sysroot or C toolchain is needed.
#
# One-time setup:
#   rustup target add aarch64-unknown-linux-gnu
#   pip install ziglang          # or install zig from your package manager
#   cargo install --locked cargo-zigbuild
#
# Usage: tools/build-frame.sh [cargo args...]   e.g. tools/build-frame.sh -p frame-probe
set -euo pipefail
cd "$(dirname "$0")/.."
target=aarch64-unknown-linux-gnu
cargo zigbuild --release --target "$target.2.28" "$@"
echo "Built into target/$target/release/"
