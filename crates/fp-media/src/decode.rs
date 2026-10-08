//! Thin RAII wrappers over FFmpeg packets, frames and decoders.

use crate::{Error, Result, check};
use fp_ffmpeg_sys as ff;
use std::ffi::{CStr, CString};

/// An owned `AVPacket`.
pub struct Packet(pub(crate) *mut ff::AVPacket);
// SAFETY: packets are reference counted buffers; ownership moves between threads.
unsafe impl Send for Packet {}

impl Packet {
    pub fn new() -> Packet {
        // SAFETY: allocation; checked by users through as_ptr non-null.
        Packet(unsafe { ff::av_packet_alloc() })
    }
    pub fn as_ptr(&self) -> *mut ff::AVPacket {
        self.0
    }
    pub fn stream_index(&self) -> usize {
        // SAFETY: valid packet.
        unsafe { (*self.0).stream_index.max(0) as usize }
    }
    /// Moves the contents into a fresh packet, leaving this one empty.
    pub fn take(&mut self) -> Packet {
        let p = Packet::new();
        // SAFETY: both packets are valid.
        unsafe { ff::av_packet_move_ref(p.0, self.0) };
        p
    }
    pub fn size(&self) -> usize {
        // SAFETY: valid packet.
        unsafe { (*self.0).size.max(0) as usize }
    }
}

impl Default for Packet {
    fn default() -> Self {
        Packet::new()
    }
}

impl Drop for Packet {
    fn drop(&mut self) {
        // SAFETY: we own the packet.
        unsafe { ff::av_packet_free(&mut self.0) };
    }
}

/// An owned `AVFrame`.
pub struct Frame(pub(crate) *mut ff::AVFrame);
// SAFETY: as for Packet.
unsafe impl Send for Frame {}

impl Frame {
    pub fn new() -> Frame {
        // SAFETY: allocation.
        Frame(unsafe { ff::av_frame_alloc() })
    }
    pub fn as_ptr(&self) -> *mut ff::AVFrame {
        self.0
    }
    /// Gives the underlying reference away and allocates a fresh empty frame.
    pub fn take_raw(&mut self) -> *mut ff::AVFrame {
        std::mem::replace(&mut self.0, unsafe { ff::av_frame_alloc() })
    }
}

impl Default for Frame {
    fn default() -> Self {
        Frame::new()
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        // SAFETY: we own the frame.
        unsafe { ff::av_frame_free(&mut self.0) };
    }
}

/// Hardware decoding preference.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum HwDecode {
    /// V4L2 hardware decoder when one exists for the codec, else software.
    #[default]
    Auto,
    Off,
}

/// Hardware (V4L2 stateful) decoder name for a codec, if FFmpeg has one.
fn v4l2_name(codec: ff::AVCodecID) -> Option<&'static CStr> {
    match codec {
        ff::AV_CODEC_ID_HEVC => Some(c"hevc_v4l2m2m"),
        ff::AV_CODEC_ID_H264 => Some(c"h264_v4l2m2m"),
        ff::AV_CODEC_ID_VP9 => Some(c"vp9_v4l2m2m"),
        ff::AV_CODEC_ID_VP8 => Some(c"vp8_v4l2m2m"),
        ff::AV_CODEC_ID_MPEG4 => Some(c"mpeg4_v4l2m2m"),
        _ => None,
    }
}

/// One way of decoding a stream, in the order [`candidates`] tries them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Candidate {
    /// Our own V4L2 decoder ([`crate::v4l2`]).
    NativeV4l2,
    /// An FFmpeg decoder by name, and whether it is hardware.
    Named(&'static CStr, bool),
    /// FFmpeg's default decoder for the codec (software).
    Default,
}

/// Decoders to try for a stream: the native V4L2 path, FFmpeg's V4L2
/// wrapper, dav1d for AV1, then FFmpeg's default software decoder.
fn candidates(video: bool, codec: ff::AVCodecID, hw: HwDecode, native: bool) -> Vec<Candidate> {
    let mut c = Vec::new();
    if video && hw == HwDecode::Auto {
        if native && crate::v4l2::Codec::from_ffmpeg(codec).is_some() {
            c.push(Candidate::NativeV4l2);
        }
        if let Some(n) = v4l2_name(codec) {
            c.push(Candidate::Named(n, true));
        }
    }
    if codec == ff::AV_CODEC_ID_AV1 {
        c.push(Candidate::Named(c"libdav1d", false));
    }
    c.push(Candidate::Default);
    c
}

