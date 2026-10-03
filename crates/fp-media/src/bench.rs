//! Decode-only benchmark (`frameplayer --decode-bench FILE`): runs the
//! player's video decoder and frame conversion without a headset or GPU, and
//! reports the rate plus a checksum per frame for comparing decoders.

use crate::decode::{Decoder, Frame, HwDecode, Packet};
use crate::frame::{FrameConverter, PixelLayout, VideoFrame};
use crate::input::Input;
use crate::{Error, Result};
use fp_core::ByteSource;
use fp_ffmpeg_sys as ff;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct BenchOptions {
    /// Frames to decode (before the seek, if any).
    pub frames: usize,
    pub hw: HwDecode,
    /// Seek here after `frames` and decode `frames_after_seek` more.
    pub seek: Option<f64>,
    pub frames_after_seek: usize,
    /// Checksum these frame numbers (0-based, counted from the start).
    pub checksum: Vec<usize>,
}

impl Default for BenchOptions {
    fn default() -> Self {
        BenchOptions {
            frames: 300,
            hw: HwDecode::Auto,
            seek: None,
            frames_after_seek: 60,
            checksum: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct BenchRun {
    pub frames: usize,
    /// From opening (or the seek) to the first frame.
    pub first_frame_secs: f64,
    /// Rate between the first and the last frame.
    pub fps: f64,
    pub first_pts: Option<f64>,
    pub last_pts: Option<f64>,
}

#[derive(Clone, Debug, Default)]
pub struct BenchReport {
    pub decoder: String,
    pub hardware: bool,
    pub width: u32,
    pub height: u32,
    pub layout: Option<PixelLayout>,
    pub run: BenchRun,
    pub after_seek: Option<BenchRun>,
    /// (frame number, pts, FNV-1a of the Y, U and V samples).
    pub checksums: Vec<(usize, f64, u64)>,
}

/// FNV-1a over Y, then U, then V samples, so NV12 and I420 decodes of the
/// same picture hash alike.
pub fn yuv_checksum(f: &VideoFrame) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |b: u8| {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    };
    let planes = f.packed_planes();
    match f.layout {
        PixelLayout::Nv12 | PixelLayout::P010 => {
            let bps = f.layout.bytes_per_sample() as usize;
            planes[0].iter().for_each(|&b| eat(b));
            for offset in [0, bps] {
                for pair in planes[1].chunks(2 * bps) {
                    pair[offset..offset + bps].iter().for_each(|&b| eat(b));
                }
            }
        }
        _ => planes.iter().flatten().for_each(|&b| eat(b)),
    }
    h
}

struct Session {
    input: Input,
    vi: usize,
    dec: Decoder,
    conv: FrameConverter,
    pkt: Packet,
    have_pkt: bool,
    eof: bool,
    eof_sent: bool,
    frame: Frame,
}

impl Session {
    /// Decodes up to `n` frames, calling `each` with the frame and its number.
    fn run(&mut self, n: usize, mut each: impl FnMut(usize, &VideoFrame)) -> Result<BenchRun> {
        let start = Instant::now();
        let mut run = BenchRun::default();
        let mut first_at = None;
        let mut last_at = start;
        while run.frames < n {
            if !self.have_pkt && !self.eof {
                if self.input.read(self.pkt.as_ptr())? {
                    if self.pkt.stream_index() != self.vi {
                        // SAFETY: valid packet.
                        unsafe { ff::av_packet_unref(self.pkt.as_ptr()) };
                        continue;
                    }
                    self.have_pkt = true;
                } else {
                    self.eof = true;
                }
            }
            if self.have_pkt {
                if self.dec.send(self.pkt.as_ptr())? {
                    // SAFETY: valid packet.
                    unsafe { ff::av_packet_unref(self.pkt.as_ptr()) };
                    self.have_pkt = false;
                }
            } else if self.eof && !self.eof_sent && self.dec.send(std::ptr::null())? {
                self.eof_sent = true;
            }
            loop {
                match self.dec.receive(&mut self.frame)? {
                    Some(true) => {
                        let pts = self.dec.frame_time(&self.frame);
                        let vf =
                            self.conv
                                .convert(self.frame.take_raw(), pts.unwrap_or(0.0), 0.0, 0)?;
                        let now = Instant::now();
                        if first_at.is_none() {
                            first_at = Some(now);
                            run.first_frame_secs = (now - start).as_secs_f64();
                            run.first_pts = pts;
                        }
                        last_at = now;
                        run.last_pts = pts;
                        each(run.frames, &vf);
                        run.frames += 1;
                        if run.frames >= n {
                            break;
                        }
                    }
                    Some(false) => {
                        n_done(&mut run, first_at, last_at);
                        return Ok(run);
                    }
                    None => break,
                }
            }
        }
        n_done(&mut run, first_at, last_at);
        Ok(run)
    }
}

fn n_done(run: &mut BenchRun, first: Option<Instant>, last: Instant) {
    if let Some(f) = first {
        let secs = (last - f).as_secs_f64();
        if run.frames > 1 && secs > 0.0 {
            run.fps = (run.frames - 1) as f64 / secs;
        }
    }
}

/// Decodes `src` as the player would and measures it.
pub fn decode_bench(
    src: Arc<dyn ByteSource>,
    name: &str,
    opts: &BenchOptions,
) -> Result<BenchReport> {
    let input = Input::open(src, name, Arc::new(AtomicBool::new(false)))?;
    let vi = input
        .best_stream(ff::AVMEDIA_TYPE_VIDEO)
        .ok_or(Error::NoStream("video"))?;
    let dec = Decoder::open(input.streams()[vi], opts.hw)?;
    let mut report = BenchReport {
        decoder: dec.name.clone(),
        hardware: dec.hardware,
        ..Default::default()
    };
    let mut s = Session {
        input,
        vi,
        dec,
        conv: FrameConverter::default(),
        pkt: Packet::new(),
        have_pkt: false,
        eof: false,
        eof_sent: false,
        frame: Frame::new(),
    };
    let mut sums = Vec::new();
    let mut dims = None;
    report.run = s.run(opts.frames, |i, f| {
        dims.get_or_insert((f.width, f.height, f.layout));
        if opts.checksum.contains(&i) {
            sums.push((i, f.pts, yuv_checksum(f)));
        }
    })?;
    if let Some(t) = opts.seek {
        s.input.seek(t)?;
        s.dec.flush();
        if s.have_pkt {
            // SAFETY: valid packet.
            unsafe { ff::av_packet_unref(s.pkt.as_ptr()) };
            s.have_pkt = false;
        }
        s.eof = false;
        s.eof_sent = false;
        report.after_seek = Some(s.run(opts.frames_after_seek, |_, _| {})?);
    }
    // The decoder may have fallen back mid-run.
    report.decoder = s.dec.name.clone();
    report.hardware = s.dec.hardware;
    report.checksums = sums;
    if let Some((w, h, l)) = dims {
        (report.width, report.height, report.layout) = (w, h, Some(l));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fp_core::source::FileSource;

    fn open(name: &str) -> Arc<dyn ByteSource> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data")
            .join(name);
        Arc::new(FileSource::open(&path).unwrap())
    }

    #[test]
    fn bench_decodes_seeks_and_checksums() {
        let opts = BenchOptions {
            frames: 20,
            seek: Some(1.0),
            frames_after_seek: 5,
            checksum: vec![0, 10],
            ..Default::default()
        };
        let r = decode_bench(open("h264_aac_180_LR.mp4"), "h264_aac_180_LR.mp4", &opts).unwrap();
        assert_eq!(r.run.frames, 20);
        assert_eq!((r.width, r.height), (640, 320));
        assert_eq!(r.checksums.len(), 2);
        assert_ne!(r.checksums[0].2, r.checksums[1].2);
        let after = r.after_seek.unwrap();
        assert_eq!(after.frames, 5);
        assert!(after.first_pts.unwrap() <= 1.0 + 1e-6);
        // Deterministic: the same frames hash the same.
        let again = decode_bench(open("h264_aac_180_LR.mp4"), "x.mp4", &opts).unwrap();
        assert_eq!(again.checksums, r.checksums);
    }
}
