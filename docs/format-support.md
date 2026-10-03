# Format support matrix

What FramePlayer plays and **which code path handles it**. "HW" is the
Snapdragon 8 Gen 3 video decoder (V4L2 stateful M2M, `fp_video::decode::v4l2`),
"SW" is a software decoder behind a cargo feature (`dav1d`, `ffmpeg`;
`fp-app/sw-decode` enables both). Release builds enable `sw-decode` and ship
LGPL-only FFmpeg as shared libraries next to the binary, built by
`tools/build-codecs.sh` (see [Bundled libraries](#bundled-libraries) and
ADR 0002); the default developer build needs no C libraries and has HW +
pure-Rust paths only.

Items depending on unconfirmed device behaviour are marked [verify] and
tracked in [platform-notes.md](platform-notes.md).

## Containers

| Container | Extensions | Demuxer | Notes |
|---|---|---|---|
| MP4 / MOV (ISO-BMFF, QuickTime) | `.mp4 .m4v .mov` | pure Rust `fp_video::demux::mp4` | progressive + fragmented; spherical v1 (XML uuid) and v2 (`st3d`/`sv3d`) boxes; `SA3D` ambisonic audio |
| Matroska / WebM | `.mkv .webm .mk3d` | pure Rust `fp_video::demux::mkv` | Cues seeking, lacing, header stripping; `StereoMode`, `Projection` elements |
| MPEG-TS | `.ts .m2ts .mts` | libavformat `mpegts` (`ffmpeg` feature) | |
| MPEG-PS / VOB | `.mpg .vob` | libavformat `mpegps` (`ffmpeg` feature) | best effort |
| AVI, FLV, Ogg | `.avi .flv .ogv` | libavformat `avi`/`flv`/`ogg` (`ffmpeg` feature) | best effort; no other libavformat demuxers are compiled in |
| HLS / DASH | `.m3u8 .mpd` | `fp_sources::{hls,dash}` segment fetchers → MP4/TS demuxer | network sources |

## Video codecs

| Codec | Profiles / depth | Primary path | Fallback | Limit |
|---|---|---|---|---|
| HEVC / H.265 | Main, Main 10 | HW (V4L2) → DMA-BUF NV12/P010 → Vulkan import | SW libavcodec `hevc` | HW: up to 8K60 per SoC spec [verify exposed limits]; SW capped at 4K |
| AV1 | Main 8/10-bit | HW (V4L2) [verify AV1 exposed on Frame] | SW dav1d (`dav1d` feature), then libavcodec `libdav1d` | SW capped at 4K |
| VP9 | Profile 0, 2 (10-bit) | HW (V4L2) | SW libavcodec `vp9` | SW capped at 4K |
| VP8 | | HW (V4L2) if exposed | SW libavcodec `vp8` | SW capped at 4K |
| H.264 / AVC | High, up to 8-bit | HW (V4L2) | SW libavcodec `h264` | 4K (H.264 level limits) |
| Vulkan Video (`VK_KHR_video_decode_h265/av1`) | | alternative HW path if Turnip exposes it on the Frame [verify] | | |

The decoder selector (`fp_video::decode::select_decoder`) tries HW first,
then SW, and reports which path it chose and why (shown in the player's info
panel). Software decode above 4K is refused with an explanation instead of
stuttering. When a software decoder could take over (compiled in, ≤ 4K), the
hardware decoder runs inside `decode::fallback::FallbackDecoder`: if V4L2
fails after opening (`STREAMON`/CAPTURE setup errors, decode errors, or no
frame within 3 s / drained without output) before it has delivered 8
frames, the buffered packets since the last keyframe are replayed into the
software decoder, playback continues without repeating frames, and the
player logs a warning and re-emits `DecoderSelected` with the new path.

## HDR / colour

| Signal | Detection | Handling |
|---|---|---|
| SDR BT.709 | default | YUV→RGB compute pass |
| HDR10 (PQ, ST 2084) + mastering/CLL metadata | container colour info / SEI | on-GPU tone map to the SDR LCD, per-video exposure |
| HDR10+ | dynamic metadata ignored, treated as HDR10 | |
| HLG (ARIB STD-B67) | colour info | HLG→SDR curve |
| Dolby Vision | base layer only (profile 8.x as HDR10/HLG; profile 5 unsupported) | |

## Projections

| Projection | Filename tokens (fp-core detector) | Container metadata | Render path |
|---|---|---|---|
| Flat screen (size, distance, curvature) | `flat`, `screen`, movie 3D tokens (`3DH`, `HSBS`, `HOU`) | none | `fp-gfx` screen mesh |
| Equirect 180° | `180`, `vr180`, `180x180`, `dome`; a bare `_LR`/`_TB` implies 180 | sv3d equirect with 180° bounds | sphere-segment mesh |
| Equirect 360° | `360`, `vr360`, `360x180`, `mono360` | sv3d equirect, spherical v1 | full sphere mesh |
| Fisheye 180/190/200/220 | `fisheye`, `fisheye190`, `fisheye200`, `fisheye220`, `f180`, `vrca220` | none | fisheye mesh, per-lens FOV + centre offset |
| Canon RF 5.2 mm dual fisheye | `rf52` | none | fisheye preset |
| MKX200 / MKX220 | `mkx200`, `mkx220` | none | fisheye presets |
| EAC (YouTube equi-angular cubemap) | `eac`, `eac360` | sv3d `cbmp`/EAC | cube mesh |
| Custom mesh (OBJ) | per-file override only | sv3d `mshp` (future) | user mesh |

Per-file overrides persist in the library database and always win over
detection.

## Stereo layouts

| Layout | Tokens | Metadata | Notes |
|---|---|---|---|
| Mono | `mono`, `2d` | `st3d` mono, MKV `StereoMode=mono` | |
| Side-by-side (L left) | `lr`, `sbs`, `hsbs`, `fsbs`, `3dh`, `sidebyside` | `st3d` left-right, MKV `StereoMode=left_right` | |
| Side-by-side, eyes swapped | `rl` | MKV `StereoMode=right_left` | sets swap-eyes |
| Over-under (L top) | `tb`, `ou`, `hou`, `fou`, `htab`, `tab`, `3dv`, `overunder`, `topbottom` | `st3d` top-bottom, MKV `top_bottom` | |
| Over-under, swapped | `bt` | MKV `bottom_top` | |
| Per-eye crop | override only | | trims letterboxed SBS/OU |

2D-to-3D conversion is intentionally not offered.

## Audio

| Format | Decoder | Output |
|---|---|---|
| AAC-LC | pure Rust (symphonia, default feature `aac`) | |
| PCM (MP4 `sowt`/`twos`/`lpcm`…, Matroska `A_PCM`) | pure Rust | |
| Opus, Vorbis, FLAC, MP3, MP2, AC-3, E-AC-3, HE-AAC / multichannel AAC | libavcodec (`ffmpeg` feature) | |
| Stereo | | direct |
| 5.1 / 7.1 | | downmix to stereo (`fp_audio::downmix`), or binaural virtual speakers |
| Ambisonics FOA / HOA (AmbiX ACN/SN3D, FuMa) | `SA3D` box or track title | head-tracked rotation + binaural HRTF decode (`fp_audio::ambisonics`) |

Output goes to PipeWire through cpal/ALSA (`fp-audio` `cpal` feature). Audio
is the master clock; pitch-corrected speed 0.25×–4×.

## Subtitles

| Format | External file | Embedded | Parser / renderer |
|---|---|---|---|
| SubRip (SRT) | `.srt` | MKV, MP4 (`mov_text` via ffmpeg) | pure Rust `fp_video::subtitle::srt` → VR text renderer |
| WebVTT | `.vtt` | MKV/WebM | pure Rust `fp_video::subtitle::webvtt` |
| ASS / SSA | `.ass .ssa` | MKV | pure Rust `fp_video::subtitle::ass` (styled spans, alignment; unsupported override tags stripped); libass for full typesetting is an optional later path |
| PGS (Blu-ray bitmap) | `.sup` | MKV | pure Rust `fp_video::subtitle::pgs` → RGBA bitmaps |

Subtitles render on their own layer at an adjustable depth with automatic
per-eye disparity (`fp_video::subtitle::stereo`).

## Interactive scripts

| Format | Discovery | Handler |
|---|---|---|
| Funscript (single and multi-axis) | same dir, `Interactive/` folder, URL from DeoVR feed | `fp-haptics` |

## Bundled libraries

`tools/build-codecs.sh` builds these from pinned, sha256-checked tarballs
(falling back to a commit-verified git checkout) into
`third_party/out/<target>/`; release tarballs ship `dist-lib/*` as
`versions/<v>/lib/` and the binary carries `RUNPATH $ORIGIN/../lib`.

| Library | Version | Licence | Linkage | aarch64 size (stripped) |
|---|---|---|---|---|
| FFmpeg libavcodec / libavformat / libavutil / libswscale / libswresample | 7.1.1 (`.so.61/.61/.59/.8/.5`) | LGPL-2.1-or-later (no `--enable-gpl/nonfree/version3`; the script refuses otherwise) | shared, in `lib/` | 6.7 MB total |
| dav1d | 1.5.1 | BSD-2-Clause | static (in libavcodec and in `frameplayer`) | — |

FFmpeg is configured `--disable-everything --disable-autodetect` and enables
only: decoders `h264 hevc vp8 vp9 libdav1d aac aac_latm ac3 eac3 opus vorbis
flac mp3 mp3float mp2 mp2float pcm_*` (common variants), the matching
parsers, demuxers `mpegts mpegps avi flv ogg`, plus swscale/swresample. No
encoders, muxers, protocols, filters, devices, network or hwaccels;
subtitles are parsed in Rust. NEON (and dotprod/i8mm with runtime
detection) on aarch64, nasm SIMD on x86_64, pthreads on.

glibc floor: the aarch64 libraries and `frameplayer` are compiled and linked
against a glibc 2.31 sysroot (Ubuntu 20.04 arm64 packages, the Steam Linux
Runtime 3 baseline), so the binary needs at most `GLIBC_2.30` symbols
(checked: `objdump -T`) and runs on any SteamOS 3.x (3.8 ships glibc 2.41).
[verify] the glibc of SteamOS on the Frame itself.

## Test coverage

`tools/gen-test-videos.sh` produces one clip per projection × stereo × codec
with matching filename tokens, plus HDR10/HLG, 5.1 and FOA ambiX audio,
embedded SRT+ASS and sidecar SRT/VTT/ASS, a Matroska `StereoMode` file with
no filename tokens, and 60/59.94/23.976 fps pacing clips. Every row of this
document should have at least one clip there; add new rows to both.

The software path has committed fixtures (`crates/video/tests/fixtures/`,
HEVC 8/10-bit, H.264 in MP4 and MPEG-TS, VP9 + Opus WebM, AV1) decoded by
`crates/video/tests/sw_decode.rs` with dimension, frame-count, timestamp and
pixel-value checks (`cargo test -p fp-video --features ffmpeg,dav1d` after
sourcing `third_party/out/<host>/env.sh`).
