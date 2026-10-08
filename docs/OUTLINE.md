# FramePlayer — Project Outline

A fully featured VR video player built **exclusively and natively for the Valve Steam Frame**, intended as a complete DeoVR replacement (and borrowing the best ideas from HereSphere), with a one-click install path that a non-developer can complete in under two minutes.

> Status: planning. Nothing below is implemented yet. Items marked **[verify]** are assumptions about the Frame platform that must be confirmed on real hardware before they drive design decisions.

---

## 0. Why this project

| Problem today | What FramePlayer does about it |
|---|---|
| DeoVR / SKYBOX / HereSphere on Frame run as x86 Windows builds through Proton + FEX, or as Android APKs through Lepton. Both cost CPU, battery, and latency, and neither can use the Frame's hardware video decoder cleanly. | Native Linux ARM64 binary talking directly to the on-device SteamVR OpenXR runtime and the Snapdragon 8 Gen 3 hardware decoder. |
| Existing players are generic cross-platform ports; none uses Frame-specific features (eye tracking, Frame controller profile, hand tracking, Arcturus colour passthrough, 144 Hz). | Frame is the *only* target, so every one of those is first-class. |
| Sideloading on Frame means Developer Mode, pairing, and a desktop utility. | Ship manifests for both community installers (Frame Control `frame-control://install`, FrameDrop "Install with FrameDrop"), plus an in-headset self-updater, with a Steam Store release as the end goal. |
| DeoVR is closed, ad-supported in places, and tied to a content platform. | Open source (MIT or Apache-2.0), zero telemetry, DeoVR-compatible APIs so existing libraries (XBVR, Stash, ohdoki, Handy) just work. |

---

## 1. Platform facts the design rests on

Gathered from Valve's Steamworks Steam Frame docs and community projects (Frame Control, FrameDrop, FrameHome, SteamFrameRaylibQuickstart, chromium-webxr-linux). Re-check anything marked **[verify]** on device.

### 1.1 Hardware
- Qualcomm Snapdragon 8 Gen 3 (SM8650), ARM64, Adreno 750, 16 GB LPDDR5X, 256 GB / 1 TB UFS, microSD slot.
- Dual 2160×2160 LCD, 72–144 Hz. **[verify]** which refresh rates the on-device SteamVR runtime actually offers to native apps; the raylib quickstart reports only 72 Hz exposed, 1728×1728 recommended per-eye render size.
- Four outward monochrome tracking cameras with IR illumination. Stock passthrough is **monochrome with a configurable tint**. Arcturus Vision add-on (expansion port, dual 32 MP RGB) adds colour passthrough system-wide.
- Inward eye-tracking cameras; exposed to apps via `XR_EXT_eye_gaze_interaction`.
- Hardware video decode: HEVC / VP9 / AV1 up to 8K60 (SoC spec), HDR10 / HDR10+ / HLG / Dolby Vision bitstreams. **[verify]** which path SteamOS exposes on the Frame: V4L2 stateful (`venus`/`iris` driver), Vulkan Video on Turnip, or neither.
- Frame controllers: A/B/X/Y + menu (right), D-pad + view (left), shoulder (bumper)/grip/trigger/stick on both, capacitive touch on every button. OpenXR interaction profile: `/interaction_profiles/valve/frame_controller_valve` from the `XR_VALVE_frame_controller_interaction` extension; component paths per Valve's published profile ([ValveSoftware/Unity](https://github.com/ValveSoftware/Unity), `SteamFrameControllerProfile.cs`). Without the extension SteamVR presents the controllers as emulated Touch controllers. Bare-hand tracking available.
- Wi-Fi 6E with dedicated streaming radio, 21.6 Wh battery, speakers + mics.

