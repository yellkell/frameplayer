//! Video decoding.
//!
//! [`VideoDecoder`] is a non-blocking packet-in / frame-out interface.
//! Frames come out as [`DecodedFrame::DmaBuf`] (hardware path: zero-copy
//! DMA-BUF planes for Vulkan import via `VK_EXT_external_memory_dma_buf` +
//! `VK_EXT_image_drm_format_modifier`) or [`DecodedFrame::Cpu`] (software
//! path; the renderer uploads it).
//!
//! Implementations:
//! * [`v4l2::V4l2Decoder`]: V4L2 stateful memory-to-memory decoder
//!   (HEVC / H.264 / VP9 / AV1) via raw ioctls, exporting CAPTURE buffers as
//!   DMA-BUF. This is the Frame's primary path (qcom `iris`/`venus`).
//! * `dav1d` (feature `dav1d`): software AV1.
//! * `ffmpeg` (feature `ffmpeg`): software HEVC / H.264 / VP9 / AV1 via libavcodec.
//!
//! [`select_decoder`] tries hardware first, then software (capped at 4K by
//! default) and reports which path it picked and why. When both are
//! available the hardware decoder is wrapped in a
//! [`fallback::FallbackDecoder`], which switches to software at runtime if
//! the hardware fails during its first frames (STREAMON / CAPTURE setup
//! errors, decode errors, a stalled decoder).

pub mod convert;
pub mod fallback;
pub mod v4l2;

#[cfg(feature = "dav1d")]
pub mod dav1d;
#[cfg(feature = "ffmpeg")]
pub mod ffmpeg;

use crate::error::{Result, VideoError};
use crate::packet::{CodecId, Packet, TrackDesc};
use fp_core::MediaTime;
use std::os::fd::RawFd;
use std::sync::Arc;
use std::time::Duration;

/// DRM fourcc codes (`drm_fourcc.h`).
pub mod drm {
    pub const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
        (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
    }
    pub const FORMAT_NV12: u32 = fourcc(b'N', b'V', b'1', b'2');
    pub const FORMAT_P010: u32 = fourcc(b'P', b'0', b'1', b'0');
    pub const MOD_LINEAR: u64 = 0;
    /// `DRM_FORMAT_MOD_QCOM_COMPRESSED` (UBWC).
    pub const MOD_QCOM_COMPRESSED: u64 = (0x05u64 << 56) | 1;
}

/// CPU-side pixel layouts produced by software decoders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    /// 8-bit Y plane + interleaved UV plane (4:2:0).
    Nv12,
    /// 10-bit in the high bits of 16-bit little-endian samples, Y + UV (4:2:0).
    P010,
    /// 8-bit planar Y, U, V (4:2:0).
    I420,
    /// 10-bit planar Y, U, V in the low bits of 16-bit LE samples (yuv420p10le).
    I420P10,
}

impl PixelFormat {
    pub fn bit_depth(self) -> u8 {
        match self {
            PixelFormat::Nv12 | PixelFormat::I420 => 8,
            PixelFormat::P010 | PixelFormat::I420P10 => 10,
        }
    }
}

/// One plane of a DMA-BUF frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmaBufPlane {
    /// Borrowed fd, valid while the owning [`DmaBufFrame`] lives. Several
    /// planes may share one fd (single-buffer NV12).
    pub fd: RawFd,
    pub offset: u32,
    pub pitch: u32,
}

/// Runs a closure when the last clone is dropped; used to hand hardware
/// buffers back to the decoder once the renderer is done with a frame.
pub struct FrameLease(Option<Box<dyn FnOnce() + Send + Sync>>);

impl FrameLease {
    pub fn new(f: impl FnOnce() + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(FrameLease(Some(Box::new(f))))
    }
}

impl Drop for FrameLease {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f();
        }
    }
}

impl std::fmt::Debug for FrameLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FrameLease")
    }
}

/// A decoded frame living in DMA-BUF memory (zero-copy to Vulkan).
#[derive(Debug, Clone)]
pub struct DmaBufFrame {
    /// Stable identifier of the decoder buffer behind this frame (same value
    /// every time that buffer is reused, new value after a reallocation).
    /// Renderers cache DMA-BUF imports under it.
    pub buffer_id: u64,
    pub planes: Vec<DmaBufPlane>,
    /// DRM fourcc ([`drm::FORMAT_NV12`] or [`drm::FORMAT_P010`]).
    pub fourcc: u32,
    pub modifier: u64,
    /// Visible size (the buffer may be larger; see `coded_*`).
    pub width: u32,
    pub height: u32,
    pub coded_width: u32,
    pub coded_height: u32,
    pub pts: MediaTime,
    /// Keeps the buffer out of the decoder's queue while held.
    pub lease: Option<Arc<FrameLease>>,
}

