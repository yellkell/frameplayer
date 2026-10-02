# Steam Frame platform notes

Milestone 0 exit criterion (OUTLINE §6): every **[verify]** assumption about
the Frame is answered here. Until then the code implements the best-known
approach and marks the spot with a `// [verify]` comment.

**Status values:** `unverified` (nothing tested yet), `confirmed`,
`refuted` (write what is true instead and open an issue for the code change),
`partial`. When you change a status, add the date, SteamOS `BUILD_ID`
(`frameplayer-install status`), SteamVR version and what you ran.

Useful commands on a paired headset (see `tools/frame.sh`):

```sh
tools/frame.sh shell 'cat /etc/os-release; uname -a; ldd --version | head -1'
tools/frame.sh shell 'ls -l /dev/video* /dev/dri/*; id'
tools/frame.sh shell 'v4l2-ctl --list-devices; v4l2-ctl -d /dev/video0 --list-formats-out --list-formats'
tools/frame.sh shell 'vulkaninfo --summary'          # if vulkan-tools is present
tools/frame.sh logs -f                                 # FramePlayer logs its XR/Vulkan capability report at start
```

## 1. Platform checklist (OUTLINE §1, §3, §5)

| # | Assumption | What the code currently assumes | How to test on the device | Status |
|---|---|---|---|---|
| P1 | Refresh rates SteamVR offers native apps (raylib quickstart saw only 72 Hz exposed; 1728×1728 recommended per eye) | Configurable requested rate, 0 = runtime default (`crates/app/src/config.rs`); `crates/xr/src/session.rs` logs what the runtime offers; projection meshes budgeted for 72 Hz | Launch, read the startup capability line in `logs`; try 90/120/144 in settings; record `xrEnumerateDisplayRefreshRatesFB` output and `recommendedImageRectWidth` | unverified |
| P2 | Hardware video decode path exposed by SteamOS: V4L2 stateful (`iris`/`venus`), Vulkan Video on Turnip, or neither | V4L2 stateful M2M first (`crates/video/src/decode/v4l2`), Vulkan Video second (`crates/gfx/src/vk/video.rs`), software dav1d/libavcodec capped at 4K last | `v4l2-ctl --list-devices`; `--list-formats-out` for HEVC/AV1/VP9 on each `/dev/video*`; `vulkaninfo | grep -i video_decode`; play `fp_180_LR_hevc_*` from `tools/gen-test-videos.sh` and read which decoder the info panel reports | unverified |
| P3 | `/dev/video*` nodes usable by the non-root gaming-mode user | Opens the node directly; the decoder selector falls back to software and reports why (`select_decoder`) | `ls -l /dev/video*`, `id` (is the user in `video`?); run FramePlayer and check the decoder line | unverified |
| P4 | Decoder CAPTURE format / DRM modifier (NV12/P010 linear vs Qualcomm UBWC `QC08C`/`QC10C`) and DMA-BUF import into Turnip | Imports per-plane with `VK_EXT_image_drm_format_modifier`; modifiers the driver does not accept (e.g. UBWC) are refused by `supports_modifier` (`crates/gfx/src/vk/video.rs`), and a multi-planar UBWC import is still to do | `v4l2-ctl --list-formats` on the CAPTURE queue; FramePlayer logs the modifier it receives and whether `vkGetPhysicalDeviceImageFormatProperties2` accepts it | unverified |
| P5 | Vulkan Video (`VK_KHR_video_decode_h265/av1`) on the Frame's Turnip | Optional path, used only if the extensions are reported | `vulkaninfo --summary` device extensions | unverified |
| P6 | Passthrough: can a native app request system passthrough blending (`XR_ENVIRONMENT_BLEND_MODE_ALPHA_BLEND`), or only OpenVR's camera API? | Asks for ALPHA_BLEND only if enumerated, otherwise OPAQUE + relies on SteamVR's own passthrough toggle (`crates/xr/src/select.rs`) | Startup log lists `xrEnumerateEnvironmentBlendModes`; toggle SteamVR passthrough while the UI is up | unverified |
| P7 | glibc / libstdc++ baseline on SteamOS-ARM (native apps run outside the Steam Linux Runtime) | Built in the SLR 4 arm64 SDK image (fallback Debian bookworm, glibc 2.36); everything but glibc and the Vulkan loader is static | `ldd --version`, `strings /usr/lib/libstdc++.so.6 | grep GLIBCXX_3.4 | tail -1`; compare with `max GLIBC symbol` printed by `tools/release.sh` | unverified |
| P8 | Foveation hints (`XR_FB_foveation` or similar) exposed | Not used unless enumerated; mesh density chosen to be foveation-friendly | Startup extension report | unverified |
| P9 | System Vulkan loader (`libvulkan.so.1`) present outside the SLR container | `ash` loads it at runtime (feature `loaded`, no bundled loader) | `ls /usr/lib/libvulkan.so.1`; run FramePlayer outside Steam via `tools/frame.sh run` | unverified |
| P10 | Frame controller interaction profile `/interaction_profiles/valve/frame_controller_valve` and its component paths | Suggests bindings for it plus fallbacks (`crates/xr/src/bindings.rs`) | SteamVR binding UI / `xrEnumerateBoundSourcesForAction` dump in the log | unverified |
| P11 | Eye gaze via `XR_EXT_eye_gaze_interaction` | Enabled when present; gaze-assisted pointing degrades to controller ray | Startup extension report; look at UI elements with gaze enabled | unverified |
| P12 | Swapchain formats (raylib quickstart only confirmed sRGB RGBA8) | Prefers sRGB RGBA8/BGRA8 (`choose_color_format`, `crates/xr/src/select.rs`); HDR intermediates stay in our own images | Startup log of `xrEnumerateSwapchainFormats` | unverified |
| P13 | Composition layer cylinder orientation (`XR_KHR_composition_layer_cylinder`) | Spec behaviour: arc centred on local −Z, u to the right (`crates/ui/src/input.rs`) | Point at the left edge of a cylinder UI and confirm the hit maps to the left edge | unverified |
| P14 | Removable media mount point (microSD / USB) | `/run/media/<user>/<label>` like the Deck (`crates/sources/src/local.rs`) | Insert a card; `findmnt -l | grep media` | unverified |
| P15 | Audio output: cpal → ALSA → `pipewire-alsa` works for a gaming-mode app with sane timestamps | `crates/audio/src/output/cpal_out.rs` | Play `fp_audio51_*`; check A/V offset with the timecode overlay | unverified |
| P16 | LAN discovery: firewall allows SSDP replies (DLNA) and inbound LAN connections for the remote API | `crates/sources/src/dlna/ssdp.rs`, `crates/remote` | Browse a DLNA server; connect to the web remote from a phone | unverified |
| P17 | Thermal: sustained 8K60 HEVC decode + render for 2 h at ≥ 72 Hz; battery ≥ 2.5 h at 4K | Budgets in OUTLINE §3.4 | `tools/perf-capture.sh --launch --duration 7200` with an 8K test clip; record max temperatures and dropped frames | unverified |
| P18 | SteamOS `VERSION_ID` numbering on the Frame (Deck uses 3.x; Frame press notes say 0.3.0) used for `min_steamos` gating | `crates/updater/src/platform.rs` compares dotted versions; unknown → allowed | `cat /etc/os-release` | unverified |