### 1.2 Software
- SteamOS 3 (ARM64 branch; 0.3.0 as of Sept 2026). KDE Plasma desktop mode exists.
- SteamVR runs **on the headset** as the compositor and OpenXR runtime (SteamVR 2.17.x). Native apps connect over AF_UNIX sockets; the runtime verifies peers with `SO_PEERCRED`. Keep that in mind if we ever sandbox ourselves.
- Vulkan 1.3 via Mesa Turnip (Valve-maintained fork). OpenGL also works through `XR_KHR_opengl_enable`, but Vulkan is the path for zero-copy video.
- SteamVR's OpenXR runtime does **not** expose `XR_FB_passthrough` or any passthrough extension. Passthrough as a "see your room while the UI is up" feature is driven system-side. **[verify]** whether a native app can request system passthrough blending (e.g. via `XR_ENVIRONMENT_BLEND_MODE_ALPHA_BLEND`) or whether OpenVR's camera interface is the only option. Fallback: use SteamVR's own passthrough toggle and keep our UI/video on an alpha-blended layer.
- Three execution models for third-party software: Android 10 APK (Lepton), **Linux ARM64 (ours)**, Windows x86 (Proton 11 + FEX). Native ARM64 Linux builds run outside the Steam Linux Runtime container, so we must bundle or statically link everything beyond glibc. **[verify]** exact glibc / libstdc++ baseline on SteamOS-ARM.
- Sideloading: Developer Mode (Settings → System → Developer Mode), SSH pairing via Valve's devkit service, files land in `~/devkit-game/<name>`, app shows up under Library → Non-Steam → Devkit Game. Both Frame Control and FrameDrop automate this and support website install buttons with SHA-256 verified manifests.
- Steam DevTools HTTP endpoint on the headset at `127.0.0.1:8080` (Frame Control uses it over SSH tunnel). Relevant for our installer and for a desktop remote.

---

## 2. Product scope

### 2.1 Target users
1. People with a large local/NAS library of 180°/360°/fisheye stereo video who currently use DeoVR or HereSphere.
2. People using library managers (XBVR, Stash, Whisparr-style tools) that already speak the DeoVR JSON feed and remote API.
3. People who want a great flat-screen cinema (2D/3D movies, screen mirroring of a phone/PC later).
4. Users of interactive haptic devices (Handy, OSR2/SR6, buttplug.io ecosystem) with funscripts.

### 2.2 Feature set (DeoVR parity + beyond)

**Playback core**
- Containers: MP4/MOV, MKV, WebM, TS; codecs HEVC, AV1, VP9, H.264 via hardware; AV1/HEVC software fallback (dav1d / ffmpeg) for unusual profiles.
- Up to 8K60 10-bit hardware decode, zero-copy DMA-BUF import into Vulkan.
- HDR: HDR10 / HLG decode with on-GPU tone mapping to the Frame's SDR LCD; per-video exposure and tone curve.
- Precise seek (keyframe + decode-ahead), variable speed 0.25×–4× with pitch-corrected audio, frame step, A-B loop, chapters.
- Audio: stereo, 5.1/7.1 downmix, first-order and higher-order ambisonics (AmbiX / FuMa) head-tracked, binaural HRTF, A/V sync offset, audio-track selection.
- Subtitles: SRT / ASS / WebVTT / PGS, external and embedded, rendered at adjustable depth with automatic stereo disparity (HereSphere-style).

**Projection & stereo**
- Flat (cinema screen, adjustable size/distance/curvature), 180° equirect, 360° equirect, fisheye 180/190/200 with per-lens FOV and centre offset, Canon RF 5.2mm dual fisheye, MKX200/MKX220 presets, EAC/cubemap, custom mesh (OBJ) projection.
- Stereo: mono, SBS, OU, swap eyes, per-eye crop; 2D-to-3D off by design (no fake depth).
- Auto-detection from filename tokens (`_LR`, `_TB`, `_180`, `_360`, `MKX200`, `FISHEYE190`, `SBS`, `3DH`…), container spherical metadata (Google spatial media v1/v2 boxes), and a per-file override that persists.
- HereSphere-class corrections: software IPD adjust, horizontal/vertical alignment, lens-distortion k1/k2, zoom, tilt/roll, and an optional stereo-depth-estimated reprojection that lets 180° content tolerate small head translations. Keyframed settings over time.
- Comfort: head-locked vs world-locked, recenter on button, screen auto-position when sitting/lying, "lying down" mode with gravity override, dimmable environment, optional passthrough background (mono tint, or Arcturus colour when present).

