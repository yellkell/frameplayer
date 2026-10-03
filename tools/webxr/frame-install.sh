#!/bin/bash
# Install the patched WebXR Chromium on a Steam Frame, plus what's needed to
# test the patches. Run ON THE FRAME (Desktop Mode terminal or SSH):
#
#   tools/webxr/frame-install.sh chromium-xr-arm64.tar.xz [chromium-xr-tests-arm64.tar.xz]
#   tools/webxr/frame-install.sh --uninstall
#
# 1. Runs saphid/chromium-webxr-steam-frame's frame/install.sh on the tarball
#    (the browser, the `chromium-xr` launcher, menu entry and Steam shortcut).
#    That launcher keeps --disable-seccomp-filter-sandbox, so it works even if
#    the sandbox patches (docs/webxr/patches 0001-0003) don't.
# 2. Adds "Chromium XR (sandboxed)": the same build with the seccomp filter ON
#    and its own profile, which is what tests patches 0001-0003.
# 3. Installs `frame-webxr-check [sandboxed]`, which serves the check page on
#    localhost (a secure context, so WebXR is allowed) and opens it.
# 4. With a tests tarball, unpacks the unit tests and runs them.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
saphid=${SAPHID_REPO:-$HOME/chromium-webxr-steam-frame}
state=$HOME/.local/share/frameplayer-webxr
bin=$HOME/.local/bin
apps=$HOME/.local/share/applications
sandboxed=$bin/chromium-xr-sandboxed
check=$bin/frame-webxr-check

if [[ ${1:-} == --uninstall ]]; then
  if [[ -f $state/steam-shortcut.py ]]; then
    python3 "$state/steam-shortcut.py" remove "Chromium XR (sandboxed)" "$state/steam-appid" ||
      echo "Couldn't remove the Steam shortcut; delete Chromium XR (sandboxed) by hand." >&2
  fi
  rm -rf "$state" "$HOME/chromium-xr-tests"
  rm -f "$sandboxed" "$check" "$apps/chromium-xr-sandboxed.desktop"
  echo "Removed the sandboxed launcher, check page and tests. To remove the browser itself:"
  echo "  ~/.local/share/chromium-xr/uninstall.sh"
  exit 0
fi

tarball=${1:-}
tests=${2:-}
[[ -n $tarball && -f $tarball ]] || { echo "usage: $0 chromium-xr-arm64.tar.xz [chromium-xr-tests-arm64.tar.xz] | --uninstall" >&2; exit 2; }
[[ $(uname -m) == aarch64 ]] || { echo "Run this on the Steam Frame (arm64); this is $(uname -m)." >&2; exit 1; }

if [[ ! -x $saphid/frame/install.sh ]]; then
  echo "Fetching saphid/chromium-webxr-steam-frame into $saphid"
  git clone -q https://github.com/saphid/chromium-webxr-steam-frame "$saphid"
fi
"$saphid/frame/install.sh" "$tarball"
cat "$HOME/chromium-xr/BUILD-INFO.txt" 2>/dev/null || true

mkdir -p "$state" "$bin" "$apps"
install -m 644 "$saphid/frame/steam-shortcut.py" "$state/"

# saphid's launcher, minus the seccomp switch, with its own profile so both
# variants can be compared side by side.
grep -q -- '^ *--disable-seccomp-filter-sandbox \\' "$saphid/frame/chromium-xr" ||
  { echo "saphid's launcher changed; update the sed below." >&2; exit 1; }
sed -e '/^ *--disable-seccomp-filter-sandbox \\/d' \
    -e 's#\.config/chromium-xr"#.config/chromium-xr-sandboxed"#' \
    "$saphid/frame/chromium-xr" > "$sandboxed"
chmod 755 "$sandboxed"
if grep -q -- '^ *--disable-seccomp-filter-sandbox' "$sandboxed"; then
  echo "Couldn't strip the seccomp switch from the sandboxed launcher." >&2; exit 1
fi

icon=$HOME/chromium-xr/product_logo_256.png
cat > "$apps/chromium-xr-sandboxed.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Chromium XR (sandboxed)
Comment=Chromium XR with the seccomp sandbox on (tests the FramePlayer sandbox patches)
Exec=$sandboxed %U
Icon=$icon
Terminal=false
Categories=Network;WebBrowser;
EOF
if appid=$(python3 "$state/steam-shortcut.py" ensure "Chromium XR (sandboxed)" "$sandboxed" "$HOME" "$icon" "$state/steam-appid"); then
  echo "Steam shortcut 'Chromium XR (sandboxed)' ready (app id $appid)"
else
  echo "Couldn't add the sandboxed Steam shortcut; add $sandboxed as a non-Steam game by hand." >&2
fi

# The check page, served on localhost only.
rm -rf "$state/frame-webxr-check"
cp -r "$here/frame-webxr-check" "$state/"
cat > "$check" <<'EOF'
#!/bin/bash
# Serve the Frame WebXR check page on localhost and open it.
#   frame-webxr-check            # in Chromium XR (seccomp off)
#   frame-webxr-check sandboxed  # in Chromium XR (sandboxed)
set -euo pipefail
port=8765
if ! (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
  nohup python3 -m http.server "$port" --bind 127.0.0.1 --directory "@STATE@/frame-webxr-check" >/dev/null 2>&1 &
  sleep 1
fi
launcher="@BIN@/chromium-xr"
[[ ${1:-} == sandboxed ]] && launcher="@BIN@/chromium-xr-sandboxed"
exec "$launcher" "http://localhost:$port/"
EOF
sed -i -e "s#@STATE@#$state#g" -e "s#@BIN@#$bin#g" "$check"
chmod 755 "$check"

if [[ -n $tests ]]; then
  t=$HOME/chromium-xr-tests
  rm -rf "$t"; mkdir -p "$t"
  tar -xJf "$tests" -C "$t"
  # Tests look for their resources next to the binary.
  for f in "$HOME"/chromium-xr/*.pak "$HOME"/chromium-xr/icudtl.dat "$HOME"/chromium-xr/*.bin; do
    [[ -e $t/$(basename "$f") ]] || ln -s "$f" "$t/"
  done
  echo
  echo "== device_unittests (IWFDK patch 0004: Frame controller profile)"
  "$t/device_unittests" --gtest_filter='OpenXrInteractionProfilesTest.*' 2>&1 | tail -n 15 || true
  echo
  echo "== sandbox_linux_unittests (FramePlayer patch 0001: bare /proc/self)"
  "$t/sandbox_linux_unittests" --gtest_filter='BrokerProcess.RewriteProcSelf*' 2>&1 | tail -n 15 || true
fi

cat <<EOF

Installed. Next, in the headset:
  1. Steam library > Chromium XR (sandboxed). Or from a terminal: frame-webxr-check sandboxed
  2. Open http://localhost:8765/ (frame-webxr-check does this), press Enter VR,
     press every control, press Menu, then hold both triggers and grips for 2 s.
  3. Copy the report and send it back. If the sandboxed browser can't enter VR,
     run the same check with plain Chromium XR (frame-webxr-check) to tell a
     sandbox problem from anything else, and run tools/webxr/frame-xr-trace.sh.
EOF
