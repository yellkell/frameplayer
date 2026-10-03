# Software-decode fixtures

Used by `crates/video/tests/sw_decode.rs` (features `ffmpeg` + `dav1d`) and
the headless end-to-end check. Each clip is 128×72, 10 frames at 10 fps,
solid RGB `0x2060C0`, which every encoder below stores as BT.601 limited
range Y=91 U=180 V=93 (±1). Generated with the Ubuntu 24.04 `ffmpeg` 6.1.1
CLI (any build with these encoders works); our own LGPL build has no
encoders on purpose.

```sh
C="color=c=0x2060C0:s=128x72:r=10:d=1"
FF="ffmpeg -y -f lavfi -i $C"
$FF -c:v libx265 -preset ultrafast -pix_fmt yuv420p     -tag:v hvc1 -an hevc.mp4
$FF -c:v libx265 -preset ultrafast -pix_fmt yuv420p10le -tag:v hvc1 -an hevc_main10.mp4
$FF -c:v libx264 -preset ultrafast -pix_fmt yuv420p -an h264.mp4
$FF -c:v libx264 -preset ultrafast -pix_fmt yuv420p -an -f mpegts h264.ts
ffmpeg -y -f lavfi -i $C -f lavfi -i "sine=frequency=440:sample_rate=48000:duration=1" \
  -c:v libvpx-vp9 -deadline realtime -cpu-used 8 -pix_fmt yuv420p \
  -c:a libopus -b:a 24k -ac 2 vp9_opus.webm
$FF -c:v libaom-av1 -cpu-used 8 -usage realtime -pix_fmt yuv420p -an av1.mp4
```

| File | Container / demuxer | Codec | Path exercised |
|---|---|---|---|
| `hevc.mp4` | MP4, pure Rust | HEVC Main | libavcodec `hevc` |
| `hevc_main10.mp4` | MP4, pure Rust | HEVC Main 10 | libavcodec `hevc`, 10-bit output |
| `h264.mp4` | MP4, pure Rust | H.264 High | libavcodec `h264` |
| `h264.ts` | MPEG-TS, libavformat | H.264 | `FfmpegDemuxer` + libavcodec |
| `vp9_opus.webm` | WebM, pure Rust | VP9 + Opus | libavcodec `vp9`, `opus` |
| `av1.mp4` | MP4, pure Rust | AV1 | dav1d crate; libavcodec `libdav1d` |