**Library & sources**
- Local: internal storage, microSD, USB-C drives (auto-mount watch).
- Network: SMB 2/3 (libsmb2), WebDAV, HTTP(S) direct, HLS/DASH streams, DLNA/UPnP discovery and browse, SFTP.
- DeoVR-compatible JSON feed consumer (`/deovr` endpoints from XBVR, Stash and friends) so existing library servers appear as remote libraries with thumbnails, tags, and scripts.
- Library indexer with thumbnails, preview scrubbing sprites, duration/resolution/codec badges, tags, ratings, favourites, resume points, watch history, smart playlists, fuzzy search, and timestamped bookmarks with auto-seek.
- Optional embedded web-feed browser (lightweight, not a full Chromium) later; **not** in MVP.

**Interactive / haptics**
- Funscript loading (same dir, `Interactive/` fallback, remote URL from feed), multi-axis scripts, offset tuning, script preview strip on the timeline.
- Device backends: The Handy (local Bluetooth and cloud API), buttplug.io / Intiface Central over WebSocket, OSR/TCode over serial-over-USB or network.
- DeoVR-compatible remote control API (TCP JSON on the same port DeoVR uses) so existing tools such as ohdoki and script players keep working unchanged, plus our own richer WebSocket/REST API.

**Input & presence**
- Full Frame controller mapping with touch-sensing used for hover highlights; hands-only operation via pinch/grab; eye-gaze-assisted pointing (gaze + pinch to click), gaze-dimming of UI when watching.
- Voice: none in MVP.

**System integration**
- Launches from the Steam library like any app; Steam Input disabled in favour of OpenXR actions.
- In-app self-update (signed release manifest, delta download, atomic swap, rollback).
- Optional companion: phone/desktop web remote served from the headset over the LAN (QR code to pair). Browse library, control playback, type search terms with a real keyboard.
- Multi-user watch-together (DeoVR "multiuser") deferred to a later phase; design the player state machine so a sync layer can be added.

### 2.3 Explicit non-goals
- Not a general media centre (no music, no photos beyond stereo stills, no live TV).
- No Android/Windows/Quest/PC-VR builds. Frame-only, by design.
- No content store, accounts, ads, or telemetry.
- No DRM (Widevine) playback.

---

## 3. Technical architecture

### 3.1 Stack decision

**Recommendation: custom engine in Rust, no game engine.**

| Option | Pros | Cons |
|---|---|---|
| **Rust + ash (Vulkan) + openxr crate + GStreamer/ffmpeg bindings** (recommended) | Full control of the zero-copy video path (DMA-BUF → Vulkan image), tiny binary, deterministic frame pacing, easy static linking, excellent cross-compile story for aarch64. | Must build our own UI toolkit (keep it small: panels, lists, sliders, text). |
| Godot 4 (FrameHome proves it runs on Frame) | Fast UI iteration, OpenXR already wired. | Video texture path is CPU copy; 8K60 10-bit will not fit the budget. Would need a GDExtension doing everything below anyway. |
| C++ equivalent of the Rust option | Same control; more existing VR sample code. | Memory-safety and build hygiene costs; no real upside over Rust here. |

### 3.2 Process and module layout

```
frameplayer/                  (single binary + bundled libs)
├── xr/          OpenXR session, swapchains, views, reference spaces, actions,
│                eye gaze, hand tracking, frame timing / phase sync
├── gfx/         Vulkan device (Turnip), render graph, projection meshes,
│                YUV→RGB + tone-map compute, UI compositing, foveated hints
├── video/       Demux (ffmpeg), HW decode (V4L2 stateful or Vulkan Video),
│                SW fallback (dav1d/ffmpeg), DMA-BUF import, A/V clock,
│                seek engine, subtitle renderer (libass)
├── audio/       PipeWire/ALSA output, ambisonic decoder, HRTF, resampler
├── sources/     local fs, smb2, webdav, http/hls/dash, dlna, sftp,
│                deovr-feed client; unified async "Source" trait
├── library/     SQLite index, thumbnailer (uses video/ in batch mode),
│                tags, playlists, resume, search
├── haptics/     funscript parser, timeline, backends (handy, buttplug, tcode)
├── remote/      DeoVR-compatible TCP API, REST+WS API, LAN web remote UI
├── ui/          Immediate-mode VR UI toolkit, panels, virtual keyboard,
│                laser + gaze + hand interaction, theming
├── app/         State machine (Library → Player → Settings), config, logging
└── updater/     release manifest check, download, verify, swap, rollback
```

