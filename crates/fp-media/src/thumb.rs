//! Probing and single-frame grabs for the media library.

use crate::decode::{Decoder, Frame, HwDecode, Packet};
use crate::info::{MediaInfo, read_info};
use crate::input::Input;
use crate::{Error, Result};
use fp_core::ByteSource;
use fp_ffmpeg_sys as ff;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Which part of the frame to keep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Crop {
    Full,
    /// Left half (side-by-side stereo).
    LeftHalf,
    /// Top half (top/bottom stereo).
    TopHalf,
    /// Left half, stretched to the whole frame's shape (half side-by-side
    /// movies, whose eyes are squeezed to half width).
    LeftHalfUnsqueezed,
    /// Top half, stretched to the whole frame's shape (half over/under).
    TopHalfUnsqueezed,
}

/// An RGBA image.
#[derive(Clone, Debug, PartialEq)]
pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Decodes the first video frame (or a still image: PNG, JPEG) at full size.
/// Used for photo viewing and by renderer tests.
pub fn decode_first_frame(src: Arc<dyn ByteSource>, name: &str) -> Result<crate::VideoFrame> {
    let mut input = Input::open(src, name, Arc::new(AtomicBool::new(false)))?;
    let vi = input
        .best_stream(ff::AVMEDIA_TYPE_VIDEO)
        .ok_or(Error::NoStream("video"))?;
    let mut dec = Decoder::open(input.streams()[vi], HwDecode::Off)?;
    let pkt = Packet::new();
    let mut frame = Frame::new();
    let mut conv = crate::frame::FrameConverter::default();
    let mut eof = false;
    for _ in 0..10_000 {
        if !eof {
            if input.read(pkt.as_ptr())? {
                if pkt.stream_index() == vi {
                    dec.send(pkt.as_ptr())?;
                }
                // SAFETY: valid packet.
                unsafe { ff::av_packet_unref(pkt.as_ptr()) };
            } else {
                eof = true;
                dec.send(std::ptr::null())?;
            }
        }
        match dec.receive(&mut frame)? {
            Some(true) => {
                let t = dec.frame_time(&frame).unwrap_or(0.0);
                return conv.convert(frame.take_raw(), t, 0.0, 0);
            }
            Some(false) => break,
            None => {}
        }
    }
    Err(Error::Unsupported("no frame decoded".into()))
}

/// Reads stream information without decoding. Setting `interrupt` makes
/// reads in progress fail, so a probe of a slow network file can be cut
/// short.
pub fn probe(
    src: Arc<dyn ByteSource>,
    name: &str,
    interrupt: Arc<AtomicBool>,
) -> Result<MediaInfo> {
    let input = Input::open(src, name, interrupt)?;
    Ok(read_info(&input))
}

/// Decodes the first frame at or after `at` seconds and scales it to fit
/// within `max_w` x `max_h`, keeping the aspect ratio of the cropped area
/// (of the whole frame for the unsqueezed crops).
/// Setting `interrupt` stops it at the next read.
pub fn grab(
    src: Arc<dyn ByteSource>,
    name: &str,
    at: f64,
    max_w: u32,
    max_h: u32,
    crop: Crop,
    interrupt: Arc<AtomicBool>,
) -> Result<Rgba> {
    let mut input = Input::open(src, name, interrupt)?;
    let vi = input
        .best_stream(ff::AVMEDIA_TYPE_VIDEO)
        .ok_or(Error::NoStream("video"))?;
    // Software decoding: the hardware decoder is busy with playback.
    let mut dec = Decoder::open(input.streams()[vi], HwDecode::Off)?;
    if at > 0.0 {
        input.seek(at)?;
    }
    let pkt = Packet::new();
    let mut frame = Frame::new();
    let mut eof = false;
    let mut fallback: Option<Frame> = None;
    for _ in 0..2000 {
        if !eof {
            if input.read(pkt.as_ptr())? {
                if pkt.stream_index() == vi {
                    dec.send(pkt.as_ptr())?;
                }
                // SAFETY: valid packet.
                unsafe { ff::av_packet_unref(pkt.as_ptr()) };
            } else {
                eof = true;
                dec.send(std::ptr::null())?;
            }
        }
        match dec.receive(&mut frame)? {
            Some(true) => {
                let t = dec.frame_time(&frame).unwrap_or(at);
                if t + 0.05 >= at {
                    return scale(&frame, max_w, max_h, crop);
                }
                let keep = Frame::new();
                // SAFETY: both frames valid; keep the latest as a fallback.
                unsafe { ff::av_frame_move_ref(keep.as_ptr(), frame.as_ptr()) };
                fallback = Some(keep);
            }
            Some(false) => break,
            None => {}
        }
    }
    match fallback {
        Some(f) => scale(&f, max_w, max_h, crop),
        None => Err(Error::Unsupported("no frame decoded".into())),
    }
}