/// A decoded frame in system memory.
#[derive(Debug, Clone, PartialEq)]
pub struct CpuFrame {
    pub format: PixelFormat,
    pub width: u32,
    pub height: u32,
    /// Planes in format order (NV12/P010: Y, UV; I420: Y, U, V).
    pub planes: Vec<Vec<u8>>,
    /// Bytes per row for each plane.
    pub strides: Vec<usize>,
    pub pts: MediaTime,
}

#[derive(Debug, Clone)]
pub enum DecodedFrame {
    DmaBuf(DmaBufFrame),
    Cpu(CpuFrame),
}

impl DecodedFrame {
    pub fn pts(&self) -> MediaTime {
        match self {
            DecodedFrame::DmaBuf(f) => f.pts,
            DecodedFrame::Cpu(f) => f.pts,
        }
    }
    pub fn set_pts(&mut self, pts: MediaTime) {
        match self {
            DecodedFrame::DmaBuf(f) => f.pts = pts,
            DecodedFrame::Cpu(f) => f.pts = pts,
        }
    }
    pub fn size(&self) -> (u32, u32) {
        match self {
            DecodedFrame::DmaBuf(f) => (f.width, f.height),
            DecodedFrame::Cpu(f) => (f.width, f.height),
        }
    }
}

/// Which decode path is in use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecoderPath {
    /// V4L2 stateful M2M decoder.
    Hardware { device: String, driver: String },
    /// Software library (`"dav1d"`, `"ffmpeg"`, `"mock"`, …).
    Software { library: String },
}

impl std::fmt::Display for DecoderPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecoderPath::Hardware { device, driver } => {
                write!(f, "hardware ({driver} on {device})")
            }
            DecoderPath::Software { library } => write!(f, "software ({library})"),
        }
    }
}

/// Non-blocking video decoder.
///
/// Protocol: call [`send_packet`](Self::send_packet) until it returns
/// `Ok(false)` (input queue full; the packet was *not* consumed), then pull
/// frames with [`receive_frame`](Self::receive_frame) until `Ok(None)`.
/// At end of stream call [`drain`](Self::drain) and keep receiving until
/// [`is_drained`](Self::is_drained). For a seek call [`flush`](Self::flush)
/// and resume from a keyframe.
pub trait VideoDecoder: Send {
    fn path(&self) -> DecoderPath;
    /// Queue a packet. `Ok(false)` = full, retry after receiving frames.
    fn send_packet(&mut self, pkt: &Packet) -> Result<bool>;
    /// A decoded frame, in presentation order, if one is ready.
    fn receive_frame(&mut self) -> Result<Option<DecodedFrame>>;
    /// Signal end of stream.
    fn drain(&mut self) -> Result<()>;
    /// True once every frame has been returned after [`drain`](Self::drain).
    fn is_drained(&self) -> bool;
    /// Discard all queued packets and pending frames (seek).
    fn flush(&mut self) -> Result<()>;
    /// Block until the decoder likely has work (a frame ready or input
    /// space), up to `timeout`.
    fn wait(&mut self, timeout: Duration) {
        std::thread::sleep(timeout.min(Duration::from_millis(1)));
    }
    /// Set once when the decoder switched paths at runtime (hardware →
    /// software fallback); the message says why. [`path`](Self::path)
    /// reports the new path afterwards.
    fn take_fallback_notice(&mut self) -> Option<String> {
        None
    }
}

/// What to decode.
#[derive(Debug, Clone, PartialEq)]
pub struct DecoderRequest {
    pub codec: CodecId,
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    /// `avcC` / `hvcC` / `av1C` / `vpcC` record from the container.
    pub codec_private: Vec<u8>,
}

impl DecoderRequest {
    pub fn from_track(t: &TrackDesc) -> Self {
        let v = t.video.clone().unwrap_or_default();
        DecoderRequest {
            codec: t.codec.clone(),
            width: v.width,
            height: v.height,
            bit_depth: v.bit_depth.max(8),
            codec_private: t.codec_private.clone(),
        }
    }
}