All I/O (network sources, indexing, haptics) runs on a tokio runtime off the render thread. The render thread owns OpenXR and Vulkan exclusively and never blocks on I/O.

### 3.3 Video path (the part that matters most)
1. Demux with ffmpeg (`libavformat`) from any `Source` via a custom AVIO callback (so SMB/WebDAV/HTTP all look like files).
2. Decode:
   - Preferred: V4L2 stateful decoder on `/dev/video*` through GStreamer's `v4l2slh265dec`/`v4l2slav1dec` or a direct V4L2 M2M wrapper. **[verify]** node availability and permissions for a non-root app on the Frame.
   - Alternative: Vulkan Video (`VK_KHR_video_decode_h265`, `_av1`) if Turnip on the Frame exposes it. **[verify]**
   - Fallback: software (`dav1d` for AV1, ffmpeg for HEVC) with multi-threading; cap at 4K for software paths and surface a warning.
3. Decoded frames exported as DMA-BUF, imported into Vulkan with `VK_EXT_external_memory_dma_buf` + `VK_EXT_image_drm_format_modifier` as NV12/P010 multi-plane images.
4. A compute pass converts YUV→linear RGB, applies tone mapping/exposure/sharpening, writes to an RGBA16F texture.
5. The projection pass samples that texture per eye through the selected projection mesh with per-eye UV offsets (SBS/OU), IPD/alignment corrections, and optional depth-reprojection.
6. UI composited on a separate OpenXR quad/cylinder layer so text stays sharp under reprojection.
7. Frame pacing: decode-ahead queue of 3–4 frames; present on `xrWaitFrame` cadence; frame selection by predicted display time; audio is master clock.

### 3.4 Performance budgets (72 Hz baseline, 90/120 Hz stretch)
- GPU: ≤ 8 ms per frame at 1728×1728 per eye for 8K equirect; use foveation-friendly mesh density and `XR_FB_foveation`-style hints if the runtime exposes anything. **[verify]**
- Thermal: sustained 8K60 HEVC decode + render for 2 h without throttling below 72 Hz; measure with `SFXR_PERF_LOG`-style counters and SteamVR's perf HUD.
- Battery: target ≥ 2.5 h of 4K playback.
- Cold start to library: < 3 s. Video open to first frame: < 700 ms local, < 2 s over SMB.

### 3.5 Data & config
- SQLite (WAL) in `$XDG_DATA_HOME/frameplayer/`; config as TOML; per-video overrides keyed by content hash + path.
- Everything exportable/importable as a single zip (migration, backup).

### 3.6 Security
- Credentials for SMB/WebDAV stored encrypted with a key kept in the user's home dir (no secret service on SteamOS gaming mode; document the trade-off).
- Remote APIs bind to LAN only, token-protected, off by default.
- Updater verifies ed25519 signature on the release manifest and SHA-256 on payloads.

---

## 4. Installation: the "incredibly easy" story

Three tiers, shipped in this order:

