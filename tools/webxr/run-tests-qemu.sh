#!/bin/bash
# Run the patches' arm64 unit tests on the x86-64 build host under qemu-user,
# so they don't need the headset. Needs `qemu-user-static` and a finished
# build-frame-chromium.sh run with BUILD_TESTS=1.
#
#   tools/webxr/run-tests-qemu.sh
#
# The OpenXR profile test (patch 0004) is plain logic and runs faithfully under
# qemu. The broker tests (patch 0001) fork and read /proc/self, which qemu-user
# emulates; a failure there is worth re-running on real arm64 before trusting.
set -uo pipefail
W=${CHROMIUM_XR_DIR:-$HOME/chromium-xr}
B=$W/src/out/XR
SYSROOT=$W/src/build/linux/debian_bullseye_arm64-sysroot
QEMU=$(command -v qemu-aarch64-static || command -v qemu-aarch64) ||
  { echo "install qemu-user-static" >&2; exit 1; }

run() {  # binary filter
  echo "== $1 --gtest_filter=$2"
  local out rc=0
  out=$(cd "$B" && "$QEMU" -L "$SYSROOT" "./$1" --gtest_filter="$2" --single-process-tests 2>&1) || rc=$?
  grep -E '^\[ *(RUN|OK|FAILED|PASSED|SKIPPED|==========) *\]|Failure|error' <<<"$out" | tail -n 40
  (( rc == 0 )) || echo "exit $rc"
  return "$rc"
}
status=0
run device_unittests 'OpenXrInteractionProfilesTest.*' || status=1
run sandbox_linux_unittests 'BrokerProcess.RewriteProcSelf*' || status=1
exit $status