fn scale(frame: &Frame, max_w: u32, max_h: u32, crop: Crop) -> Result<Rgba> {
    // SAFETY: frame is decoded; swscale reads only the cropped region by
    // limiting the source width/height (pointers start at the top-left).
    unsafe {
        let f = &*frame.as_ptr();
        let (sw, sh) = match crop {
            Crop::Full => (f.width, f.height),
            Crop::LeftHalf | Crop::LeftHalfUnsqueezed => (f.width / 2, f.height),
            Crop::TopHalf | Crop::TopHalfUnsqueezed => (f.width, f.height / 2),
        };
        if sw <= 0 || sh <= 0 {
            return Err(Error::Unsupported("empty frame".into()));
        }
        // The shape the picture is shown at.
        let (vw, vh) = match crop {
            Crop::LeftHalfUnsqueezed | Crop::TopHalfUnsqueezed => (f.width, f.height),
            _ => (sw, sh),
        };
        let s = (max_w as f64 / vw as f64)
            .min(max_h as f64 / vh as f64)
            .min(1.0);
        let dw = ((vw as f64 * s).round() as i32).max(2) & !1;
        let dh = ((vh as f64 * s).round() as i32).max(2) & !1;
        let ctx = ff::sws_getContext(
            sw,
            sh,
            f.format,
            dw,
            dh,
            ff::AV_PIX_FMT_RGBA,
            (ff::SWS_AREA | ff::SWS_ACCURATE_RND | ff::SWS_FULL_CHR_H_INT) as i32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null(),
        );
        if ctx.is_null() {
            return Err(Error::Unsupported(format!(
                "cannot scale pixel format {}",
                f.format
            )));
        }
        let mut pixels = vec![0u8; (dw * dh * 4) as usize];
        let dst = [
            pixels.as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ];
        let dst_stride = [dw * 4, 0, 0, 0];
        ff::sws_scale(
            ctx,
            f.data.as_ptr() as *const *const u8,
            f.linesize.as_ptr(),
            0,
            sh,
            dst.as_ptr(),
            dst_stride.as_ptr(),
        );
        ff::sws_freeContext(ctx);
        Ok(Rgba {
            width: dw as u32,
            height: dh as u32,
            pixels,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fp_core::source::FileSource;

    fn src(name: &str) -> Arc<dyn ByteSource> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data")
            .join(name);
        Arc::new(FileSource::open(&path).unwrap())
    }

    fn no_interrupt() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    #[test]
    fn grabs_left_eye_thumbnail() {
        let img = grab(
            src("h264_aac_180_LR.mp4"),
            "x.mp4",
            1.0,
            160,
            160,
            Crop::LeftHalf,
            no_interrupt(),
        )
        .unwrap();
        assert_eq!((img.width, img.height), (160, 160));
        assert_eq!(img.pixels.len(), 160 * 160 * 4);
        let distinct: std::collections::HashSet<_> = img
            .pixels
            .chunks(4)
            .map(|p| (p[0] / 32, p[1] / 32, p[2] / 32))
            .collect();
        assert!(distinct.len() > 10, "image looks blank");
    }

    #[test]
    fn unsqueezed_eye_takes_the_frame_shape() {
        // 640x320: each eye is 320x320, shown at 2:1 when unsqueezed.
        let img = grab(
            src("h264_aac_180_LR.mp4"),
            "x.mp4",
            1.0,
            160,
            160,
            Crop::LeftHalfUnsqueezed,
            no_interrupt(),
        )
        .unwrap();
        assert_eq!((img.width, img.height), (160, 80));
    }

    #[test]
    fn decodes_first_frame_full_size() {
        let f = decode_first_frame(src("h264_aac_180_LR.mp4"), "x.mp4").unwrap();
        assert_eq!((f.width, f.height), (640, 320));
    }

    #[test]
    fn grabs_10bit_top_half_and_probes() {
        let img = grab(
            src("hevc10_tb.mkv"),
            "x.mkv",
            0.5,
            256,
            256,
            Crop::TopHalf,
            no_interrupt(),
        )
        .unwrap();
        assert_eq!((img.width, img.height), (256, 128));
        let info = probe(src("av1_opus.webm"), "x.webm", no_interrupt()).unwrap();
        assert_eq!(info.video_stream().unwrap().codec, "av1");
    }

    #[test]
    fn interrupted_grab_fails() {
        let stop = Arc::new(AtomicBool::new(true));
        let r = grab(
            src("h264_aac_180_LR.mp4"),
            "x.mp4",
            1.0,
            160,
            160,
            Crop::Full,
            stop,
        );
        assert!(r.is_err());
    }
}