/// Runs `f` with the calling thread allowed on every CPU, so the threads it
/// starts inherit that: a software decoder sizes its pool from the CPUs it
/// may use and starts its workers when opened. Steam launches games on the
/// Frame's five big cores only (`STEAM_LAUNCH_WRAPPER_AFFINITY_LIST=0xf8`),
/// where 8K60 AV1 decodes at 61 fps with nothing else running and falls
/// behind once the compositor and renderer share them; the other three add
/// ~7%. SteamVR's tracking there runs at nice -10, so it still comes first.
fn on_all_cpus<T>(f: impl FnOnce() -> T) -> T {
    #[cfg(target_os = "linux")]
    // SAFETY: plain affinity calls on the calling thread with owned sets.
    unsafe {
        let size = std::mem::size_of::<libc::cpu_set_t>();
        let mut old: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(0, size, &mut old) == 0 {
            let mut all: libc::cpu_set_t = std::mem::zeroed();
            let cpus = libc::sysconf(libc::_SC_NPROCESSORS_CONF).clamp(1, 1024) as usize;
            for cpu in 0..cpus {
                libc::CPU_SET(cpu, &mut all);
            }
            if libc::sched_setaffinity(0, size, &all) == 0 {
                let r = f();
                libc::sched_setaffinity(0, size, &old);
                return r;
            }
        }
    }
    f()
}

/// Packets kept while the native decoder starts up, replayed into the
/// fallback decoder if it fails before its first frame.
#[cfg(target_os = "linux")]
const STARTUP_PACKETS: usize = 240;

#[cfg(target_os = "linux")]
struct Native {
    dec: crate::v4l2::V4l2Decoder,
    /// Owned copy of the stream parameters, for opening a fallback.
    par: *mut ff::AVCodecParameters,
    hw: HwDecode,
    /// Packets sent before the first frame (None once frames flow or the
    /// startup window is exceeded).
    startup: Option<Vec<Packet>>,
}

#[cfg(target_os = "linux")]
impl Drop for Native {
    fn drop(&mut self) {
        // SAFETY: we own the parameters copy.
        unsafe { ff::avcodec_parameters_free(&mut self.par) };
    }
}

pub struct Decoder {
    /// FFmpeg decoder; null while the native decoder is active.
    ctx: *mut ff::AVCodecContext,
    #[cfg(target_os = "linux")]
    native: Option<Box<Native>>,
    /// Packets to feed the FFmpeg decoder before new ones (after a fallback).
    replay: std::collections::VecDeque<Packet>,
    pub name: String,
    pub hardware: bool,
    pub time_base: ff::AVRational,
}

// SAFETY: one thread drives a decoder at a time.
unsafe impl Send for Decoder {}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: we own the context (null-safe).
        unsafe { ff::avcodec_free_context(&mut self.ctx) };
    }
}

impl Decoder {
    /// Opens a decoder for `stream`. Video tries the hardware decoders first
    /// when `hw` allows, and AV1 uses dav1d in software.
    pub(crate) fn open(stream: *mut ff::AVStream, hw: HwDecode) -> Result<Decoder> {
        // SAFETY: stream belongs to an open input.
        let (par, time_base) = unsafe { ((*stream).codecpar, (*stream).time_base) };
        Self::open_with(par, time_base, hw, crate::v4l2::enabled())
    }

