# ADR 0001: Custom engine in Rust, no game engine

- Status: accepted (pending owner confirmation, OUTLINE §8 Q4)
- Date: 2026-10-02
- Context: OUTLINE §3.1, §3.3

## Context

FramePlayer exists to play 8K60 10-bit stereo video natively on the Steam
Frame (Snapdragon 8 Gen 3, Adreno 750, SteamOS ARM, SteamVR OpenXR runtime on
the headset, Mesa Turnip Vulkan). The single most important property is the
video path: hardware decode → DMA-BUF → Vulkan image → projection pass, with no
CPU copy of decoded frames and with frame selection driven by the OpenXR
predicted display time.

The options were:

| Option | Zero-copy video | Frame pacing control | Binary / deps | UI cost |
|---|---|---|---|---|
| Rust + `ash` + `openxr` + own decode glue | yes (we own the import) | full | small, static except glibc + Vulkan loader | build a small immediate-mode toolkit |
| Godot 4 (proven on Frame by FrameHome) | no: video textures go through CPU copies; would need a GDExtension doing everything in row 1 anyway | engine-owned | large | low |
| Unity / Unreal | no Linux ARM64 Frame target we can rely on; same copy problem | engine-owned | very large | low |
| C++ equivalent of row 1 | yes | full | small | same as Rust |

## Decision

Write a custom engine in Rust: one Cargo workspace with a crate per module
(`xr`, `gfx`, `video`, `audio`, `sources`, `library`, `haptics`, `remote`,
`ui`, `app`, `updater`, `installer`), Vulkan through `ash` and OpenXR through
the `openxr` crate, both loading their system libraries at runtime.

## Consequences

- We control the DMA-BUF import (`VK_EXT_external_memory_dma_buf`,
  `VK_EXT_image_drm_format_modifier`) and can fall back to Vulkan Video or
  software decode without fighting an engine.
- Deterministic frame pacing: the render thread owns OpenXR and Vulkan and
  never blocks on I/O (tokio runtime for everything else).
- We must build and maintain a small VR UI toolkit (`fp-ui`): panels, lists,
  sliders, text, virtual keyboard. Kept intentionally small.
- Cross-compiling for `aarch64-unknown-linux-gnu` is first-class in Rust; CI
  builds on native arm64 runners and in an SLR-based Docker image.
- Memory safety in the parsers that face untrusted input (containers,
  subtitles, funscripts, DeoVR JSON, network protocols) without a separate
  hardening effort.
- C dependencies (FFmpeg, dav1d, libass, libsmb2) are optional cargo features,
  statically linked in release builds, so the default build and tests need no
  system libraries.