## 2. Installation, pairing and update assumptions

| # | Assumption | What the code currently assumes | How to test | Status |
|---|---|---|---|---|
| I1 | `steamos-devkit-service` listens on HTTP port 32000 and advertises mDNS `_steamos-devkit._tcp` in Developer Mode | `crates/installer/src/{config,discovery}.rs` | `frameplayer-install discover`; `curl http://<ip>:32000/properties.json` | unverified |
| I2 | Devkit endpoints: `GET /properties.json` (has `login`), `GET /login-name`, `POST /register` with the SSH public key; approval appears on the headset; denial is 401/403 | `crates/installer/src/devkit.rs` (polls SSH after register, so blocking or immediate replies both work) | `frameplayer-install -v pair --host <ip>`; deny once, approve once | unverified |
| I3 | Login account name on the Frame (`deck` on the Deck) | Taken from the service; `--user` overrides | `curl http://<ip>:32000/login-name` | unverified |
| I4 | Games live in `~/devkit-game/<name>` and appear under Library → Non-Steam → Devkit Game | Install root `~/devkit-game/frameplayer` | Install with Frame Control, then `frameplayer-install status` | unverified |
| I5 | `~/devkit-utils/steam-client-create-shortcut --parms <json>` exists after pairing and accepts `gameid/directory/argv/settings` | Tried first; falls back to Steam's JS API | `tools/frame.sh shell 'ls ~/devkit-utils'` after pairing with each tool | unverified |
| I6 | Steam CEF DevTools on `127.0.0.1:8080` in Developer Mode, target `SharedJSContext`, `SteamClient.Apps.{GetAllShortcuts,AddShortcut,SetShortcutName,SetCustomArtworkForApp,RunGame,RemoveShortcut}` | `crates/installer/src/steam.rs` (artwork asset types grid=0 hero=1 logo=2 wide=3 icon=4; game id `(appid<<32)|0x02000000`) | `frameplayer-install install`, then check art; `frameplayer-install launch` | unverified |
| I7 | Favourites API for `--pin` (`collectionStore`) and whether "pin to home" means Favorites on the Frame's home | Tries `collectionStore.SetAppsAsFavorite`, then the favourites collection; failure is a warning | `frameplayer-install install --pin` | unverified |
| I8 | Frame Control / FrameDrop manifest field names and link schemes (`frame-control://install?manifest=`, FrameDrop's scheme) | Superset manifest with aliases (`dist/frameplayer.json`, ADR 0003); FrameDrop link `framedrop://install?manifest=` | Click both buttons on the download page with each tool installed; read their source/docs | unverified |
| I9 | Tools extract tarballs preserving the `versions/` tree and the exec bit on `frameplayer.sh` | Launcher adopts `RELEASE` on first start | Install via each tool; `tools/frame.sh shell 'ls -lR ~/devkit-game/frameplayer | head'` | unverified |
| I10 | GNU coreutils (`mv -T`, `sort -V`), GNU tar, gzip on the headset; `zstd` for `.tar.zst` | `dist/frameplayer.sh`, `crates/installer/src/remote.rs` | `tools/frame.sh shell 'mv --version; tar --version; command -v zstd'` | unverified |
| I11 | A native OpenXR app started over SSH (outside Steam) finds the SteamVR runtime | `tools/frame.sh run` is a debugging aid only; normal launch goes through Steam | `tools/frame.sh run` and read the XR init log line | unverified |
| I12 | Windows OpenSSH accepts our key file permissions under `%APPDATA%` | `crates/installer/src/sshkey.rs` | `frameplayer-install pair` on stock Windows 11 | unverified |
| I13 | SLR 4 arm64 SDK image path (`registry.gitlab.steamos.cloud/steamrt/steamrt4/sdk/arm64`) | `docker/Dockerfile.aarch64`, fallback Debian bookworm in `release.yml` | `docker pull --platform linux/arm64 <path>` | unverified |
| I14 | Sysfs paths for thermal zones / Adreno devfreq; SteamVR log location `~/.local/share/Steam/logs` | `tools/perf-capture.sh` (missing files tolerated) | `tools/frame.sh shell 'ls /sys/class/thermal /sys/class/kgsl /sys/class/drm/card0/device/devfreq; ls ~/.local/share/Steam/logs'` | unverified |

## 3. Every `[verify]` marker in the code, by file

Regenerate with `tools/verify-index.sh --update docs/platform-notes.md`
(other modules add markers as they land). Status for each is `unverified`
unless noted in sections 1–2 above.

<!-- BEGIN verify-index -->
| Location | Assumption to check on hardware |
|---|---|
| `.github/workflows/release.yml:53` | SLR 4 arm64 SDK path; fall back to Debian bookworm so a release is never blocked. |
| `crates/app/src/config.rs:26` | Requested display refresh rate in Hz; 0 = runtime default. → which rates SteamVR on the Frame offers to native apps. |
| `crates/audio/src/output/cpal_out.rs:8` | On SteamOS (Frame) the default cpal host is ALSA, which reaches PipeWire through the `pipewire-alsa` PCM plugin. Confirm that the `default` PCM exists for a non-root gaming-mode app and that the reported playback timestamps are sane; otherwise fall back to ... |
| `crates/gfx/src/color.rs:121` | Luminance of display white in nits. → Frame LCD peak. |
| `crates/gfx/src/mesh/eac.rs:35` | Bottom-row rotations follow ffmpeg's v360 EAC table; confirm the orientation with a YouTube EAC test clip (look straight down/back/up). |
| `crates/gfx/src/vk/mod.rs:6` | conformant. → on the Frame's Mesa build. |
| `crates/gfx/src/vk/renderer.rs:828` | Turnip honours FOREIGN acquire + PREINITIALIZED/GENERAL without discarding decoder output (no implicit-sync wait here: the decoder must have finished the frame before handing it over). |
| `crates/gfx/src/vk/video.rs:10` | Which modifier the Frame's decoder exports. If it is a Qualcomm UBWC (compressed) modifier, per-plane import will be refused by `supports_modifier`; that case needs a multi-planar import with the exporter's full plane list (incl. metadata planes) and plane-... |
| `crates/haptics/src/buttplug.rs:687` | `latency_ms: 30,`: BLE latency through Intiface varies per device. |
| `crates/haptics/src/handy.rs:16` | Endpoint paths and bodies follow the public Handy API v2 documentation (handyfeeling.com/api/handy/v2/docs). Handy has since published API v3 ("HSP" streaming); check whether v2 remains available for firmware 4 devices before release. |
| `crates/haptics/src/handy.rs:31` | Default upload endpoint of Handy's temporary script hosting. →  |
| `crates/haptics/src/handy.rs:103` | Lookahead for HDSP streaming (cloud round trip + device). → tune on real network. |
| `crates/haptics/src/handy.rs:432` | whether leaving HSSP discards the loaded script; assume it does. |
| `crates/haptics/src/handy.rs:539` | GATT service exposed by Handy firmware 3. →  |
| `crates/haptics/src/handy.rs:541` | Write characteristic for handyplug payloads. →  |
| `crates/haptics/src/handy.rs:576` | Field numbers follow buttplug's `handyplug.proto` (oneof `LinearCmd = 403`). |
| `crates/haptics/src/handy.rs:595` | Encode a keepalive `Ping { Id }` (oneof field 102). →  |
| `crates/haptics/src/handy.rs:742` | `latency_ms: 50,`:  |
| `crates/installer/src/config.rs:12` | 32000 is the port used by ValveSoftware/steamos-devkit's service on the Steam Deck; confirm the Frame's service uses the same. |
| `crates/installer/src/devkit.rs:12` | All of the above on the Frame: endpoint names, whether `/register` blocks until the user answers or returns immediately (we handle both by polling SSH afterwards), the status code for "denied", and the login name (`deck` on the Deck; the Frame may use anoth... |
| `crates/installer/src/discovery.rs:3` | The Frame advertises the same service type as the Deck. |
| `crates/installer/src/remote.rs:16` | `~/devkit-utils/steam-client-create-shortcut --parms <json>` is how the SteamOS Devkit Client registers a "Devkit Game" on the Deck; check it exists on the Frame after pairing with Frame Control / FrameDrop / Valve's client, and the exact JSON keys it accepts. |
| `crates/installer/src/remote.rs:33` | SteamOS ships GNU tar with gzip; `zstd` (only needed for .tar.zst uploads) is expected because pacman depends on it. |
| `crates/installer/src/site_manifest.rs:5` | Neither tool's manifest schema is formally documented. This format is a deliberate superset: a structured core (`tarball`, `launch`, `artwork`) plus flat aliases (`url`, `download_url`, `sha256`, `size`, `launch_command`, `executable`) holding the same valu... |
| `crates/installer/src/sshkey.rs:70` | Windows OpenSSH rejects private keys readable by other users. Files under %APPDATA% inherit an ACL limited to the user, SYSTEM and Administrators, which OpenSSH accepts; confirm on a stock Windows 11 install. |
| `crates/installer/src/steam.rs:9` | Everything in this module against the Frame's Steam client: the port, the target title, and the `SteamClient.Apps.*` method names and signatures (taken from Decky Loader / SteamGridDB plugin usage on the Deck), the artwork asset-type numbers, and the favour... |
| `crates/remote/src/deovr.rs:11` | Port 23554 and the field set follow DeoVR's published remote-control docs and the client implementations in MultiFunPlayer / ScriptPlayer; re-test against ohdoki before release. When nothing is loaded we send keepalive pings instead of a status object, whic... |
| `crates/sources/src/deovr.rs:357` | Confirm against current XBVR and Stash releases that the form-POST login is still what their /deovr endpoints expect. |
| `crates/sources/src/dlna/ssdp.rs:59` | SteamOS's firewall (if enabled on the Frame) must allow inbound unicast UDP replies to the ephemeral port. |
| `crates/sources/src/local.rs:286` | Confirm the Frame mounts microSD/USB under /run/media/<user>/<label> like the Steam Deck (udisks2 via steamos-automount) rather than /media. |
| `crates/ui/src/input.rs:203` | that SteamVR's `XR_KHR_composition_layer_cylinder` on the Frame centres the arc on local -Z with u increasing to the right, as the spec says. |
| `crates/ui/src/input.rs:264` | tune against Frame hand-tracking jitter. |
| `crates/ui/src/theme.rs:65` | from 1.5 m (≈ 0.76°). → tune on the Frame's 2160² panels and lenses. |
| `crates/updater/src/platform.rs:48` | On the Frame's ARM branch `VERSION_ID` may follow its own numbering (press coverage says "SteamOS 0.3.0" for the Frame update) rather than the Deck's 3.x. `min_steamos` in manifests must use whatever the device reports here; check `/etc/os-release` on hardw... |
| `crates/video/src/decode/v4l2/mod.rs:20` | Frame (SM8650) specifics that need hardware confirmation: * driver name/node: upstream `iris` (SM8650) vs downstream `venus`, and whether `/dev/video*` is accessible to a non-root gaming-mode user; * the CAPTURE formats offered (`NV12` vs `QC08C` UBWC, `P01... |
| `crates/video/src/decode/v4l2/mod.rs:451` | their DMA-BUFs alive (orphaned buffers). → on iris. |
| `crates/xr/src/bindings.rs:208` | Every component path below on SteamVR for the Frame (dump with `xrEnumerateBoundSourcesForAction` / SteamVR binding UI). Paths mirror the Index profile's naming where the hardware matches. |
| `crates/xr/src/input.rs:132` | Feels right with Frame hand tracking; alternatives use a shoulder-to-knuckle ray. |
| `crates/xr/src/pinch.rs:20` | tune against Frame hand tracking noise. |
| `crates/xr/src/select.rs:21` | Which formats SteamVR on the Frame offers; the raylib quickstart only reports that sRGB RGBA8 works. |
| `crates/xr/src/select.rs:51` | Whether SteamVR on the Frame offers ALPHA_BLEND to native apps at all; the outline expects only OPAQUE (passthrough is system-driven). |
| `crates/xr/src/session.rs:73` | Run on the Frame and record this line in docs/platform-notes.md. |
| `dist/site/index.html:72` | `// `: FrameDrop's install-link scheme; adjust once its docs confirm it. |
| `docker/Dockerfile.aarch64:16` | SLR 4's registry path and arm64 tag. Valve publishes Steam Runtime SDK images at registry.gitlab.steamos.cloud/steamrt/<suite>/sdk (sniper = SLR 3, x86_64 only). The Frame-era runtime ("steamrt4", arm64) is expected at the path below; if it is unavailable, ... |
| `tools/frame.sh:94` | Outside Steam the SteamVR runtime env may be missing. → whether a native OpenXR app started from SSH finds the runtime on the Frame. |
| `tools/perf-capture.sh:48` | sysfs paths on the Frame (SM8650): thermal zone names and the Adreno devfreq node. The loop tolerates missing files. |
| `tools/perf-capture.sh:64` | SteamVR's own logs for compositor-side timing. → location on the Frame. |
<!-- END verify-index -->

## 4. Results log

Append findings here, newest first:

```
### YYYY-MM-DD  P2 hardware decode: <confirmed|refuted|partial>
SteamOS BUILD_ID: …  SteamVR: …  FramePlayer: …
Command / clip: …
Result: …
Code change needed: … (issue #)
```