/// Decoder selection policy.
#[derive(Debug, Clone, PartialEq)]
pub struct DecoderOptions {
    pub allow_hardware: bool,
    pub allow_software: bool,
    /// Largest picture (in pixels) a software decoder will be used for;
    /// `None` = unlimited. Default: 4096×2160.
    pub software_pixel_cap: Option<u64>,
    /// Accept vendor-compressed (UBWC) CAPTURE formats from V4L2.
    pub allow_compressed_formats: bool,
    /// Frames the engine keeps queued ahead of display (CAPTURE buffers
    /// are sized to cover it).
    pub decode_ahead: u32,
    /// Software decoder thread count (0 = auto).
    pub threads: usize,
}

impl Default for DecoderOptions {
    fn default() -> Self {
        DecoderOptions {
            allow_hardware: true,
            allow_software: true,
            software_pixel_cap: Some(4096 * 2160),
            allow_compressed_formats: false,
            decode_ahead: 4,
            threads: 0,
        }
    }
}

/// Result of [`select_decoder`].
pub struct DecoderSelection {
    pub decoder: Box<dyn VideoDecoder>,
    pub path: DecoderPath,
    /// User-visible warnings (e.g. "software decoding").
    pub warnings: Vec<String>,
    /// Paths that were tried and rejected, with reasons.
    pub rejected: Vec<String>,
}

impl std::fmt::Debug for DecoderSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecoderSelection")
            .field("path", &self.path)
            .field("warnings", &self.warnings)
            .field("rejected", &self.rejected)
            .finish()
    }
}

/// Compiled-in software decoders able to handle `codec`.
pub fn software_decoders_for(codec: &CodecId) -> Vec<&'static str> {
    #[allow(unused_mut)]
    let mut v = Vec::new();
    #[cfg(feature = "dav1d")]
    if *codec == CodecId::Av1 {
        v.push("dav1d");
    }
    #[cfg(feature = "ffmpeg")]
    if matches!(
        codec,
        CodecId::Hevc | CodecId::H264 | CodecId::Vp9 | CodecId::Vp8 | CodecId::Av1
    ) {
        v.push("ffmpeg");
    }
    let _ = codec;
    v
}

/// Open a software decoder by library name.
pub fn open_software(
    library: &str,
    req: &DecoderRequest,
    threads: usize,
) -> Result<Box<dyn VideoDecoder>> {
    let _ = threads;
    match library {
        #[cfg(feature = "dav1d")]
        "dav1d" => Ok(Box::new(dav1d::Dav1dDecoder::new(threads)?)),
        #[cfg(feature = "ffmpeg")]
        "ffmpeg" => Ok(Box::new(ffmpeg::FfmpegVideoDecoder::new(req, threads)?)),
        _ => Err(VideoError::NoSoftwareDecoder(format!(
            "{:?} via {library}",
            req.codec
        ))),
    }
}

/// Opens a hardware decoder for [`select_decoder_with`].
pub type HardwareOpener<'a> =
    &'a mut dyn FnMut(&DecoderRequest, &DecoderOptions) -> Result<Box<dyn VideoDecoder>>;

/// Opens a software decoder by library name for [`select_decoder_with`].
pub type SoftwareOpener =
    Arc<dyn Fn(&str, &DecoderRequest, usize) -> Result<Box<dyn VideoDecoder>> + Send + Sync>;

/// Pick a decoder: V4L2 hardware first, then software. Reports the chosen
/// path, warnings, and why other paths were rejected. A hardware decoder is
/// returned wrapped in a runtime fallback to software when a software
/// decoder for the stream is compiled in and within the pixel cap.
pub fn select_decoder(req: &DecoderRequest, opts: &DecoderOptions) -> Result<DecoderSelection> {
    let mut hw = |r: &DecoderRequest, o: &DecoderOptions| {
        v4l2::V4l2Decoder::open_best(r, o).map(|d| Box::new(d) as Box<dyn VideoDecoder>)
    };
    let libs: Vec<String> = software_decoders_for(&req.codec)
        .into_iter()
        .map(String::from)
        .collect();
    select_decoder_with(
        req,
        opts,
        &mut hw,
        &libs,
        Arc::new(|lib: &str, r: &DecoderRequest, t: usize| open_software(lib, r, t)),
        fallback::FallbackPolicy::default(),
    )
}

