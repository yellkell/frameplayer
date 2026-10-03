# Embedded test clips

Tiny clips compiled into `frameplayer-probe` with `include_bytes!`
(`src/clips.rs`) so the hardware decoder can be exercised without any
media on the headset. All are MP4 so fp-video's pure-Rust demuxer reads
them. Content is ffmpeg's `testsrc2` pattern, which is synthetic, so the
clips carry no licensing concerns.

| File | Codec | Size | Frames | Purpose |
|---|---|---|---|---|
| `hevc_256x256.mp4` | HEVC Main, 8-bit, `hvc1` | 256×256 | 16 | basic HEVC decode, DMA-BUF import test |
| `h264_256x256.mp4` | H.264 Main | 256×256 | 16 | H.264 decode |
| `vp9_256x256.mp4` | VP9 profile 0 | 256×256 | 16 | VP9 decode |
| `av1_256x256.mp4` | AV1 Main | 256×256 | 16 | AV1 decode |
| `hevc10_3840x1920.mp4` | HEVC Main10, all-intra | 3840×1920 | 4 | 4K 10-bit (P010 / QC10C output) |
| `hevc10_7680x3840.mp4` | HEVC Main10, all-intra | 7680×3840 | 2 | 8K: does the decoder accept the target resolution? |

Generated with the static ffmpeg 7.0.2 from `pip install imageio-ffmpeg`
(libx264, libx265, libvpx-vp9, libaom-av1):

```sh
SRC="-f lavfi -i testsrc2=size=256x256:rate=30 -frames:v 16"
ffmpeg $SRC -pix_fmt yuv420p -c:v libx264 -profile:v main -preset veryslow -crf 30 -g 16 -bf 0 -movflags +faststart h264_256x256.mp4
ffmpeg $SRC -pix_fmt yuv420p -c:v libx265 -tag:v hvc1 -preset veryslow -crf 32 -x265-params "keyint=16:bframes=0:log-level=error" -movflags +faststart hevc_256x256.mp4
ffmpeg $SRC -pix_fmt yuv420p -c:v libvpx-vp9 -b:v 0 -crf 45 -g 16 -row-mt 1 -movflags +faststart vp9_256x256.mp4
ffmpeg $SRC -pix_fmt yuv420p -c:v libaom-av1 -crf 45 -cpu-used 8 -g 16 -movflags +faststart av1_256x256.mp4
ffmpeg -f lavfi -i testsrc2=size=3840x1920:rate=30 -frames:v 4 -pix_fmt yuv420p10le -c:v libx265 -tag:v hvc1 -profile:v main10 -preset medium -crf 40 -x265-params "keyint=1:bframes=0:log-level=error" -movflags +faststart hevc10_3840x1920.mp4
ffmpeg -f lavfi -i testsrc2=size=7680x3840:rate=30 -frames:v 2 -pix_fmt yuv420p10le -c:v libx265 -tag:v hvc1 -profile:v main10 -preset medium -crf 45 -x265-params "keyint=1:bframes=0:log-level=error" -movflags +faststart hevc10_7680x3840.mp4
```

Total ≈ 486 KB. `clips::tests::every_clip_demuxes_with_fp_video` checks
that every clip demuxes with fp-video and has the expected codec, size,
bit depth and frame count.
