# FramePlayer

A native Linux ARM64 VR video player for the Valve Steam Frame. Aims to be a complete, open-source DeoVR replacement with one-click installation.

See [docs/OUTLINE.md](docs/OUTLINE.md) for the full project outline: platform constraints, feature set, architecture, install strategy, milestones, and risks.

## Status

Every module in the outline is implemented as a Rust crate with unit tests, but **nothing has run on a Steam Frame yet**. Behaviour that depends on unconfirmed platform facts is marked `[verify]` in the code and tracked in [docs/platform-notes.md](docs/platform-notes.md). Milestone 0 (the on-device spike that answers those questions) is the next step.

## Layout

| Crate | What it does |
|---|---|
| `crates/core` (`fp-core`) | Shared types: projections, stereo layouts, picture corrections and keyframes, media info, timestamps, filename auto-detection, UI draw lists |
| `crates/xr` (`fp-xr`) | OpenXR instance/session, Vulkan bring-up via `XR_KHR_vulkan_enable2`, swapchains and layers, Frame controller / eye-gaze / hand-tracking input |
| `crates/gfx` (`fp-gfx`) | Projection meshes (equirect, fisheye, EAC, curved screen, OBJ), colour and tone-mapping math, WGSL shaders compiled by naga, Vulkan renderer with DMA-BUF import |
| `crates/video` (`fp-video`) | MP4 and Matroska demuxers, spherical metadata, V4L2 hardware decoder, software fallbacks, playback engine (seek, speed, A-B loop, chapters), subtitles |
| `crates/audio` (`fp-audio`) | Audio output, downmix, resampler, time stretch, ambisonics and binaural rendering |
| `crates/sources` (`fp-sources`) | Local, HTTP/HLS/DASH, WebDAV, DLNA, SMB, SFTP and DeoVR-feed sources; encrypted credentials |
| `crates/library` (`fp-library`) | SQLite library index, search, playlists, resume points, per-video overrides, thumbnails, export/import |
| `crates/haptics` (`fp-haptics`) | Funscripts, timeline engine, Handy / buttplug.io / TCode backends |
| `crates/remote` (`fp-remote`) | DeoVR-compatible TCP remote, REST + WebSocket API, LAN web remote with QR pairing |
| `crates/ui` (`fp-ui`) | Immediate-mode VR UI toolkit and the Library, Player, Picture Adjust and Settings screens |
| `crates/updater` (`fp-updater`) | Signed release manifests, delta downloads, atomic install with automatic rollback |
| `crates/installer` (`frameplayer-install`) | Desktop CLI: pair with the headset, install, launch, logs; release signing tools |
| `crates/app` (`frameplayer`) | The application binary that wires everything together |

Other directories: `dist/` (launcher, install manifests, download page), `docker/` (aarch64 build images), `tools/` (device scripts, release script, test-video generator, perf capture), `docs/` (outline, platform notes, ADRs, format support, install guide).

## Building

```sh
make check     # fmt + clippy + tests, as CI runs them
make build     # release build for the Frame (aarch64-unknown-linux-gnu)
make frame-go  # push to a paired headset, launch, tail logs
```

Cross-compiling needs `gcc-aarch64-linux-gnu`. Optional features pull in system libraries (`ffmpeg`, `dav1d`, `cpal` audio output); the `docker/` image builds them statically. See [docs/format-support.md](docs/format-support.md) for which decode paths each feature enables.

## Installing

End-user steps are in [docs/install.md](docs/install.md).

## Licence

MIT OR Apache-2.0 ([LICENSE-MIT](LICENSE-MIT), [LICENSE-APACHE](LICENSE-APACHE)). See [docs/adr/0002-licence.md](docs/adr/0002-licence.md) for the reasoning and the FFmpeg (LGPL-only build) implications.
