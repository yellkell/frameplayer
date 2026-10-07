# WebXR on Steam Frame: sandbox fix

An attempt at the Steam Frame WebXR incompatibility. The browser side of the
story matters to FramePlayer because a working, *sandboxed* WebXR browser on the
Frame is the fallback path for web-hosted VR video, and because the same
SteamVR-in-a-sandbox facts constrain our own native player.

**Status (2026-10-02):** root cause identified from upstream Chromium source,
three patches written against `chromium/main`, broker logic verified with a
standalone simulation on real `/proc`. **Not yet built or run on a Frame**: the
on-device confirmation step is in §5.

---

## 1. The problem

Chromium's standard build cannot do WebXR on the Frame. Two community projects
fixed that: [utzcoz/chromium-webxr-linux](https://github.com/utzcoz/chromium-webxr-linux)
(OpenXR backend for Linux, now landed upstream) and
[saphid/chromium-webxr-steam-frame](https://github.com/saphid/chromium-webxr-steam-frame)
(arm64 build + installer for the Frame). Immersive sessions work on the Frame
today **only with `--disable-seccomp-filter-sandbox`**, which switches off the
system-call filter for every Chromium process. saphid's own notes recommend
using that browser for VR content only.

The blocker is tracked in
[utzcoz/chromium-webxr-linux#7](https://github.com/utzcoz/chromium-webxr-linux/issues/7)
and has two parts:

| # | Symptom on Frame (SteamVR 2.17.10, arm64) | Where |
|---|---|---|
| A | XR utility process crashes inside `xrCreateInstance` on syscall `0xd1` = `getsockopt` | seccomp policy |
| B | With A patched, `xrCreateInstance` fails with `VRInitError_Init_Internal`; vrserver logs show the app registered with a PID **one higher** than the XR process | file broker |

## 2. What upstream Chromium looks like now

utzcoz's CL "vr: run the XR device service in the sandbox on Linux" landed as
`e0f937bba4ff` (reland, 2026-09-26). Relevant facts, all read from
`chromium/main` on 2026-10-02:

- `sandbox/policy/linux/bpf_xr_policy_linux.cc`: `XrProcessPolicy` extends
  `GpuProcessPolicy`. It allows `getpeername`/`getsockname`, `alarm`, `flock`,
  `kill(pid, 0)`, AF_UNIX `socket`, brokered `connect`/`bind`. `getsockopt`
  falls through to the baseline, which allows only `SO_PEEK_OFF` and
  **crashes** on anything else. That is symptom A.
- `content/services/isolated_xr_device/xr_sandbox_hook_linux.cc`: the
  pre-sandbox hook starts the broker with a file allow-list (manifest,
  runtime .so, Vulkan ICDs, `/dev/dri`, `/dev/shm` for SteamVR, log dirs,
  SteamVR socket names). It grants **no `/proc` paths at all**.
- `content/browser/service_host/utility_sandbox_delegate.cc`: the XR service
  forks from the **unsandboxed** zygote, so it lives in the host PID namespace
  with the host `/proc`. The policy comment "There is no PID namespace here"
  agrees. So a correct `/proc/self` rewrite is sufficient; no namespace
  translation is needed.
- `sandbox/linux/syscall_broker/broker_host.cc`: since 2022 (`abbfd26`,
  "Rewrite /proc/self in syscall broker") the broker rewrites paths that
  start with **`/proc/self/`** (trailing slash) to `/proc/<pid>/`, where
  `<pid>` is `getpid()` captured in the XR process just before forking the
  broker. The bare path **`/proc/self`** is not matched.
- `sandbox/linux/syscall_broker/syscall_dispatcher.cc`: `openat`/`readlinkat`/
  `fstatat` with any `dirfd` other than `AT_FDCWD` return `EPERM`, so a
  directory fd cannot be used to sidestep the rewrite.

## 3. Root cause of symptom B

With the prefixed form already rewritten, the only way the broker can answer
"for itself" is a request for the **bare** `/proc/self` link:

- `readlink("/proc/self")` returns the *broker's* pid as text. The broker is
  forked by the XR process immediately after `getpid()`, so it is normally
  the next pid: exactly the "one higher" seen in vrserver's log.
- `stat`/`open` of bare `/proc/self` land in the broker's `/proc` directory.

Programs hit the bare form whenever they canonicalise a `/proc/self/...` path:
glibc's `realpath(3)` `readlink()`s every component, so
`realpath("/proc/self/exe")` first asks for `/proc/self`. SteamVR's `vrclient`
identifies the calling application to `vrserver` (app key
`system.generated.openxr.chromium 156.chrome` is derived from the executable
name) and evidently obtains its pid through this route rather than `getpid()`.

The simulation in `tools/webxr/sim.cc` (see §6) reproduces both halves on a
stock Linux box: an unpatched broker answers `readlink("/proc/self")` with
its own pid and `stat("/proc/self")` with its own inode; the patched logic
returns the sandboxed process's values.

> Inference, not yet strace-verified: that vrclient reads the bare link is
> deduced from the symptom plus the upstream rewrite rules. §5 confirms it on
> hardware before anything is sent upstream.

## 4. The patches

In `docs/webxr/patches/`, generated with `git format-patch` against
`chromium/main` at 2026-10-02 (post `e0f937bba4ff`). Together with IWFDK's
controller patch 0004 they form one series, applied in file-name order:
later patches build on earlier ones. Apply it as commits with
`tools/webxr/apply-chromium-patches.sh /path/to/chromium/src` (it fetches
IWFDK's 0004 itself).

**0001 Linux sandbox: broker answers for the bare /proc/self link**
`broker_host.cc`: `RewritePathname()` also maps exact `/proc/self` to
`/proc/<pid>`; `ReadlinkFileForIPC()` special-cases it and returns the
sandboxed pid as the kernel would (the rewritten target is a directory, on
which `readlink` would give `EINVAL`). A `/proc/self` permission is still
required, and still reaches only the sandboxed process's own entries.
Adds `BrokerProcess.RewriteProcSelfLink{Client,Host}` unit tests covering
readlink, truncated readlink, stat and directory open.

**0002 vr: let SteamVR's runtime read the XR process's own /proc entries**
`xr_sandbox_hook_linux.cc`: for `XrRuntimeId::kSteamVr` only, grant
`ReadOnly` on `/proc/self` and `/proc/self/{cmdline,comm,exe,stat,status}`.
Nothing else in `/proc` is reachable.

**0003 sandbox: allow getsockopt(SO_PEERCRED) in the XR process policy**
`bpf_xr_policy_linux.cc`: allow exactly `SOL_SOCKET`/`SO_PEERCRED`; every
other `getsockopt` keeps the baseline behaviour (crash), so new needs still
surface loudly. Equivalent in effect to saphid's local patch, but delegates
to the parent policy instead of returning `EPERM`.

Security review of the delta: the XR process gains (a) one read-only socket
option that reveals the peer's pid/uid/gid on a socket it already owns, and
(b) read access to six of its own `/proc` entries. It gains no access to any
other process.

## 5. Verifying on a Steam Frame

Needs a Frame with Developer Mode, an arm64 Chromium build with the patches,
and a static arm64 `strace`.

1. **Confirm the diagnosis first** (no patched build needed). Run
   `tools/webxr/frame-xr-trace.sh <chromium-dir>` on the headset with the
   seccomp sandbox *enabled*, enter VR on any WebXR page. In
   `proc-paths.txt` expect a bare `readlink("/proc/self")` (or `stat`) from
   the XR utility process, and a `getsockopt(..., SOL_SOCKET, SO_PEERCRED, ...)`
   in `sockopts.txt`. Any `/proc/self/<entry>` outside
   `{cmdline, comm, exe, stat, status}` goes into patch 0002.
2. **Build.** `tools/webxr/build-frame-chromium.sh` (§8). It replaces
   saphid's local `0001-xr-sandbox-allow-getsockopt-SO_PEERCRED.patch` with
   0003, which supersedes it.
3. **Unit tests** on the build host:
   `sandbox_linux_unittests --gtest_filter='BrokerProcess.RewriteProcSelf*'`
   and `content_unittests --gtest_filter='XrSandboxHookLinuxTest.*'`.
4. **Device run** with the launcher's `--disable-seccomp-filter-sandbox`
   **removed**. Success criteria: `isSessionSupported("immersive-vr")` true,
   an immersive session renders, and `vrserver.txt` shows the app registered
   with the XR utility process's real pid.
5. **Regression**: run the same page on a desktop Linux box with Monado to
   confirm the SteamVR-only grants do not affect other runtimes.

## 6. Reproducing the broker behaviour locally

`tools/webxr/sim.cc` is a 150-line standalone model of the broker's
`/proc/self` handling (same rewrite rules, a forked "broker" over a
socketpair, a glibc-style realpath walk). Build and run:

```
g++ -std=c++17 -O1 -Wall -o sim tools/webxr/sim.cc && ./sim
```

Expected: 2 failures for the unpatched model (`readlink` and `stat` of bare
`/proc/self`), 0 for the patched one.

## 7. Upstreaming plan

1. After §5 step 1 confirms the paths, attach the strace excerpt to
   utzcoz/chromium-webxr-linux#7 and link these patches.
2. Submit to Chromium Gerrit as three CLs in this order (0001 is a general
   sandbox fix and can land alone; 0002 depends on it; 0003 is independent).
   Reviewers: sandbox/linux OWNERS for 0001 and 0003, utzcoz plus
   content/services/isolated_xr_device OWNERS for 0002. Add a crbug and
   replace `Bug: none`.
3. Until they land, saphid's Frame build can carry them as local patches and
   drop `--disable-seccomp-filter-sandbox`.

## 8. Building and installing the Frame browser

CL 8132979 (the OpenXR device on Linux) merged on 2026-09-29, so the build no
longer pins a Gerrit patch set: it builds `chromium/main` at `2255089d4176`,
the commit patches 0001-0003 here and IWFDK's controller patch
(`yellkell/iwfdk`, `platform/chromium/patches/0004`) were written against.

**Build** on x86-64 Linux or WSL2 (about 90 GB free, no root apart from
`pkg-config`):

```sh
sudo apt install pkg-config
mkdir -p ~/chromium-xr
tmux new -d -s chromium-xr 'tools/webxr/build-frame-chromium.sh > ~/chromium-xr/build.log 2>&1'
tail -F ~/chromium-xr/stage
```

The script fetches the IWFDK patch itself, checks before syncing that the
whole series applies in order (to a scratch index, `~/chromium-xr/patches.index`),
and resumes when re-run: it skips applying when the patched files already
match the series, and stops on a partly patched or hand-edited tree. Output:
`~/chromium-xr/chromium-xr-arm64.tar.xz`, then the arm64 `device_unittests`
and `sandbox_linux_unittests` in `src/out/XR`. On Windows, keep the WSL
distro and its swap file on a drive with the space, and check out this repo
with LF endings (`.gitattributes` enforces it for patches and scripts).

**Install from a link.** `TAG=… tools/webxr/package-frame-title.sh` packs the
build as a Frame Control title: `ChromiumXR-Frame-arm64.zip` (Chromium in
`chromium/`, launchers `chromium-xr.sh` and `chromium-xr-sandboxed.sh` on
top) plus a manifest per title, for a GitHub release. Its `INSTALL.md` has
the `frame-control://install?manifest=…` links. Both titles share the zip:

- **Chromium XR**: seccomp filter off, the configuration saphid verified.
- **Chromium XR Sandboxed**: seccomp filter on, with its own profile: the
  configuration §5 step 4 tests.

Each opens the page in `~/.config/chromium-xr-frame/home-url` (FramePlayer's
Web XR tab writes it), else Fish & Chips (https://yellkell.com/fac). The
WebXR check page is in the title's `chromium/start/` folder. Logs go to
`~/.local/state/chromium-xr-frame/`.

**Install over SSH** instead, with saphid's installer and a test run:
`tools/webxr/frame-install.sh chromium-xr-arm64.tar.xz [tests.tar.xz]`.

**The check page** (`tools/webxr/frame-webxr-check/`) reports `isSessionSupported`, then in VR
shows every controller button live against the `valve-frame` layout, checks
that Menu no longer ends the session, and produces a JSON report. On a
desktop, `?emulate=frame` or `?emulate=touch` runs it against IWER's emulated
controllers.

### Rendering fixes (found on the headset, 2026-10-03)

Two bugs made three.js games (Fish & Chips: IWSDK 0.4.2, three.js r184)
unusable even with the session running. Both were confirmed with headset
captures (`IVRScreenshots` stereo shots over SSH):

- **Black headset: projection layers.** The Linux Vulkan binding reports
  `SupportsLayers() == false`, so the session gets no layer manager, yet
  Blink still offers `XRWebGLBinding.createProjectionLayer` (the `WebXRLayers`
  runtime feature is stable). three.js draws into a projection layer whenever
  that method exists, so every frame was submitted empty. The launcher passes
  `--disable-blink-features=WebXRLayers`, and three.js falls back to an
  `XRWebGLLayer`.
- **Right eye black or flickering, water missing: patch 0005.** Recording the
  game's WebGL calls showed both eyes drawn correctly into the XR framebuffer,
  so the loss happened after the page. Blink's end-of-frame
  `DiscardFramebufferEXT(depth, stencil)` on the shared buffer also lost
  in-flight colour rendering on ANGLE/GL/zink/Turnip. Patch 0005 skips the
  discard on Linux: both eyes and the water render in 8 of 8 captures, where
  before the right eye was black in 7 of 8.

`tools/webxr/frame-webxr-check/xrtest.html` reproduces the first bug
(`?layers=1`, `?noproj=1`) and has the scenes used to rule out other causes
(`?transmission=1`, `?heavy=N`, `?aa=0`, `?bounded=1`).

### Controller vibration: patch 0008

Chromium gave XR input sources no vibration actuator (crbug.com/955097) and
its OpenXR device had no haptics. Patch 0008 gives each immersive session's
`inputSource.gamepad` a `vibrationActuator` whose `playEffect("dual-rumble",
…)` plays the stronger of the two magnitudes for the effect's duration on
that hand's `/output/haptic` (`xrApplyHapticFeedback`, unspecified
frequency); `reset()` stops it. The effect resolves `"complete"` after its
duration, or `"preempted"` when another effect or `reset()` replaces it.
`gamepad.hapticActuators[0].pulse()` (the older extension) is not
implemented. Run with `--vmodule=openxr_input_helper=1` to log each
`xrApplyHapticFeedback` result. Patch 0012 scales every amplitude by 0.35:
Frame controllers vibrate much harder than Quest's at the same value, so
games tuned on Quest felt heavy. `CHROMIUM_XR_HAPTICS_SCALE` (0 to 1) in the
browser's environment overrides it.

### Games other than Fish & Chips (2026-10-05): patches 0009-0013

Measured on the headset with DevTools (frame times and WebGL timer queries
from the page) and SteamVR stereo screenshots. Down and Fire Fight 2 are
IWSDK 0.4.2 games from yellkell.com/vrmenu that run well on Quest 3.

- **Down was black: patch 0009.** It asks for `framebufferScaleFactor: 0.82`.
  The Vulkan binding made its shared images the swapchain's size, Blink's
  depth buffer had the page's size, and every draw failed ("Attachments are
  not all the same size"). Shared images now have the page's framebuffer
  size, and the binding scales them into the swapchain with `vkCmdBlitImage`.
- **No antialiasing anywhere: patch 0010.** Chromium offers it only when the
  runtime's `maxSwapchainSampleCount` is above 1; SteamVR reports 1. That
  limit doesn't apply on Linux, where Blink resolves into its own SharedImage.
  Patch 0013 uses 2 samples on Linux arm64, as Android does.
- **Fire Fight 2 was laggy: fill rate, and no overlap.** Its script takes
  3-4 ms a frame, its GPU work 19 ms at 2160x2160 per eye. With no GPU fence
  (ANGLE on desktop OpenGL over zink, the default) the render loop
  `glFinish`es every frame, and the Vulkan binding then waited on the CPU
  twice more, so a frame cost script + GPU time. Patch 0011 removes the
  binding's waits; the launcher puts ANGLE on Vulkan, which has the fence; and
  it now defaults to SteamVR's 1728 per eye instead of 2160. Copies that are
  no longer waited for must still finish before `xrDestroySwapchain`: one
  that didn't faulted the GPU when a session ended (kernel `gpu fault ...
  dir=WRITE`), every GL context was lost and Chromium blocked WebGL for the
  sites. `OnSwapchainDestroying()` waits for them.
- **Antialiased pages were black on ANGLE's Vulkan backend.** Blink's
  implicit resolve (`GL_EXT_multisampled_render_to_texture`) never reached
  the headset. The launcher disables those extensions, so Blink resolves
  with a blit.

Fire Fight 2 with all of it: a steady 54 fps (half of 108 Hz, p99 frame
18.9 ms, GPU 15 ms). Down: 108 fps, GPU 4 ms. Fish & Chips renders as before.

| Fire Fight 2, 108 Hz | GPU per frame | Frame rate |
|---|---|---|
| 2160, no AA, zink (before) | 19 ms | 36-38 fps |
| 1728, no AA, zink | 11 ms | 53 fps |
| 1728, 4x AA, zink | 17 ms | 38 fps |
| 1728, 4x AA, Vulkan, 0011 | 19 ms | 47 fps |
| 1728, 2x AA, Vulkan, 0011 | 15 ms | 54 fps |

Patches 0011, 0012 and 0013 build on 0009, 0008 and 0005 (as 0006 and 0008
build on 0004), so the build script checks and applies the patches as one
ordered series.

The launcher reads extra Chromium flags from `~/.config/chromium-xr-frame/flags`
(one per line) for experiments, e.g. `--use-angle=gl` and
`--enable-features=OpenXR` to go back to zink.

### Thumbstick scrolling (2026-10-07)

Steam shows the browser as a desktop window: the laser reaches Chromium as a
pen (with mouse events), but the thumbsticks don't scroll. Steam turns the
pointing hand's stick into a few weak wheel steps (6-26 px), and passes the
sticks to Chromium only now and then, as a virtual Xbox pad that pages can't
see until a button is pressed. `frame-title/stick-scroll.py`, started by the
launcher next to Chromium, reads the sticks from SteamVR as a background
OpenVR app and, while the laser is on the browser (the X pointer moved in the
last 0.35 s; it doesn't during WebXR sessions), sends wheel steps with XTEST:
up to 16 a second (1600 px) at full deflection, from the controller whose
trigger was pulled last (where SteamVR puts the laser; the right one before
that). About 1% of a core while idle; `CHROMIUM_XR_STICK_SCROLL=0` turns it
off. Tests: `python3 -m unittest test_stick_scroll` in `frame-title/`.

### Major WebXR sites, and VR video (2026-10-07): patch 0014

three.js (VR, AR), A-Frame, Babylon.js, PlayCanvas, Moon Rider, the
Immersive Web samples, Wonderland Engine (The Escape Artist, Dead Secret,
Study Room) and Unity's WebXR export all enter VR and render; most hold
108 Hz. The slow ones are fill-rate bound content (three.js sandbox 41 fps at
21 ms GPU, PlayCanvas's VR demo 70 fps at 8.9 ms). AR sessions show the
passthrough camera (headset screenshots don't capture it). `hand-tracking`
is granted, but the Frame does no camera hand tracking through SteamVR's
OpenXR: the joints come from the controllers' finger sensors (a hand held
still is a fixed open pose; checked with the controllers put down: no hands).
Pages that need bare hands don't work.

VR video had no hardware decode and no HEVC at all ("no supported streams"),
and AV1 8K60 in software pulled WebXR down to 82 fps. The build now has
`use_v4l2_codec = true` and patch 0014, and the launcher enables
`AcceleratedVideoDecoder,PreferV4L2VideoAcceleration`, so Chromium's V4L2
stateful decoder drives the iris decoder (H.264, HEVC, VP9; no AV1):

| In a WebXR session, 108 Hz | Software | V4L2 (0014) |
|---|---|---|
| HEVC 8192x4096 60 fps | doesn't play | 60 fps, XR 107.5 |
| HEVC 5760x2880 60 fps | doesn't play | 60 fps, XR 108 |
| H.264 5760x2880 60 fps | 60 fps | 60 fps, XR 108 |
| AV1 8192x4096 60 fps | 33 fps, XR 82 | (software) |

0014 declares the stream's size on the bitstream queue (iris refused 0x0
with EINVAL), passes HEVC access units to the stateful decoder (it was
`NOTIMPLEMENTED()`), and on Linux sends multi-planar (NV12) frames to WebGL
through an intermediate RGBA shared image: ANGLE on Vulkan can't import the
decoder's DMA-BUF ("invalid mailbox name"), and video textures were black
while `<video>` played. Limit: iris admits an 8K session only when no other
decoder session is open, so 8K fails to start while the previous page's
decoder is still closing or Steam holds one (FramePlayer's primer frame works
around this; not ported). MSE, WebCodecs (8K H.264/HEVC/VP9) and
`requestVideoFrameCallback` work; Widevine and WebXR media layers don't.

## 9. What this does not solve

- Anything SteamVR needs beyond these two items will only show up once the
  seccomp filter is on and the pid is right. §5 step 1 is designed to catch
  those in one pass.
- `/proc/thread-self` is not rewritten (the broker does not know the caller's
  tid). No evidence SteamVR uses it.
- `lstat("/proc/self")` through the broker now reports a directory rather
  than a symlink. glibc's `realpath` uses `readlink`, not `lstat`, so this
  does not affect canonicalisation.
- Widevine and the other limitations in saphid's README are unrelated to the
  sandbox (controller haptics: patch 0008).
