#!/usr/bin/env bash
# Developer loop against a paired Steam Frame (OUTLINE §5 "device lab").
#
#   tools/frame.sh pair [--host IP]   pair via Valve's devkit service (once)
#   tools/frame.sh build              cargo build --release for aarch64
#   tools/frame.sh push               build + copy a dev build to the headset
#   tools/frame.sh install [TARBALL]  full install incl. Steam library entry
#   tools/frame.sh launch             start through Steam (correct XR env)
#   tools/frame.sh run [ARGS]         start directly over SSH, output streamed
#   tools/frame.sh stop               kill a running FramePlayer
#   tools/frame.sh logs [-f]          tail launcher/app logs
#   tools/frame.sh shell [CMD]        interactive shell or one command
#   tools/frame.sh status             what is installed / running
#   tools/frame.sh go                 push + launch + follow logs (make frame-go)
#
# Env: FRAME_DEVICE (paired device name/IP), PROFILE (release|debug),
#      FPI (frameplayer-install command), FRAME_DOCKER=1 to build in docker/.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
TARGET=aarch64-unknown-linux-gnu
PROFILE=${PROFILE:-release}
DEV_VERSION=0.0.0-dev
REMOTE_ROOT='devkit-game/frameplayer'
FPI=${FPI:-}
if [ -z "$FPI" ]; then
  if [ -x "$ROOT/target/release/frameplayer-install" ]; then
    FPI="$ROOT/target/release/frameplayer-install"
  else
    FPI="cargo run -q --release --manifest-path $ROOT/Cargo.toml -p fp-installer --"
  fi
fi
DEV_ARGS=()
[ -n "${FRAME_DEVICE:-}" ] && DEV_ARGS=(--device "$FRAME_DEVICE")

fpi() { $FPI ${DEV_ARGS[@]+"${DEV_ARGS[@]}"} "$@"; }

SSH_CFG=""
ssh_cfg() {
  if [ -z "$SSH_CFG" ]; then
    SSH_CFG=$(mktemp "${TMPDIR:-/tmp}/frame-ssh.XXXXXX")
    trap 'rm -f "$SSH_CFG"' EXIT
    fpi ssh-config --alias frame >"$SSH_CFG"
  fi
}
rsh() { ssh_cfg; ssh -F "$SSH_CFG" frame "$@"; }

build() {
  local flags=()
  [ "$PROFILE" = release ] && flags+=(--release)
  if [ "${FRAME_DOCKER:-0}" = 1 ]; then
    docker run --rm --platform linux/arm64 -v "$ROOT:/src" -w /src \
      -v frameplayer-cargo:/usr/local/cargo/registry frameplayer-build:latest \
      cargo build ${flags[@]+"${flags[@]}"} --target "$TARGET" -p fp-app
  else
    (cd "$ROOT" && cargo build ${flags[@]+"${flags[@]}"} --target "$TARGET" -p fp-app)
  fi
}

push() {
  build
  local bin="${CARGO_TARGET_DIR:-$ROOT/target}/$TARGET/$PROFILE/frameplayer"
  [ -x "$bin" ] || { echo "no binary at $bin" >&2; exit 1; }
  ssh_cfg
  local vdir="$REMOTE_ROOT/versions/$DEV_VERSION"
  rsh "mkdir -p $vdir/bin $vdir/share/frameplayer"
  echo "==> uploading $(du -h "$bin" | cut -f1) binary"
  if command -v rsync >/dev/null && rsh 'command -v rsync >/dev/null'; then
    rsync -az --info=progress2 -e "ssh -F $SSH_CFG" "$bin" "frame:$vdir/bin/frameplayer"
    [ -d "$ROOT/assets" ] && rsync -az --delete -e "ssh -F $SSH_CFG" "$ROOT/assets/" "frame:$vdir/share/frameplayer/"
  else
    scp -F "$SSH_CFG" -q "$bin" "frame:$vdir/bin/frameplayer.new"
    rsh "mv -f $vdir/bin/frameplayer.new $vdir/bin/frameplayer"
  fi
  scp -F "$SSH_CFG" -q "$ROOT/dist/frameplayer.sh" "frame:$REMOTE_ROOT/frameplayer.sh"
  # Adopt the dev build directly (same files the launcher uses) without
  # putting it on trial, so crash loops while debugging never roll back.
  rsh "cd $REMOTE_ROOT && chmod +x frameplayer.sh versions/$DEV_VERSION/bin/frameplayer \
    && echo $DEV_VERSION > RELEASE && echo $DEV_VERSION > .installed-release \
    && ln -sfn versions/$DEV_VERSION .current.tmp && mv -Tf .current.tmp current && rm -f trial"
  echo "==> pushed $DEV_VERSION ($PROFILE)"
}

cmd=${1:-help}
[ $# -gt 0 ] && shift
case "$cmd" in
  pair) fpi pair "$@" ;;
  build) build ;;
  push) push ;;
  install)
    if [ $# -gt 0 ]; then fpi install --tarball "$1"; else fpi install --latest; fi ;;
  launch) fpi launch ;;
  run)
    # Outside Steam the SteamVR runtime env may be missing. [verify] whether a
    # native OpenXR app started from SSH finds the runtime on the Frame.
    rsh -t "cd $REMOTE_ROOT && FP_LOG_STDERR=1 ./frameplayer.sh $*; tail -n 50 \${XDG_DATA_HOME:-\$HOME/.local/share}/frameplayer/logs/launcher.log" ;;
  stop) rsh "pkill -f '$REMOTE_ROOT/.*bin/frameplayer' && echo stopped || echo 'not running'" ;;
  logs) fpi logs "$@" ;;
  shell) if [ $# -gt 0 ]; then rsh "$*"; else rsh -t; fi ;;
  status) fpi status ;;
  go)
    push
    fpi launch || { echo "Steam launch failed; starting directly" >&2; rsh "cd $REMOTE_ROOT && nohup ./frameplayer.sh >/dev/null 2>&1 &"; }
    sleep 1
    fpi logs -n 50 -f ;;
  perf) exec "$ROOT/tools/perf-capture.sh" "$@" ;;
  help|-h|--help) sed -n '2,20p' "$0" ;;
  *) echo "unknown command: $cmd (try help)" >&2; exit 2 ;;
esac
