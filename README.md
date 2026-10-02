# FramePlayer

A native Linux ARM64 VR video player for the Valve Steam Frame. Aims to be a complete, open-source DeoVR replacement with one-click installation.

See [docs/OUTLINE.md](docs/OUTLINE.md) for the full project outline: platform constraints, feature set, architecture, install strategy, milestones, and risks.

Also here: [docs/webxr/README.md](docs/webxr/README.md), an analysis and Chromium patch set for running WebXR on the Steam Frame with the seccomp sandbox enabled.

## Status

Milestone 0 (platform de-risking) is in progress. `frame-probe` answers every open platform question in one run on a headset:

```
tools/frame-probe.sh --build     # cross-builds, copies to a paired Frame, runs, fetches the report
```

Results are recorded in [docs/platform-notes.md](docs/platform-notes.md). Building needs Rust, `zig` and `cargo-zigbuild`; see `tools/build-frame.sh`.