    fn open_with(
        par: *const ff::AVCodecParameters,
        time_base: ff::AVRational,
        hw: HwDecode,
        native: bool,
    ) -> Result<Decoder> {
        // SAFETY: par is valid for the call.
        let (codec_id, kind) = unsafe { ((*par).codec_id, (*par).codec_type) };
        let mut last_err = Error::Unsupported(format!("no decoder for codec id {codec_id}"));
        for cand in candidates(kind == ff::AVMEDIA_TYPE_VIDEO, codec_id, hw, native) {
            let r = match cand {
                Candidate::NativeV4l2 => Self::open_native(par, time_base, hw),
                Candidate::Named(name, hardware) => {
                    // SAFETY: lookup returns a static descriptor or null.
                    let c = unsafe { ff::avcodec_find_decoder_by_name(name.as_ptr()) };
                    if c.is_null() {
                        continue;
                    }
                    Self::try_open(c, par, time_base, hardware)
                }
                Candidate::Default => {
                    // SAFETY: as above.
                    let c = unsafe { ff::avcodec_find_decoder(codec_id) };
                    if c.is_null() {
                        continue;
                    }
                    Self::try_open(c, par, time_base, false)
                }
            };
            match r {
                Ok(d) => return Ok(d),
                Err(e) => {
                    log::info!("decoder unavailable: {e}");
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }

    #[cfg(target_os = "linux")]
    fn open_native(
        par: *const ff::AVCodecParameters,
        time_base: ff::AVRational,
        hw: HwDecode,
    ) -> Result<Decoder> {
        let dec = crate::v4l2::V4l2Decoder::open(par, time_base)?;
        // SAFETY: copies the parameters into a fresh allocation we own.
        let own = unsafe {
            let mut p = ff::avcodec_parameters_alloc();
            if p.is_null() || ff::avcodec_parameters_copy(p, par) < 0 {
                ff::avcodec_parameters_free(&mut p);
                return Err(Error::Unsupported("codec parameters copy".into()));
            }
            p
        };
        let name = format!("v4l2 ({})", dec.device);
        Ok(Decoder {
            ctx: std::ptr::null_mut(),
            native: Some(Box::new(Native {
                dec,
                par: own,
                hw,
                startup: Some(Vec::new()),
            })),
            replay: Default::default(),
            name,
            hardware: true,
            time_base,
        })
    }

    #[cfg(not(target_os = "linux"))]
    fn open_native(
        _par: *const ff::AVCodecParameters,
        _time_base: ff::AVRational,
        _hw: HwDecode,
    ) -> Result<Decoder> {
        Err(Error::Unsupported("V4L2 needs Linux".into()))
    }

    fn try_open(
        codec: *const ff::AVCodec,
        par: *const ff::AVCodecParameters,
        time_base: ff::AVRational,
        hardware: bool,
    ) -> Result<Decoder> {
        // SAFETY: standard decoder setup; ctx freed by Decoder::drop or here.
        unsafe {
            let name = CStr::from_ptr((*codec).name).to_string_lossy().into_owned();
            let ctx = ff::avcodec_alloc_context3(codec);
            if ctx.is_null() {
                return Err(Error::Unsupported("avcodec_alloc_context3".into()));
            }
            let mut dec = Decoder {
                ctx,
                #[cfg(target_os = "linux")]
                native: None,
                replay: Default::default(),
                name,
                hardware,
                time_base,
            };
            check(
                ff::avcodec_parameters_to_context(ctx, par),
                "decoder parameters",
            )?;
            (*ctx).pkt_timebase = time_base;
            if !hardware {
                (*ctx).thread_count = 0; // auto
                (*ctx).thread_type = (ff::FF_THREAD_FRAME | ff::FF_THREAD_SLICE) as i32;
            }
            let mut opts: *mut ff::AVDictionary = std::ptr::null_mut();
            if hardware {
                // An 8K capture buffer is 48 MiB and the player's frame queue
                // holds its own copies, so a few beyond the driver's minimum
                // are enough (24 used to exhaust the decoder's memory).
                let k = CString::new("num_capture_buffers").unwrap_or_default();
                let v = CString::new("8").unwrap_or_default();
                ff::av_dict_set(&mut opts, k.as_ptr(), v.as_ptr(), 0);
            }
            let video = (*par).codec_type == ff::AVMEDIA_TYPE_VIDEO;
            let r = if video && !hardware {
                on_all_cpus(|| ff::avcodec_open2(ctx, codec, &mut opts))
            } else {
                ff::avcodec_open2(ctx, codec, &mut opts)
            };
            ff::av_dict_free(&mut opts);
            check(
                r,
                if hardware {
                    "open hardware decoder"
                } else {
                    "open decoder"
                },
            )?;
            dec.time_base = time_base;
            Ok(dec)
        }
    }

    pub fn as_ptr(&self) -> *mut ff::AVCodecContext {
        self.ctx
    }

    /// Replaces a failed native decoder with the next candidates (FFmpeg's
    /// V4L2 wrapper, then software), replaying startup packets into it.
    #[cfg(target_os = "linux")]
    fn fall_back(&mut self, why: &Error) -> Result<()> {
        let Some(mut native) = self.native.take() else {
            return Ok(());
        };
        log::warn!(
            "{}: {why}; falling back to FFmpeg decoding{}",
            self.name,
            if native.startup.is_some() {
                ""
            } else {
                " (resumes at the next keyframe)"
            }
        );
        let mut next = Self::open_with(native.par, self.time_base, native.hw, false)?;
        std::mem::swap(&mut self.ctx, &mut next.ctx);
        self.name = std::mem::take(&mut next.name);
        self.hardware = next.hardware;
        self.replay = native.startup.take().unwrap_or_default().into();
        log::info!("now decoding with {}", self.name);
        Ok(())
    }

    /// Sends a packet (or end of stream when `pkt` is null). Returns false if
    /// the decoder is full and frames must be received first.
    pub(crate) fn send(&mut self, pkt: *const ff::AVPacket) -> Result<bool> {
        #[cfg(target_os = "linux")]
        if let Some(n) = self.native.as_mut() {
            match n.dec.send(pkt) {
                Ok(accepted) => {
                    if accepted
                        && !pkt.is_null()
                        && let Some(s) = n.startup.as_mut()
                    {
                        if s.len() < STARTUP_PACKETS {
                            let copy = Packet::new();
                            // SAFETY: adds a reference to the caller's packet.
                            if unsafe { ff::av_packet_ref(copy.as_ptr(), pkt) } >= 0 {
                                s.push(copy);
                            }
                        } else {
                            n.startup = None;
                        }
                    }
                    return Ok(accepted);
                }
                Err(e) => self.fall_back(&e)?,
            }
        }
        while let Some(p) = self.replay.front() {
            if !self.send_ffmpeg(p.as_ptr())? {
                return Ok(false);
            }
            self.replay.pop_front();
        }
        self.send_ffmpeg(pkt)
    }

    fn send_ffmpeg(&mut self, pkt: *const ff::AVPacket) -> Result<bool> {
        if self.ctx.is_null() {
            return Err(Error::Unsupported("decoder closed after a failure".into()));
        }
        // SAFETY: ctx is open; pkt valid or null.
        let r = unsafe { ff::avcodec_send_packet(self.ctx, pkt) };
        if r == ff::AVERROR_EAGAIN || r == ff::AVERROR_EOF_ {
            return Ok(false);
        }
        if r == ff::AVERROR_INVALIDDATA_ {
            log::debug!("{}: skipping invalid packet", self.name);
            return Ok(true);
        }
        check(r, "send packet")?;
        Ok(true)
    }

    /// Receives a frame. `Ok(None)` means more input is needed, `Ok(Some(false))`
    /// means the decoder is fully drained.
    pub fn receive(&mut self, frame: &mut Frame) -> Result<Option<bool>> {
        #[cfg(target_os = "linux")]
        if let Some(n) = self.native.as_mut() {
            match n.dec.receive(frame.0) {
                Ok(r) => {
                    if r == Some(true) {
                        n.startup = None;
                    }
                    return Ok(r);
                }
                Err(e) => {
                    self.fall_back(&e)?;
                    return Ok(None);
                }
            }
        }
        if self.ctx.is_null() {
            return Err(Error::Unsupported("decoder closed after a failure".into()));
        }
        // SAFETY: ctx is open; frame valid.
        let r = unsafe { ff::avcodec_receive_frame(self.ctx, frame.0) };
        if r == ff::AVERROR_EAGAIN {
            return Ok(None);
        }
        if r == ff::AVERROR_EOF_ {
            return Ok(Some(false));
        }
        check(r, "receive frame")?;
        Ok(Some(true))
    }

    pub fn flush(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(n) = self.native.as_mut() {
            n.dec.flush();
            if n.startup.is_some() {
                n.startup = Some(Vec::new());
            }
            return;
        }
        self.replay.clear();
        if !self.ctx.is_null() {
            // SAFETY: ctx is open.
            unsafe { ff::avcodec_flush_buffers(self.ctx) };
        }
    }

    /// Best-effort presentation time of a decoded frame in seconds.
    pub fn frame_time(&self, frame: &Frame) -> Option<f64> {
        // SAFETY: valid frame.
        let ts = unsafe { (*frame.0).best_effort_timestamp };
        (ts != ff::AV_NOPTS_VALUE).then(|| ts as f64 * crate::q2d(self.time_base))
    }

    pub fn frame_duration(&self, frame: &Frame) -> f64 {
        // SAFETY: valid frame.
        let d = unsafe { (*frame.0).duration };
        if d > 0 {
            d as f64 * crate::q2d(self.time_base)
        } else {
            0.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{FrameConverter, PixelLayout};
    use crate::input::Input;
    use fp_core::source::FileSource;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    fn decode_all(name: &str, max: usize) -> (String, Vec<crate::VideoFrame>) {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data")
            .join(name);
        let mut input = Input::open(
            Arc::new(FileSource::open(&path).unwrap()),
            name,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let vi = input.best_stream(ff::AVMEDIA_TYPE_VIDEO).unwrap();
        let mut dec = Decoder::open(input.streams()[vi], HwDecode::Auto).unwrap();
        let mut conv = FrameConverter::default();
        let pkt = Packet::new();
        let mut frame = Frame::new();
        let mut out = Vec::new();
        let mut eof = false;
        while out.len() < max {
            if !eof {
                if input.read(pkt.as_ptr()).unwrap() {
                    if pkt.stream_index() != vi {
                        unsafe { ff::av_packet_unref(pkt.as_ptr()) };
                        continue;
                    }
                    dec.send(pkt.as_ptr()).unwrap();
                    unsafe { ff::av_packet_unref(pkt.as_ptr()) };
                } else {
                    eof = true;
                    dec.send(std::ptr::null()).unwrap();
                }
            }
            loop {
                match dec.receive(&mut frame).unwrap() {
                    Some(true) => {
                        let t = dec.frame_time(&frame).unwrap_or(0.0);
                        out.push(conv.convert(frame.take_raw(), t, 0.0, 0).unwrap());
                    }
                    Some(false) => return (dec.name.clone(), out),
                    None => break,
                }
            }
        }
        (dec.name.clone(), out)
    }

    #[test]
    fn candidate_order_and_fallbacks() {
        use Candidate::*;
        let hevc = ff::AV_CODEC_ID_HEVC;
        assert_eq!(
            candidates(true, hevc, HwDecode::Auto, true),
            vec![NativeV4l2, Named(c"hevc_v4l2m2m", true), Default]
        );
        // After the native decoder fails (or with it disabled).
        assert_eq!(
            candidates(true, hevc, HwDecode::Auto, false),
            vec![Named(c"hevc_v4l2m2m", true), Default]
        );
        assert_eq!(
            candidates(true, ff::AV_CODEC_ID_H264, HwDecode::Auto, true)[0],
            NativeV4l2
        );
        assert_eq!(candidates(true, hevc, HwDecode::Off, true), vec![Default]);
        // Codecs the native decoder does not handle go to FFmpeg directly.
        assert_eq!(
            candidates(true, ff::AV_CODEC_ID_VP9, HwDecode::Auto, true),
            vec![Named(c"vp9_v4l2m2m", true), Default]
        );
        assert_eq!(
            candidates(true, ff::AV_CODEC_ID_AV1, HwDecode::Auto, true),
            vec![Named(c"libdav1d", false), Default]
        );
        assert_eq!(
            candidates(false, ff::AV_CODEC_ID_AAC, HwDecode::Auto, true),
            vec![Default]
        );
    }

    #[test]
    fn decodes_h264() {
        let (name, frames) = decode_all("h264_aac_180_LR.mp4", 100);
        assert_eq!(name, "h264", "no V4L2 device here, so software");
        assert_eq!(frames.len(), 60);
        let f = &frames[10];
        assert_eq!(
            (f.width, f.height, f.layout),
            (640, 320, PixelLayout::I420 { bits: 8 })
        );
        assert!((frames[30].pts - 1.0).abs() < 0.04, "{}", frames[30].pts);
        let planes = f.packed_planes();
        assert_eq!(planes[0].len(), 640 * 320);
        assert_eq!(planes[1].len(), 320 * 160);
        // testsrc2 is colourful: luma must vary.
        let (mn, mx) = planes[0]
            .iter()
            .fold((255u8, 0u8), |(a, b), &v| (a.min(v), b.max(v)));
        assert!(mx - mn > 100);
    }

    #[test]
    fn decodes_hevc_10bit() {
        let (_, frames) = decode_all("hevc10_tb.mkv", 5);
        assert_eq!(frames[0].layout, PixelLayout::I420 { bits: 10 });
        let y = &frames[0].packed_planes()[0];
        assert_eq!(y.len(), 512 * 512 * 2);
        let max = y
            .chunks(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .max()
            .unwrap();
        assert!(
            max <= 1023 && max > 600,
            "10-bit values in low bits, got max {max}"
        );
    }

    #[test]
    fn decodes_av1_with_dav1d_and_vp9() {
        let (name, frames) = decode_all("av1_opus.webm", 24);
        assert_eq!(name, "libdav1d");
        assert_eq!(frames.len(), 24);
        let (_, v) = decode_all("vp9.webm", 5);
        assert_eq!(v[0].width, 320);
    }
}