/// [`select_decoder`] with injectable decoder constructors (tests, tools).
pub fn select_decoder_with(
    req: &DecoderRequest,
    opts: &DecoderOptions,
    hardware: HardwareOpener<'_>,
    software_libs: &[String],
    software: SoftwareOpener,
    policy: fallback::FallbackPolicy,
) -> Result<DecoderSelection> {
    let mut rejected = Vec::new();
    // Software eligibility first: it decides both the direct software path
    // and whether a hardware decoder gets a runtime fallback.
    let pixels = req.width as u64 * req.height as u64;
    let software_reject = if !opts.allow_software {
        Some("software: disabled".to_string())
    } else if software_libs.is_empty() {
        Some(format!(
            "software: no software decoder compiled in for {:?}",
            req.codec
        ))
    } else {
        opts.software_pixel_cap.filter(|&c| pixels > c).map(|cap| {
            format!(
                "software: {}x{} exceeds the software decode cap ({} Mpx)",
                req.width,
                req.height,
                cap as f64 / 1e6
            )
        })
    };

    if opts.allow_hardware {
        match hardware(req, opts) {
            Ok(d) => {
                let path = d.path();
                tracing::info!("video decode: {path}");
                let decoder: Box<dyn VideoDecoder> = if software_reject.is_none() {
                    let libs = software_libs.to_vec();
                    let req = req.clone();
                    let threads = opts.threads;
                    let factory: fallback::SoftwareFactory = Box::new(move || {
                        let mut errs = Vec::new();
                        for lib in &libs {
                            match software(lib, &req, threads) {
                                Ok(d) => return Ok(d),
                                Err(e) => errs.push(format!("{lib}: {e}")),
                            }
                        }
                        Err(VideoError::NoSoftwareDecoder(errs.join("; ")))
                    });
                    Box::new(fallback::FallbackDecoder::with_policy(d, factory, policy))
                } else {
                    d
                };
                return Ok(DecoderSelection {
                    decoder,
                    path,
                    warnings: Vec::new(),
                    rejected,
                });
            }
            Err(e) => {
                tracing::info!("no V4L2 decoder for {:?}: {e}", req.codec);
                rejected.push(format!("hardware: {e}"));
            }
        }
    }
    match software_reject {
        Some(r) => {
            if opts.allow_software {
                rejected.push(r);
            }
        }
        None => {
            for lib in software_libs {
                match software(lib, req, opts.threads) {
                    Ok(d) => {
                        let path = d.path();
                        let warn = format!(
                            "Using software video decoding ({lib}); playback above 4K or at high frame rates may stutter."
                        );
                        tracing::warn!("{warn}");
                        return Ok(DecoderSelection {
                            decoder: d,
                            path,
                            warnings: vec![warn],
                            rejected,
                        });
                    }
                    Err(e) => rejected.push(format!("software {lib}: {e}")),
                }
            }
        }
    }
    Err(VideoError::NoDecoder(format!(
        "{:?} {}x{} {}-bit: {}",
        req.codec,
        req.width,
        req.height,
        req.bit_depth,
        rejected.join("; ")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_runs_once_on_last_drop() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let lease = FrameLease::new(move || tx.send(7).unwrap());
        let a = lease.clone();
        drop(lease);
        assert!(rx.try_recv().is_err());
        drop(a);
        assert_eq!(rx.try_recv().unwrap(), 7);
    }

    #[test]
    fn drm_fourccs() {
        assert_eq!(drm::FORMAT_NV12, 0x3231_564e);
        assert_eq!(drm::FORMAT_P010, 0x3031_3050);
    }

    use crate::decode::fallback::tests::{FailingHw, Failure};
    use crate::mock::MockVideoDecoder;

    fn mock_sw() -> SoftwareOpener {
        Arc::new(|lib: &str, _: &DecoderRequest, _| match lib {
            "mock" => Ok(Box::new(MockVideoDecoder::default()) as Box<dyn VideoDecoder>),
            _ => Err(VideoError::NoSoftwareDecoder(lib.into())),
        })
    }

    fn req(w: u32, h: u32) -> DecoderRequest {
        DecoderRequest {
            codec: CodecId::Hevc,
            width: w,
            height: h,
            bit_depth: 8,
            codec_private: vec![],
        }
    }

    fn select(
        r: &DecoderRequest,
        failure: Option<Failure>,
        libs: &[&str],
    ) -> Result<DecoderSelection> {
        let mut hw = |_: &DecoderRequest, _: &DecoderOptions| match failure {
            Some(f) => Ok(Box::new(FailingHw::new(f)) as Box<dyn VideoDecoder>),
            None => Err(VideoError::NoDecoder("no V4L2 M2M decoder devices".into())),
        };
        let libs: Vec<String> = libs.iter().map(|s| s.to_string()).collect();
        select_decoder_with(
            r,
            &DecoderOptions::default(),
            &mut hw,
            &libs,
            mock_sw(),
            fallback::FallbackPolicy::default(),
        )
    }

    fn pkt(i: i64) -> Packet {
        Packet {
            track: 1,
            pts: MediaTime::from_millis(i * 40),
            dts: MediaTime::from_millis(i * 40),
            duration: MediaTime::from_millis(40),
            keyframe: i == 0,
            data: vec![i as u8; 8],
        }
    }

    #[test]
    fn no_hardware_selects_software_with_warning() {
        let sel = select(&req(1920, 1080), None, &["broken", "mock"]).unwrap();
        assert_eq!(
            sel.path,
            DecoderPath::Software {
                library: "mock".into()
            }
        );
        assert_eq!(sel.warnings.len(), 1);
        assert!(
            sel.rejected[0].starts_with("hardware: "),
            "{:?}",
            sel.rejected
        );
        assert!(
            sel.rejected[1].starts_with("software broken"),
            "{:?}",
            sel.rejected
        );
    }

    #[test]
    fn hardware_preferred_and_falls_back_at_runtime() {
        let mut sel = select(&req(3840, 2160), Some(Failure::ReceiveError), &["mock"]).unwrap();
        assert!(matches!(sel.path, DecoderPath::Hardware { .. }));
        assert!(sel.warnings.is_empty());
        let d = &mut sel.decoder;
        assert!(d.send_packet(&pkt(0)).unwrap());
        let f = d
            .receive_frame()
            .unwrap()
            .expect("software frame after fallback");
        assert_eq!(f.pts(), MediaTime::ZERO);
        assert!(matches!(d.path(), DecoderPath::Software { .. }));
        assert!(d.take_fallback_notice().unwrap().contains("falling back"));
    }

    #[test]
    fn software_cap_applies_to_selection_and_fallback() {
        // 8K: hardware works, no software fallback wrapper (errors pass through).
        let mut sel = select(&req(7680, 3840), Some(Failure::ReceiveError), &["mock"]).unwrap();
        sel.decoder.send_packet(&pkt(0)).unwrap();
        assert!(sel.decoder.receive_frame().is_err());
        assert!(sel.decoder.take_fallback_notice().is_none());
        // 8K without hardware: rejected with the cap reason.
        match select(&req(7680, 3840), None, &["mock"]) {
            Err(VideoError::NoDecoder(m)) => assert!(m.contains("software decode cap"), "{m}"),
            other => panic!("unexpected {other:?}"),
        }
        // Exactly 4K (4096x2160) is still allowed.
        assert!(select(&req(4096, 2160), None, &["mock"]).is_ok());
    }

    #[test]
    fn hardware_without_software_is_unwrapped() {
        let mut sel = select(&req(1920, 1080), Some(Failure::ReceiveError), &[]).unwrap();
        sel.decoder.send_packet(&pkt(0)).unwrap();
        assert!(sel.decoder.receive_frame().is_err());
    }

    #[test]
    fn selection_reports_reasons_without_decoders() {
        // No V4L2 devices / no SW features in the test environment: the
        // error must explain every rejected path.
        let req = DecoderRequest {
            codec: CodecId::Hevc,
            width: 7680,
            height: 3840,
            bit_depth: 10,
            codec_private: vec![],
        };
        let opts = DecoderOptions::default();
        match select_decoder(&req, &opts) {
            Ok(sel) => assert!(matches!(
                sel.path,
                DecoderPath::Hardware { .. } | DecoderPath::Software { .. }
            )),
            Err(VideoError::NoDecoder(msg)) => {
                assert!(msg.contains("hardware"), "{msg}");
                assert!(msg.contains("software"), "{msg}");
            }
            Err(e) => panic!("unexpected error {e}"),
        }
    }
}
