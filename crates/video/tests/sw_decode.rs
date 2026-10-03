//! Software decode path against real bitstreams (features `ffmpeg` +
//! `dav1d`; build the libraries with `tools/build-codecs.sh --target host`
//! and `source third_party/out/x86_64-unknown-linux-gnu/env.sh`).
//!
//! Every fixture is a 128×72, 10-frame, 10 fps solid colour clip (RGB
//! 0x2060C0 → BT.601 limited Y≈91 U≈180 V≈93); see tests/fixtures/README.md.
#![cfg(all(feature = "ffmpeg", feature = "dav1d"))]

use fp_video::audio_decode::open_audio_decoder;
use fp_video::decode::open_software;
use fp_video::{
    open_demuxer, select_decoder, CpuFrame, DecodedFrame, DecoderOptions, DecoderPath,
    DecoderRequest, Demuxer, PixelFormat, TrackKind, VideoDecoder,
};
use std::path::PathBuf;

const W: u32 = 128;
const H: u32 = 72;
const FRAMES: usize = 10;
const YUV: [i32; 3] = [91, 180, 93];
const TOLERANCE: i32 = 3;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn demux(name: &str) -> Box<dyn Demuxer> {
    let f = std::fs::File::open(fixture(name)).unwrap();
    open_demuxer(Box::new(f)).unwrap()
}

fn software_only() -> DecoderOptions {
    DecoderOptions {
        allow_hardware: false,
        ..Default::default()
    }
}

/// Feed every packet of the default video track through `dec` using the
/// engine's protocol (send until full, receive until empty, drain).
fn decode_all(d: &mut dyn Demuxer, dec: &mut dyn VideoDecoder) -> Vec<CpuFrame> {
    let track = d.default_track(TrackKind::Video).unwrap().id;
    let mut out = Vec::new();
    let take = |dec: &mut dyn VideoDecoder, out: &mut Vec<CpuFrame>| {
        while let Some(f) = dec.receive_frame().unwrap() {
            match f {
                DecodedFrame::Cpu(c) => out.push(c),
                DecodedFrame::DmaBuf(_) => panic!("software decoder returned a DMA-BUF"),
            }
        }
    };
    while let Some(p) = d.read_packet().unwrap() {
        if p.track != track {
            continue;
        }
        loop {
            if dec.send_packet(&p).unwrap() {
                break;
            }
            take(dec, &mut out);
            dec.wait(std::time::Duration::from_millis(1));
        }
        take(dec, &mut out);
    }
    dec.drain().unwrap();
    for _ in 0..10_000 {
        take(dec, &mut out);
        if dec.is_drained() {
            break;
        }
        dec.wait(std::time::Duration::from_millis(1));
    }
    assert!(dec.is_drained(), "decoder never drained");
    out
}

/// Sample (Y, U, V) at (x, y), scaled to 8 bits.
fn yuv_at(f: &CpuFrame, x: usize, y: usize) -> [i32; 3] {
    let (cx, cy) = (x / 2, y / 2);
    let u16_at = |plane: &[u8], stride: usize, x: usize, y: usize| {
        let i = y * stride + 2 * x;
        u16::from_le_bytes([plane[i], plane[i + 1]]) as i32
    };
    match f.format {
        PixelFormat::I420 => [
            f.planes[0][y * f.strides[0] + x] as i32,
            f.planes[1][cy * f.strides[1] + cx] as i32,
            f.planes[2][cy * f.strides[2] + cx] as i32,
        ],
        PixelFormat::Nv12 => [
            f.planes[0][y * f.strides[0] + x] as i32,
            f.planes[1][cy * f.strides[1] + 2 * cx] as i32,
            f.planes[1][cy * f.strides[1] + 2 * cx + 1] as i32,
        ],
        PixelFormat::I420P10 => [
            u16_at(&f.planes[0], f.strides[0], x, y) >> 2,
            u16_at(&f.planes[1], f.strides[1], cx, cy) >> 2,
            u16_at(&f.planes[2], f.strides[2], cx, cy) >> 2,
        ],
        PixelFormat::P010 => {
            let s = f.strides[1];
            [
                u16_at(&f.planes[0], f.strides[0], x, y) >> 8,
                u16_at(&f.planes[1], s, 2 * cx, cy) >> 8,
                u16_at(&f.planes[1], s, 2 * cx + 1, cy) >> 8,
            ]
        }
    }
}