### Tier 1 — One-click from the website (day one)
1. User enables Developer Mode once on the Frame and pairs with Frame Control **or** FrameDrop (both free, both support pairing without passwords via Valve's devkit service).
2. Our download page has two buttons: **Install with Frame Control** (`frame-control://install?manifest=https://…/frameplayer.json`) and **Install with FrameDrop**. We publish a signed manifest with the ARM64 tarball URL, SHA-256, launch command, and Steam artwork so it lands in the library looking like a real app.
3. Done. The app appears under Library → Non-Steam → Devkit Game with proper art.

### Tier 2 — Our own tiny installer (shortly after)
- `frameplayer-install` : a single-file CLI for macOS/Windows/Linux that reuses Valve's devkit pairing flow (SSH key exchange, approve on headset), uploads the build, registers it, sets artwork, and optionally pins it to the home environment. Zero dependencies; the same code powers a small GUI wrapper.
- Also a *headset-side* path: install the first version with either tool above; after that, updates are in-app (Tier 1 users also get this).

### Tier 3 — Steam Store (the real answer)
- Register as a Steamworks partner (one-time fee), ship the Linux ARM64 depot, pass Frame certification. Installation becomes "click Install in the Store, in the headset". Free app with optional "Supporter DLC" so the project is sustainable without ads.
- Keep Tiers 1–2 as the beta channel.

---

## 5. Repository layout, tooling, CI

```
frameplayer/
├── crates/           (Rust workspace: xr, gfx, video, audio, sources, library,
│                      haptics, remote, ui, app, updater, installer)
├── assets/           projection meshes, HRTF, fonts, icons, Steam artwork
├── dist/             manifests for Frame Control / FrameDrop, release scripts
├── docker/           aarch64 build image based on Valve's SLR 4 SDK (sysroot
│                      for the correct glibc), plus a QEMU-user test image
├── tools/            frame.sh (pair / push / launch / logs), perf capture,
│                      synthetic test-video generator (all projections/codecs)
├── docs/             this outline, ADRs, format support matrix, API docs
└── .github/workflows build (aarch64 cross), lint, unit tests, release
```

- **Build**: `tools/build-frame.sh`, which cross-compiles with `cargo zigbuild` against a glibc 2.28 baseline. No ARM64 sysroot, Docker image or emulation is needed, and binaries run inside or outside the Steam Linux Runtime container. Static-link ffmpeg/dav1d/libsmb2/libass; use the system Vulkan loader and Turnip. OpenXR runtimes are loaded directly from the active manifest rather than through the Khronos C++ loader (see `crates/frame-probe/src/xr_loader.rs`). **[verify]** that the system Vulkan loader is present outside the SLR container.
- **CI**: GitHub Actions on ARM64 runners; unit tests for parsers (funscript, filename detection, DeoVR feed), projection math, and the seek engine; QEMU can't run SteamVR, so XR tests run against a recorded-session simulator (the raylib quickstart's record/replay idea).
- **Device lab**: a `make frame-go` style command that pushes, launches, and tails logs on a paired headset. Nightly perf run on a real Frame with the synthetic video set, reporting frame time and dropped frames.
- **Release**: tag → CI builds tarball → signs manifest → publishes GitHub Release → updates website manifests → in-app updater picks it up.

---

## 6. Milestones

| # | Milestone | Deliverable | Exit criteria |
|---|---|---|---|
| 0 | **Spike / de-risk** (2–3 wk). Platform probe built (`crates/frame-probe`); device run pending, results go in [platform-notes.md](platform-notes.md) | Native ARM64 OpenXR app on Frame rendering a 4K HEVC equirect via HW decode + DMA-BUF import; raw controller input | 72 Hz sustained; every **[verify]** item in §1 answered and written to `docs/platform-notes.md` |
| 1 | **Playable MVP** | Local-file browser, flat/180/360 SBS/OU, auto-detect, seek/speed, subtitles, basic settings, Frame Control + FrameDrop manifests | A DeoVR user can watch their local library end to end |
| 2 | **Library & network** | SQLite index, thumbnails, tags, resume, SMB/WebDAV/DLNA/HTTP, DeoVR JSON feed, web remote | XBVR/Stash libraries appear and play |
| 3 | **Interactive** | Funscript + Handy/buttplug/TCode, DeoVR-compatible remote API, timeline script strip | Existing ohdoki / script tools work with zero config |
| 4 | **HereSphere-class picture** | Fisheye presets, lens/alignment/IPD corrections, keyframed settings, HDR tone mapping, ambisonics + HRTF, 8K60 | Side-by-side with HereSphere on identical clips: equal or better |
| 5 | **Frame-native polish** | Eye-gaze UI, hand-only mode, passthrough background, Arcturus colour, 90/120 Hz where offered, self-updater | "Feels like Valve shipped it" |
| 6 | **Steam Store** | Steamworks depot, certification, Supporter DLC | Live on the Store |
| 7 | **Beyond DeoVR** | Watch-together sync, depth-map volumetric playback, phone/PC screen casting | Opt-in, after a stable 1.0 |

### Status (October 2026)

Everything below is implemented and tested in CI, in the headless
`--preview` harness and against Monado's simulated OpenXR headset; nothing
has run on Steam Frame hardware yet, so every exit criterion that names the
device is still open.

