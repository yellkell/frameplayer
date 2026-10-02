# ADR 0002: Licence: MIT OR Apache-2.0, LGPL-only FFmpeg

- Status: accepted as default (owner may revisit, OUTLINE §8 Q1)
- Date: 2026-10-02

## Context

OUTLINE §0 promises an open-source player with zero telemetry. §8 asks
whether to choose MIT, Apache-2.0 or GPL-3. The licence interacts with the
media libraries we link statically into a single binary:

| Library | Licence | Notes |
|---|---|---|
| FFmpeg (libavformat/libavcodec/libswresample/libswscale) | LGPL-2.1+ by default; GPL-2+ with `--enable-gpl`; LGPL-3 with `--enable-version3`; non-redistributable with `--enable-nonfree` | We need demuxers and a few software decoders, not GPL-only components (x264/x265 encoders, some filters) |
| dav1d | BSD-2-Clause | no constraint |
| libass + FreeType + FriBidi + HarfBuzz | ISC, FTL/GPL-2 dual, LGPL-2.1, MIT | FriBidi is LGPL |
| libsmb2 | LGPL-2.1 | |
| Rust crate ecosystem | overwhelmingly MIT/Apache-2.0 | |

## Decision

1. FramePlayer's own code is **`MIT OR Apache-2.0`** (Rust ecosystem default,
   already set in the workspace `Cargo.toml`). Apache-2.0 adds an explicit
   patent grant; MIT keeps it simple for downstream users.
2. **FFmpeg is configured LGPL-only**: never `--enable-gpl`, never
   `--enable-nonfree`, and not `--enable-version3` (keeps it LGPL-2.1, the most
   compatible option). `docker/Dockerfile.aarch64` encodes this and enables
   only the demuxers/decoders/parsers we use.
3. Because LGPL libraries are linked **statically**, every release must let
   users relink against a modified LGPL library. We satisfy LGPL-2.1 §6 by:
   - publishing the complete corresponding source of FramePlayer (it is open
     source) and the exact versions/configure flags of each LGPL library (the
     Dockerfile), so anyone can rebuild the binary with a modified library; and
   - listing every bundled library, its licence and its source URL in the
     release tarball (`versions/<v>/THIRD-PARTY.md`). TODO: generate it in
     `tools/release.sh` (from the Dockerfile versions plus `cargo about`) when
     the media features are first enabled in release builds.
4. HEVC/AV1/VP9/H.264 decoding uses the Snapdragon hardware decoder where
   possible; software fallbacks (dav1d, FFmpeg's native decoders) are LGPL- or
   BSD-licensed. Codec patent licensing for software HEVC decode in some
   jurisdictions is a distribution question for the owner, not a copyright
   licence question; hardware decode on the device sidesteps most of it.

## Alternative: GPL-3

If the owner prefers GPL-3 (OUTLINE §8 Q1), FFmpeg could be built with
`--enable-gpl --enable-version3`, which unlocks GPL-only components (e.g.
`libx264`/`libx265` if we ever needed encoding, some filters, `librubberband`
for time-stretching). Trade-offs:

- Simplifies compliance (everything GPL, no LGPL relinking story needed),
  but downstream proprietary forks and some library-manager integrations
  become impossible.
- Steam Store distribution of GPL software is allowed; certification is
  unaffected either way.
- We currently need none of the GPL-only FFmpeg parts: our decode path is
  hardware first, our time-stretch is in Rust (`fp-audio`), and we never
  encode. So GPL buys nothing concrete today.

## Consequences

- Contributions are accepted under `MIT OR Apache-2.0` (standard Rust dual
  licence wording in CONTRIBUTING when added).
- CI must fail if anyone adds `--enable-gpl`/`--enable-nonfree` to the build
  image; review checklist item until automated.
- `cargo deny` (or equivalent) should be added to CI to block GPL-only Rust
  crates from entering the dependency graph.
