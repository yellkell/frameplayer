//! libavcodec software video decoder (feature `ffmpeg`).

use super::{CpuFrame, DecodedFrame, DecoderPath, DecoderRequest, PixelFormat, VideoDecoder};
use crate::error::{Result, VideoError};
use crate::packet::{CodecId, Packet};
use ffmpeg_next as ff;
use fp_core::MediaTime;

pub struct FfmpegVideoDecoder {
    dec: ff::decoder::Video,
    frame: ff::frame::Video,
    scaler: Option<(ff::software::scaling::Context, ff::format::Pixel, u32, u32)>,
    draining: bool,
    drained: bool,
}

// SAFETY: the decoder is only used from one thread at a time.
unsafe impl Send for FfmpegVideoDecoder {}

pub(crate) fn fferr(e: ff::Error) -> VideoError {
    VideoError::Device(format!("ffmpeg: {e}"))
}

pub(crate) fn codec_id(c: &CodecId) -> Option<ff::codec::Id> {
    Some(match c {
        CodecId::H264 => ff::codec::Id::H264,
        CodecId::Hevc => ff::codec::Id::HEVC,
        CodecId::Vp8 => ff::codec::Id::VP8,
        CodecId::Vp9 => ff::codec::Id::VP9,
        CodecId::Av1 => ff::codec::Id::AV1,
        CodecId::Aac => ff::codec::Id::AAC,
        CodecId::Opus => ff::codec::Id::OPUS,
        CodecId::Vorbis => ff::codec::Id::VORBIS,
        CodecId::Flac => ff::codec::Id::FLAC,
        CodecId::Mp3 => ff::codec::Id::MP3,
        CodecId::Ac3 => ff::codec::Id::AC3,
        CodecId::Eac3 => ff::codec::Id::EAC3,
        _ => return None,
    })
}

/// Copy `data` into the codec context's extradata (padded as libavcodec requires).
pub(crate) unsafe fn set_extradata(ctx: &mut ff::codec::Context, data: &[u8]) {
    if data.is_empty() {
        return;
    }
    // AV_INPUT_BUFFER_PADDING_SIZE.
    let pad = 64usize;
    let p = ff::ffi::av_mallocz(data.len() + pad) as *mut u8;
    if p.is_null() {
        return;
    }
    std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
    let raw = ctx.as_mut_ptr();
    (*raw).extradata = p;
    (*raw).extradata_size = data.len() as i32;
}

impl FfmpegVideoDecoder {
    pub fn new(req: &DecoderRequest, threads: usize) -> Result<Self> {
        ff::init().map_err(fferr)?;
        let id = codec_id(&req.codec)
            .ok_or_else(|| VideoError::Unsupported(format!("{:?}", req.codec)))?;
        let codec = ff::decoder::find(id)
            .ok_or_else(|| VideoError::NoDecoder(format!("libavcodec lacks {id:?}")))?;
        let mut ctx = ff::codec::Context::new_with_codec(codec);
        // SAFETY: ctx is a fresh, unopened context.
        unsafe { set_extradata(&mut ctx, &req.codec_private) };
        ctx.set_time_base(ff::Rational::new(1, 1_000_000));
        let mut threading = ff::threading::Config::kind(ff::threading::Type::Frame);
        threading.count = threads;
        ctx.set_threading(threading);
        let dec = ctx.decoder().video().map_err(fferr)?;
        Ok(FfmpegVideoDecoder {
            dec,
            frame: ff::frame::Video::empty(),
            scaler: None,
            draining: false,
            drained: false,
        })
    }

    fn convert(&mut self) -> Result<CpuFrame> {
        use ff::format::Pixel;
        let pts = MediaTime(self.frame.timestamp().or(self.frame.pts()).unwrap_or(0));
        let (w, h) = (self.frame.width(), self.frame.height());
        let fmt = self.frame.format();
        let (format, src): (PixelFormat, &ff::frame::Video) = match fmt {
            Pixel::YUV420P | Pixel::YUVJ420P => (PixelFormat::I420, &self.frame),
            Pixel::NV12 => (PixelFormat::Nv12, &self.frame),
            Pixel::YUV420P10LE => (PixelFormat::I420P10, &self.frame),
            Pixel::P010LE => (PixelFormat::P010, &self.frame),
            other => {
                let deep = other
                    .descriptor()
                    .is_some_and(|d| d.name().contains("10") || d.name().contains("12"));
                let (target, tf) = if deep {
                    (Pixel::P010LE, PixelFormat::P010)
                } else {
                    (Pixel::NV12, PixelFormat::Nv12)
                };
                if !self
                    .scaler
                    .as_ref()
                    .is_some_and(|(_, f, sw, sh)| *f == other && *sw == w && *sh == h)
                {
                    let s = ff::software::scaling::Context::get(
                        other,
                        w,
                        h,
                        target,
                        w,
                        h,
                        ff::software::scaling::Flags::BILINEAR,
                    )
                    .map_err(fferr)?;
                    self.scaler = Some((s, other, w, h));
                }
                let mut out = ff::frame::Video::empty();
                self.scaler
                    .as_mut()
                    .unwrap()
                    .0
                    .run(&self.frame, &mut out)
                    .map_err(fferr)?;
                return Ok(Self::copy_planes(&out, tf, pts));
            }
        };
        Ok(Self::copy_planes(src, format, pts))
    }

    fn copy_planes(f: &ff::frame::Video, format: PixelFormat, pts: MediaTime) -> CpuFrame {
        let n = f.planes();
        CpuFrame {
            format,
            width: f.width(),
            height: f.height(),
            planes: (0..n).map(|i| f.data(i).to_vec()).collect(),
            strides: (0..n).map(|i| f.stride(i)).collect(),
            pts,
        }
    }
}

impl VideoDecoder for FfmpegVideoDecoder {
    fn path(&self) -> DecoderPath {
        DecoderPath::Software {
            library: "ffmpeg".into(),
        }
    }

    fn send_packet(&mut self, pkt: &Packet) -> Result<bool> {
        let mut p = ff::Packet::copy(&pkt.data);
        p.set_pts(Some(pkt.pts.0));
        p.set_dts(Some(pkt.dts.0));
        if pkt.keyframe {
            p.set_flags(ff::packet::Flags::KEY);
        }
        match self.dec.send_packet(&p) {
            Ok(()) => {
                self.drained = false;
                self.draining = false;
                Ok(true)
            }
            Err(ff::Error::Other { errno }) if errno == ff::error::EAGAIN => Ok(false),
            // Corrupt packets are dropped rather than ending playback.
            Err(ff::Error::InvalidData) => Ok(true),
            Err(e) => Err(fferr(e)),
        }
    }

    fn receive_frame(&mut self) -> Result<Option<DecodedFrame>> {
        match self.dec.receive_frame(&mut self.frame) {
            Ok(()) => Ok(Some(DecodedFrame::Cpu(self.convert()?))),
            Err(ff::Error::Other { errno }) if errno == ff::error::EAGAIN => Ok(None),
            Err(ff::Error::Eof) => {
                self.drained = true;
                Ok(None)
            }
            Err(e) => Err(fferr(e)),
        }
    }

    fn drain(&mut self) -> Result<()> {
        if !self.draining {
            self.draining = true;
            self.dec.send_eof().map_err(fferr)?;
        }
        Ok(())
    }

    fn is_drained(&self) -> bool {
        self.drained
    }

    fn flush(&mut self) -> Result<()> {
        self.dec.flush();
        self.draining = false;
        self.drained = false;
        Ok(())
    }
}