| # | Done | Still open |
|---|---|---|
| 0 | `frame-probe`; aarch64/glibc 2.28 builds; direct OpenXR runtime loading | Device run; 72 Hz with 4K HEVC; zero-copy DMA-BUF import (frames are copied, as V4L2 decode in FFmpeg 7.1 returns NV12 in memory) |
| 1 | Local browser, all layouts, auto-detect, seek/speed, subtitles, settings, manifests, microSD/USB drives | — |
| 2 | Library, thumbnails, resume, SMB/WebDAV/DLNA/HTTP, DeoVR/HereSphere feeds, web remote | Tags UI (stored, searchable, not editable in the headset); credentials encrypted at rest |
| 3 | Funscripts (multi-axis), Handy/Buttplug/TCode, DeoVR remote API, heatmap strip | Checking against real devices and tools |
| 4 | Fisheye presets, lens/alignment/IPD, keyframes, PQ/HLG tone mapping, head-tracked ambisonics | HRTF rendering; 8K60 performance |
| 5 | Passthrough background, signed self-updater, installer, Steam artwork | Eye-gaze UI, hand-only mode, Arcturus colour, refresh-rate choice |
| 6–7 | — | Not started |

---

## 7. Risks and mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| No usable HW decode API exposed to non-root apps on SteamOS-ARM | Core value lost | Spike first (Milestone 0). Fallbacks: Vulkan Video → GStreamer V4L2 → software capped at 4K; escalate to Valve via Steamworks forums with a concrete request |
| SteamVR on Frame only offers 72 Hz / limited swapchain formats to native apps | Lower ceiling than DeoVR via Proton? No, same runtime; but limits polish | Design projection meshes for 72 Hz; adopt higher rates as they appear |
| Passthrough not accessible from OpenXR | Weaker MR mode | Use system passthrough toggle + alpha-blend layer; track SteamVR releases |
| Sideloading UX changes (Valve adds/blocks install links) | Tier 1 breaks | Tier 2 installer under our control; Tier 3 removes the problem |
| glibc / driver drift across SteamOS updates | Binary stops launching | Static link everything but glibc/Vulkan loader; CI builds against the oldest supported SteamOS image; updater can hot-fix |
| Scope creep toward "media centre" | Never ships | §2.3 non-goals enforced in reviews |
| Adult-content association affects Store certification | Tier 3 blocked | Player is content-neutral, ships with no content, no built-in feeds; same stance HereSphere takes on Steam |

---

## 7b. Side track: WebXR on the Frame

A sandboxed WebXR browser is the fallback path for web-hosted VR video and a
useful reference for how SteamVR behaves inside a seccomp sandbox. Chromium's
immersive sessions currently work on the Frame only with the seccomp filter
disabled. The root cause and three Chromium patches are in
[docs/webxr/README.md](webxr/README.md); on-device confirmation is pending.

## 8. Open questions for the owner
1. ~~Licence~~ Decided: MIT OR Apache-2.0 (LICENSE-MIT, LICENSE-APACHE). FFmpeg stays an LGPL build; its decoders need nothing GPL.
2. Project name final? (`FramePlayer` vs something trademark-safer given "Frame" is Valve's.)
3. Is Tier 3 (Steam Store) a goal from the start, which affects partner registration timing and the content-neutral stance?
4. Rust confirmed as implementation language?
5. Do you own a Frame for the Milestone 0 spike, and an Arcturus add-on for colour passthrough testing?

---

## 9. Sources consulted
- Valve Steamworks: Steam Frame overview, Custom Engines, How to load and run games on Steam Frame (partner.steamgames.com/doc/steamframe and subpages)
- saphid/steam-frame (Frame Control): sideloading, `frame-control://install` links, devkit pairing
- FrameDrop coverage (Digital Citizen, SteamDeckHQ), SteamOS 0.3.0 Frame update notes
- kblood/FrameHome (Godot 4 on Frame), Dinsmoor/SteamFrameRaylibQuickstart (native ARM64 OpenXR, Frame controller profile, 72 Hz / 1728² observations)
- utzcoz/chromium-webxr-linux issue #7 (SteamVR 2.17 on Frame, socket/seccomp behaviour)
- UploadVR / Road to VR on Arcturus Vision colour passthrough; Rectus/openxr-steamvr-passthrough on SteamVR OpenXR passthrough limits
- Qualcomm Snapdragon 8 Gen 3 product brief (decode capabilities)
- HereSphere and DeoVR store pages / help centres for feature parity list