fn check_frames(frames: &[CpuFrame], depth: u8) {
    assert_eq!(frames.len(), FRAMES, "frame count");
    let mut last = None;
    for f in frames {
        assert_eq!((f.width, f.height), (W, H));
        assert_eq!(f.format.bit_depth(), depth, "{:?}", f.format);
        // Presentation order, 100 ms apart.
        if let Some(l) = last {
            assert!(f.pts > l, "pts not increasing: {:?} after {:?}", f.pts, l);
        }
        last = Some(f.pts);
        for (x, y) in [(0, 0), (W as usize - 1, 0), (64, 36), (0, H as usize - 1)] {
            let got = yuv_at(f, x, y);
            for c in 0..3 {
                assert!(
                    (got[c] - YUV[c]).abs() <= TOLERANCE,
                    "pixel ({x},{y}) = {got:?}, want {YUV:?} ±{TOLERANCE} ({:?})",
                    f.format
                );
            }
        }
    }
    let span = frames.last().unwrap().pts - frames[0].pts;
    assert!(
        (span.as_secs_f64() - 0.9).abs() < 0.011,
        "10 fps timestamps span {span}"
    );
}

fn run_select(name: &str, want_lib: &str, depth: u8) {
    let mut d = demux(name);
    let t = d.default_track(TrackKind::Video).unwrap().clone();
    let req = DecoderRequest::from_track(&t);
    assert_eq!((req.width, req.height), (W, H));
    let mut sel = select_decoder(&req, &software_only()).unwrap();
    assert_eq!(
        sel.path,
        DecoderPath::Software {
            library: want_lib.into()
        }
    );
    assert!(!sel.warnings.is_empty(), "software decode must warn");
    let frames = decode_all(&mut *d, &mut *sel.decoder);
    check_frames(&frames, depth);
}

#[test]
fn hevc_main_mp4() {
    run_select("hevc.mp4", "ffmpeg", 8);
}

#[test]
fn hevc_main10_mp4() {
    run_select("hevc_main10.mp4", "ffmpeg", 10);
}

#[test]
fn h264_mp4() {
    run_select("h264.mp4", "ffmpeg", 8);
}

#[test]
fn vp9_webm() {
    run_select("vp9_opus.webm", "ffmpeg", 8);
}

#[test]
fn av1_mp4_dav1d() {
    run_select("av1.mp4", "dav1d", 8);
}

#[test]
fn av1_via_libavcodec_libdav1d() {
    let mut d = demux("av1.mp4");
    let t = d.default_track(TrackKind::Video).unwrap().clone();
    let mut dec = open_software("ffmpeg", &DecoderRequest::from_track(&t), 0).unwrap();
    let frames = decode_all(&mut *d, &mut *dec);
    check_frames(&frames, 8);
}

#[test]
fn h264_mpegts_via_libavformat() {
    let mut d = demux("h264.ts");
    assert_eq!(d.format_name(), "mpegts");
    let t = d.default_track(TrackKind::Video).unwrap().clone();
    let mut sel = select_decoder(&DecoderRequest::from_track(&t), &software_only()).unwrap();
    let frames = decode_all(&mut *d, &mut *sel.decoder);
    check_frames(&frames, 8);
}

#[test]
fn default_selection_falls_back_to_software_without_v4l2() {
    // No usable V4L2 decoder in CI: the default policy must still decode.
    if !fp_video::decode::v4l2::enumerate_devices().is_empty() {
        eprintln!("skipping: V4L2 decoder devices present");
        return;
    }
    let mut d = demux("hevc.mp4");
    let t = d.default_track(TrackKind::Video).unwrap().clone();
    let mut sel =
        select_decoder(&DecoderRequest::from_track(&t), &DecoderOptions::default()).unwrap();
    assert!(matches!(sel.path, DecoderPath::Software { .. }));
    assert!(sel.rejected.iter().any(|r| r.starts_with("hardware")));
    check_frames(&decode_all(&mut *d, &mut *sel.decoder), 8);
}

#[test]
fn opus_audio_via_libavcodec() {
    let mut d = demux("vp9_opus.webm");
    let t = d.default_track(TrackKind::Audio).unwrap().clone();
    let mut dec = open_audio_decoder(&t).unwrap();
    let mut samples = Vec::new();
    let mut channels = 0;
    while let Some(p) = d.read_packet().unwrap() {
        if p.track != t.id {
            continue;
        }
        if let Some(f) = dec.decode(&p).unwrap() {
            assert_eq!(f.sample_rate, 48_000);
            channels = f.channels;
            samples.extend(f.samples);
        }
    }
    assert_eq!(channels, 2);
    let frames = samples.len() / 2;
    assert!(
        (40_000..=50_000).contains(&frames),
        "~1 s of audio, got {frames} frames"
    );
    let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt();
    assert!(
        rms > 0.05,
        "440 Hz sine decoded to near silence (rms {rms})"
    );
}
