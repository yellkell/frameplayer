# FramePlayer

A native Linux ARM64 VR video player for the Valve Steam Frame. Aims to be a complete, open-source DeoVR replacement with one-click installation.

See [docs/OUTLINE.md](docs/OUTLINE.md) for the full project outline: platform constraints, feature set, architecture, install strategy, milestones, and risks.

Also here: Chromium XR for Steam Frame, a separate WebXR browser app (yellkell.com/fac and other WebXR pages); [docs/webxr/README.md](docs/webxr/README.md) has its build, patches and install.

## Status

The player is feature-complete for a first release and runs end to end
against a simulated OpenXR headset (Monado) and in the headless preview
harness; it has not yet run on Steam Frame hardware.

- Plays flat, 180°, 360°, fisheye (190/200/220°) and EAC video, mono or
  stereo, with per-video format overrides and adjustments
- Library with thumbnails, search, filters, favorites, ratings, resume and
  history; ~/Videos, ~/Downloads, microSD cards and USB drives are indexed
  automatically (exFAT cards from Windows/macOS included: FramePlayer
  mounts what SteamOS won't)
- Network sources: DeoVR/HereSphere feeds (XBVR, Stash), SMB, WebDAV, DLNA,
  HTTP folders
- Haptics: funscripts with Intiface, TCode devices and The Handy
- Phone web remote and DeoVR remote API
- Signed self-updates; install via Frame Control/FrameDrop or
  `frameplayer-install`

Guides: [user guide](docs/USER-GUIDE.md), [development](docs/DEVELOPMENT.md).

`frame-probe` (Milestone 0) answers the remaining platform questions in one
run on a headset (`tools/frame-probe.sh --build`); results go in
[docs/platform-notes.md](docs/platform-notes.md).

## License

FramePlayer is licensed under either of [Apache License, Version 2.0](LICENSE-APACHE)
or [MIT license](LICENSE-MIT), at your option. Bundled third-party libraries
and fonts keep their own licences, and the controller pictures are renders of
Valve's Steam Frame controller models; see [packaging/licenses](packaging/licenses/THIRD-PARTY.txt).

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in FramePlayer by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.
