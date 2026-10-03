//! V4L2 stateful memory-to-memory video decoder.
//!
//! Follows the kernel's stateful decoder interface
//! (`Documentation/userspace-api/media/v4l/dev-decoder.rst`):
//!
//! 1. Find a `/dev/video*` node with `V4L2_CAP_VIDEO_M2M_MPLANE` whose
//!    OUTPUT queue lists the coded format ([`enumerate_devices`]).
//! 2. `S_FMT(OUTPUT)` with the coded pixel format, `REQBUFS` + `mmap` the
//!    bitstream buffers, subscribe to `SOURCE_CHANGE`/`EOS`, `STREAMON`.
//! 3. Queue Annex-B / OBU access units (timestamps carry the pts).
//! 4. On `SOURCE_CHANGE`: `G_FMT(CAPTURE)`, optionally `S_FMT` to NV12/P010,
//!    `G_SELECTION(COMPOSE)` for the visible rectangle, `REQBUFS(CAPTURE)`
//!    sized by `V4L2_CID_MIN_BUFFERS_FOR_CAPTURE` + the engine's decode-ahead,
//!    `VIDIOC_EXPBUF` every plane to a DMA-BUF fd, queue all, `STREAMON`.
//! 5. Dequeued CAPTURE buffers become [`DmaBufFrame`]s; the buffer is
//!    re-queued when the frame's lease is dropped by the renderer.
//! 6. Drain with `V4L2_DEC_CMD_STOP` until a `V4L2_BUF_FLAG_LAST` buffer;
//!    seek with `STREAMOFF`/`STREAMON` on both queues.
//!
//! [verify] Frame (SM8650) specifics that need hardware confirmation:
//! * driver name/node: upstream `iris` (SM8650) vs downstream `venus`, and
//!   whether `/dev/video*` is accessible to a non-root gaming-mode user;
//! * the CAPTURE formats offered (`NV12` vs `QC08C` UBWC, `P010` vs `QC10C`
//!   for 10-bit) and whether Turnip imports the UBWC modifier;
//! * NV12 single-buffer chroma offset alignment (we align the luma height to
//!   32 lines on qcom drivers, matching `VENUS_Y_SCANLINES`);
//! * whether orphaned CAPTURE buffers (`REQBUFS(0)` while DMA-BUFs are still
//!   imported) are supported across a mid-stream resolution change;
//! * AV1 bitstream framing (temporal units in low-overhead OBU format).

pub mod sys;

use super::{
    drm, DecodedFrame, DecoderOptions, DecoderPath, DecoderRequest, DmaBufFrame, DmaBufPlane,
    FrameLease, VideoDecoder,
};
use crate::codec::BitstreamFilter;
use crate::error::{Result, VideoError};
use crate::packet::{CodecId, Packet, TrackDesc, TrackKind};
use crossbeam_channel::{Receiver, Sender};
use fp_core::MediaTime;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use sys::*;

/// A V4L2 M2M device and what it can decode.
#[derive(Debug, Clone, PartialEq)]
pub struct V4l2DeviceInfo {
    pub path: PathBuf,
    pub driver: String,
    pub card: String,
    /// Compressed formats accepted on the OUTPUT queue.
    pub coded_formats: Vec<u32>,
    /// Raw formats offered on the CAPTURE queue (may depend on the coded
    /// format; this is the list before any S_FMT).
    pub raw_formats: Vec<u32>,
    /// Max coded size per coded format, when ENUM_FRAMESIZES reports one.
    pub max_sizes: Vec<(u32, u32, u32)>,
}

impl V4l2DeviceInfo {
    pub fn is_decoder(&self) -> bool {
        !self.coded_formats.is_empty()
    }
    pub fn supports(&self, coded: u32, width: u32, height: u32) -> bool {
        self.coded_formats.contains(&coded)
            && self
                .max_sizes
                .iter()
                .find(|(f, _, _)| *f == coded)
                .is_none_or(|&(_, mw, mh)| {
                    (width <= mw && height <= mh) || (width <= mh && height <= mw)
                })
    }
    fn is_qcom(&self) -> bool {
        matches!(
            self.driver.as_str(),
            "qcom-venus" | "venus" | "iris" | "qcom-iris"
        ) || self.driver.contains("venus")
    }
}

/// V4L2 coded pixel format for a codec.
pub fn coded_format(codec: &CodecId) -> Option<u32> {
    match codec {
        CodecId::H264 => Some(PIX_FMT_H264),
        CodecId::Hevc => Some(PIX_FMT_HEVC),
        CodecId::Vp8 => Some(PIX_FMT_VP8),
        CodecId::Vp9 => Some(PIX_FMT_VP9),
        CodecId::Av1 => Some(PIX_FMT_AV1),
        _ => None,
    }
}

fn open_node(path: &Path) -> std::io::Result<OwnedFd> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::ErrorKind::InvalidInput)?;
    // SAFETY: valid C string; we own the returned fd.
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fd is a fresh, owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn enum_formats(fd: RawFd, type_: u32) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    for index in 0..64 {
        let mut d = FmtDesc {
            index,
            type_,
            flags: 0,
            description: [0; 32],
            pixelformat: 0,
            mbus_code: 0,
            reserved: [0; 3],
        };
        // SAFETY: d is a valid v4l2_fmtdesc.
        if unsafe { xioctl(fd, VIDIOC_ENUM_FMT, &mut d) }.is_err() {
            break;
        }
        out.push((d.pixelformat, d.flags));
    }
    out
}

fn max_frame_size(fd: RawFd, pixfmt: u32) -> Option<(u32, u32)> {
    let mut best: Option<(u32, u32)> = None;
    for index in 0..32 {
        let mut f = FrmSizeEnum {
            index,
            pixel_format: pixfmt,
            ..Default::default()
        };
        // SAFETY: valid v4l2_frmsizeenum.
        if unsafe { xioctl(fd, VIDIOC_ENUM_FRAMESIZES, &mut f) }.is_err() {
            break;
        }
        let (w, h) = if f.type_ == FRMSIZE_TYPE_DISCRETE {
            (f.u[0], f.u[1])
        } else {
            (f.u[1], f.u[4])
        };
        if best.is_none_or(|(bw, bh)| w as u64 * h as u64 > bw as u64 * bh as u64) {
            best = Some((w, h));
        }
        if f.type_ != FRMSIZE_TYPE_DISCRETE {
            break;
        }
    }
    best
}

/// Probe one device node.
pub fn probe_device(path: &Path) -> std::io::Result<V4l2DeviceInfo> {
    let fd = open_node(path)?;
    let raw = fd.as_raw_fd();
    // SAFETY: zeroed POD struct.
    let mut cap: Capability = unsafe { std::mem::zeroed() };
    // SAFETY: valid v4l2_capability.
    unsafe { xioctl(raw, VIDIOC_QUERYCAP, &mut cap)? };
    let caps = if cap.capabilities & CAP_DEVICE_CAPS != 0 {
        cap.device_caps
    } else {
        cap.capabilities
    };
    let mut info = V4l2DeviceInfo {
        path: path.to_path_buf(),
        driver: cstr(&cap.driver),
        card: cstr(&cap.card),
        coded_formats: Vec::new(),
        raw_formats: Vec::new(),
        max_sizes: Vec::new(),
    };
    if caps & CAP_VIDEO_M2M_MPLANE == 0 || caps & CAP_STREAMING == 0 {
        return Ok(info);
    }
    info.coded_formats = enum_formats(raw, BUF_TYPE_VIDEO_OUTPUT_MPLANE)
        .into_iter()
        .filter(|(_, flags)| flags & FMT_FLAG_COMPRESSED != 0)
        .map(|(f, _)| f)
        .collect();
    info.raw_formats = enum_formats(raw, BUF_TYPE_VIDEO_CAPTURE_MPLANE)
        .into_iter()
        .filter(|(_, flags)| flags & FMT_FLAG_COMPRESSED == 0)
        .map(|(f, _)| f)
        .collect();
    for &f in &info.coded_formats {
        if let Some((w, h)) = max_frame_size(raw, f) {
            info.max_sizes.push((f, w, h));
        }
    }
    Ok(info)
}

/// All V4L2 M2M decoders on the system (`/dev/video*`).
pub fn enumerate_devices() -> Vec<V4l2DeviceInfo> {
    let Ok(rd) = std::fs::read_dir("/dev") else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("video"))
        })
        .collect();
    paths.sort();
    paths
        .iter()
        .filter_map(|p| match probe_device(p) {
            Ok(i) if i.is_decoder() => Some(i),
            Ok(_) => None,
            Err(e) => {
                tracing::debug!("{}: {e}", p.display());
                None
            }
        })
        .collect()
}

/// An `mmap`ed OUTPUT (bitstream) buffer.
struct Mapping {
    ptr: *mut u8,
    len: usize,
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: ptr/len came from a successful mmap.
        unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.len) };
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct CaptureFormat {
    pixfmt: u32,
    coded_width: u32,
    coded_height: u32,
    visible: (u32, u32),
    num_planes: usize,
    bytesperline: [u32; 2],
    sizeimage: [u32; 2],
}

struct CaptureBuf {
    fds: Vec<Arc<OwnedFd>>,
    queued: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Waiting for the first SOURCE_CHANGE.
    Init,
    Decoding,
    Draining,
    Drained,
}

/// Timestamp bias so slightly negative pts survive the unsigned-ish timeval.
const TS_BIAS_US: i64 = 1_000_000_000;

fn pts_to_timeval(pts: MediaTime) -> libc::timeval {
    let v = pts.0 + TS_BIAS_US;
    libc::timeval {
        tv_sec: (v / 1_000_000) as _,
        tv_usec: (v % 1_000_000) as _,
    }
}

// `timeval` field widths differ between targets.
#[allow(clippy::unnecessary_cast)]
fn timeval_to_pts(tv: libc::timeval) -> MediaTime {
    MediaTime(tv.tv_sec as i64 * 1_000_000 + tv.tv_usec as i64 - TS_BIAS_US)
}

/// Visible-plane layout of a decoded CAPTURE buffer as DMA-BUF planes.
fn dmabuf_planes(
    fmt: &CaptureFormat,
    fds: &[RawFd],
    data_offsets: [u32; 2],
    qcom: bool,
) -> Vec<DmaBufPlane> {
    if fmt.num_planes >= 2 && fds.len() >= 2 {
        // NV12M: separate luma / chroma buffers.
        vec![
            DmaBufPlane {
                fd: fds[0],
                offset: data_offsets[0],
                pitch: fmt.bytesperline[0],
            },
            DmaBufPlane {
                fd: fds[1],
                offset: data_offsets[1],
                pitch: fmt.bytesperline[1],
            },
        ]
    } else {
        // Single buffer: chroma follows the (aligned) luma plane.
        let align = if qcom { 32 } else { 1 };
        let scanlines = fmt.coded_height.div_ceil(align) * align;
        let uv = data_offsets[0] + fmt.bytesperline[0] * scanlines;
        vec![
            DmaBufPlane {
                fd: fds[0],
                offset: data_offsets[0],
                pitch: fmt.bytesperline[0],
            },
            DmaBufPlane {
                fd: fds[0],
                offset: uv,
                pitch: fmt.bytesperline[0],
            },
        ]
    }
}

/// DRM fourcc + modifier for a V4L2 CAPTURE pixel format.
pub fn drm_format_for(pixfmt: u32) -> Option<(u32, u64)> {
    match pixfmt {
        PIX_FMT_NV12 | PIX_FMT_NV12M => Some((drm::FORMAT_NV12, drm::MOD_LINEAR)),
        PIX_FMT_P010 => Some((drm::FORMAT_P010, drm::MOD_LINEAR)),
        PIX_FMT_QC08C => Some((drm::FORMAT_NV12, drm::MOD_QCOM_COMPRESSED)),
        PIX_FMT_QC10C => Some((drm::FORMAT_P010, drm::MOD_QCOM_COMPRESSED)),
        _ => None,
    }
}

pub struct V4l2Decoder {
    fd: OwnedFd,
    info: V4l2DeviceInfo,
    filter: BitstreamFilter,
    out_bufs: Vec<Mapping>,
    out_free: Vec<usize>,
    capture: Vec<CaptureBuf>,
    cap_fmt: Option<CaptureFormat>,
    cap_streaming: bool,
    generation: u64,
    release_tx: Sender<(u64, usize)>,
    release_rx: Receiver<(u64, usize)>,
    state: State,
    pending_source_change: bool,
    bit_depth: u8,
    opts: DecoderOptions,
    scratch: Vec<u8>,
}

// SAFETY: the raw mmap pointers are only touched by the owning thread.
unsafe impl Send for V4l2Decoder {}

impl V4l2Decoder {
    /// Open the first device that supports `req`.
    pub fn open_best(req: &DecoderRequest, opts: &DecoderOptions) -> Result<Self> {
        let coded = coded_format(&req.codec)
            .ok_or_else(|| VideoError::NoDecoder(format!("{:?} not V4L2-decodable", req.codec)))?;
        let devices = enumerate_devices();
        if devices.is_empty() {
            return Err(VideoError::NoDecoder("no V4L2 M2M decoder devices".into()));
        }
        let mut errors = Vec::new();
        for d in devices
            .into_iter()
            .filter(|d| d.supports(coded, req.width, req.height))
        {
            match Self::open(d.clone(), req, opts) {
                Ok(dec) => return Ok(dec),
                Err(e) => errors.push(format!("{}: {e}", d.path.display())),
            }
        }
        Err(VideoError::NoDecoder(if errors.is_empty() {
            format!(
                "no V4L2 decoder supports {} at {}x{}",
                fourcc_str(coded),
                req.width,
                req.height
            )
        } else {
            errors.join("; ")
        }))
    }

    /// Open a specific device.
    pub fn open(info: V4l2DeviceInfo, req: &DecoderRequest, opts: &DecoderOptions) -> Result<Self> {
        let coded = coded_format(&req.codec)
            .ok_or_else(|| VideoError::Unsupported(format!("{:?}", req.codec)))?;
        let fd = open_node(&info.path)?;
        let raw = fd.as_raw_fd();
        let mut track = TrackDesc::new(0, TrackKind::Video, req.codec.clone());
        track.codec_private = req.codec_private.clone();
        let filter = BitstreamFilter::for_track(&track)?;

        // OUTPUT format.
        let mut f = Format::new(BUF_TYPE_VIDEO_OUTPUT_MPLANE);
        let mut pm = f.pix_mp();
        pm.width = req.width;
        pm.height = req.height;
        pm.pixelformat = coded;
        pm.field = FIELD_NONE;
        pm.num_planes = 1;
        // Bitstream buffer size: generous for 8K intra frames.
        let px = (req.width.max(1920) as u64 * req.height.max(1080) as u64) as u32;
        let size = (px / 2).clamp(2 << 20, 32 << 20);
        pm.plane_fmt[0].sizeimage = size;
        f.set_pix_mp(pm);
        // SAFETY: valid v4l2_format.
        unsafe { xioctl(raw, VIDIOC_S_FMT, &mut f) }
            .map_err(|e| VideoError::Device(format!("S_FMT(OUTPUT): {e}")))?;

        // Bitstream buffers.
        let mut rb = RequestBuffers {
            count: 6,
            type_: BUF_TYPE_VIDEO_OUTPUT_MPLANE,
            memory: MEMORY_MMAP,
            ..Default::default()
        };
        // SAFETY: valid v4l2_requestbuffers.
        unsafe { xioctl(raw, VIDIOC_REQBUFS, &mut rb) }
            .map_err(|e| VideoError::Device(format!("REQBUFS(OUTPUT): {e}")))?;
        let mut out_bufs = Vec::new();
        for i in 0..rb.count {
            let mut planes = [Plane::default()];
            let mut b =
                Buffer::new_mplane(BUF_TYPE_VIDEO_OUTPUT_MPLANE, MEMORY_MMAP, i, &mut planes);
            // SAFETY: b points at a live plane array.
            unsafe { xioctl(raw, VIDIOC_QUERYBUF, &mut b) }
                .map_err(|e| VideoError::Device(format!("QUERYBUF: {e}")))?;
            let len = planes[0].length as usize;
            // SAFETY: union read of mem_offset for MMAP memory.
            let off = unsafe { planes[0].m.mem_offset } as libc::off_t;
            // SAFETY: mapping a driver-provided buffer.
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    raw,
                    off,
                )
            };
            if ptr == libc::MAP_FAILED {
                return Err(VideoError::Device(format!(
                    "mmap OUTPUT buffer: {}",
                    std::io::Error::last_os_error()
                )));
            }
            out_bufs.push(Mapping {
                ptr: ptr as *mut u8,
                len,
            });
        }

        for ev in [EVENT_SOURCE_CHANGE, EVENT_EOS] {
            let mut s = EventSubscription {
                type_: ev,
                ..Default::default()
            };
            // SAFETY: valid subscription struct.
            if let Err(e) = unsafe { xioctl(raw, VIDIOC_SUBSCRIBE_EVENT, &mut s) } {
                if ev == EVENT_SOURCE_CHANGE {
                    return Err(VideoError::Device(format!(
                        "SUBSCRIBE_EVENT(SOURCE_CHANGE): {e}"
                    )));
                }
            }
        }
        let mut t: libc::c_int = BUF_TYPE_VIDEO_OUTPUT_MPLANE as libc::c_int;
        // SAFETY: int argument.
        unsafe { xioctl(raw, VIDIOC_STREAMON, &mut t) }
            .map_err(|e| VideoError::Device(format!("STREAMON(OUTPUT): {e}")))?;

        let (release_tx, release_rx) = crossbeam_channel::unbounded();
        let n = out_bufs.len();
        tracing::info!(
            "V4L2 decoder {} ({}) for {}",
            info.path.display(),
            info.driver,
            fourcc_str(coded)
        );
        Ok(V4l2Decoder {
            fd,
            info,
            filter,
            out_bufs,
            out_free: (0..n).collect(),
            capture: Vec::new(),
            cap_fmt: None,
            cap_streaming: false,
            generation: 0,
            release_tx,
            release_rx,
            state: State::Init,
            pending_source_change: false,
            bit_depth: req.bit_depth,
            opts: opts.clone(),
            scratch: Vec::new(),
        })
    }

    fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    fn reclaim_output(&mut self) {
        loop {
            let mut planes = [Plane::default()];
            let mut b =
                Buffer::new_mplane(BUF_TYPE_VIDEO_OUTPUT_MPLANE, MEMORY_MMAP, 0, &mut planes);
            // SAFETY: valid buffer + plane array.
            match unsafe { xioctl(self.raw(), VIDIOC_DQBUF, &mut b) } {
                Ok(()) => {
                    if !self.out_free.contains(&(b.index as usize)) {
                        self.out_free.push(b.index as usize);
                    }
                }
                Err(_) => break,
            }
        }
    }

    fn stream(&mut self, type_: u32, on: bool) -> Result<()> {
        let mut t = type_ as libc::c_int;
        let req = if on {
            VIDIOC_STREAMON
        } else {
            VIDIOC_STREAMOFF
        };
        // SAFETY: int argument.
        unsafe { xioctl(self.raw(), req, &mut t) }.map_err(|e| {
            VideoError::Device(format!(
                "STREAM{}({type_}): {e}",
                if on { "ON" } else { "OFF" }
            ))
        })
    }

    fn queue_capture(&mut self, index: usize) -> Result<()> {
        let np = self.cap_fmt.map_or(1, |f| f.num_planes);
        let mut planes = vec![Plane::default(); np];
        let mut b = Buffer::new_mplane(
            BUF_TYPE_VIDEO_CAPTURE_MPLANE,
            MEMORY_MMAP,
            index as u32,
            &mut planes,
        );
        // SAFETY: b points at a live plane array.
        unsafe { xioctl(self.raw(), VIDIOC_QBUF, &mut b) }
            .map_err(|e| VideoError::Device(format!("QBUF(CAPTURE): {e}")))?;
        self.capture[index].queued = true;
        Ok(())
    }

    /// (Re)allocate the CAPTURE queue after a source change.
    fn setup_capture(&mut self) -> Result<()> {
        let raw = self.raw();
        if self.cap_streaming {
            self.stream(BUF_TYPE_VIDEO_CAPTURE_MPLANE, false)?;
            self.cap_streaming = false;
        }
        if !self.capture.is_empty() {
            // Release the old buffers. Frames still held by the renderer keep
            // their DMA-BUFs alive (orphaned buffers).  [verify] on iris.
            self.capture.clear();
            let mut rb = RequestBuffers {
                count: 0,
                type_: BUF_TYPE_VIDEO_CAPTURE_MPLANE,
                memory: MEMORY_MMAP,
                ..Default::default()
            };
            // SAFETY: valid struct.
            if let Err(e) = unsafe { xioctl(raw, VIDIOC_REQBUFS, &mut rb) } {
                tracing::warn!("REQBUFS(CAPTURE, 0): {e}");
            }
        }
        self.generation += 1;

        let mut f = Format::new(BUF_TYPE_VIDEO_CAPTURE_MPLANE);
        // SAFETY: valid struct.
        unsafe { xioctl(raw, VIDIOC_G_FMT, &mut f) }
            .map_err(|e| VideoError::Device(format!("G_FMT(CAPTURE): {e}")))?;
        let mut pm = f.pix_mp();
        // Prefer a linear format the renderer can import.
        let ten_bit = self.bit_depth > 8;
        let mut wanted = vec![];
        if self.opts.allow_compressed_formats {
            wanted.push(if ten_bit {
                PIX_FMT_QC10C
            } else {
                PIX_FMT_QC08C
            });
        }
        if ten_bit {
            wanted.push(PIX_FMT_P010);
        }
        wanted.extend([PIX_FMT_NV12, PIX_FMT_NV12M]);
        let current = pm.pixelformat;
        if !wanted.first().is_some_and(|&w| w == current) {
            for w in wanted.iter().copied() {
                let mut tf = f;
                let mut tp = tf.pix_mp();
                tp.pixelformat = w;
                tf.set_pix_mp(tp);
                // SAFETY: valid struct.
                if unsafe { xioctl(raw, VIDIOC_S_FMT, &mut tf) }.is_ok()
                    && tf.pix_mp().pixelformat == w
                {
                    pm = tf.pix_mp();
                    break;
                }
            }
        }
        let pixfmt = pm.pixelformat;
        if drm_format_for(pixfmt).is_none() {
            return Err(VideoError::Device(format!(
                "unsupported CAPTURE format {}",
                fourcc_str(pixfmt)
            )));
        }
        let mut sel = Selection {
            type_: BUF_TYPE_VIDEO_CAPTURE_MPLANE,
            target: SEL_TGT_COMPOSE,
            ..Default::default()
        };
        let (cw, ch) = (pm.width, pm.height);
        // SAFETY: valid struct.
        let visible =
            if unsafe { xioctl(raw, VIDIOC_G_SELECTION, &mut sel) }.is_ok() && sel.r.width > 0 {
                (sel.r.width, sel.r.height)
            } else {
                (cw, ch)
            };
        let planes = pm.plane_fmt;
        let fmt = CaptureFormat {
            pixfmt,
            coded_width: cw,
            coded_height: ch,
            visible,
            num_planes: (pm.num_planes as usize).clamp(1, 2),
            bytesperline: [planes[0].bytesperline, planes[1].bytesperline],
            sizeimage: [planes[0].sizeimage, planes[1].sizeimage],
        };
        let mut ctrl = Control {
            id: CID_MIN_BUFFERS_FOR_CAPTURE,
            value: 0,
        };
        // SAFETY: valid struct.
        let min = if unsafe { xioctl(raw, VIDIOC_G_CTRL, &mut ctrl) }.is_ok() {
            ctrl.value.max(1) as u32
        } else {
            4
        };
        let count = min + self.opts.decode_ahead + 2;
        let mut rb = RequestBuffers {
            count,
            type_: BUF_TYPE_VIDEO_CAPTURE_MPLANE,
            memory: MEMORY_MMAP,
            ..Default::default()
        };
        // SAFETY: valid struct.
        unsafe { xioctl(raw, VIDIOC_REQBUFS, &mut rb) }
            .map_err(|e| VideoError::Device(format!("REQBUFS(CAPTURE): {e}")))?;
        self.cap_fmt = Some(fmt);
        for i in 0..rb.count {
            let mut fds = Vec::new();
            for p in 0..fmt.num_planes {
                let mut e = ExportBuffer {
                    type_: BUF_TYPE_VIDEO_CAPTURE_MPLANE,
                    index: i,
                    plane: p as u32,
                    flags: (libc::O_CLOEXEC | libc::O_RDONLY) as u32,
                    ..Default::default()
                };
                // SAFETY: valid struct.
                unsafe { xioctl(raw, VIDIOC_EXPBUF, &mut e) }
                    .map_err(|e| VideoError::Device(format!("EXPBUF: {e}")))?;
                // SAFETY: EXPBUF returned a new fd we now own.
                fds.push(Arc::new(unsafe { OwnedFd::from_raw_fd(e.fd) }));
            }
            self.capture.push(CaptureBuf { fds, queued: false });
        }
        for i in 0..self.capture.len() {
            self.queue_capture(i)?;
        }
        self.stream(BUF_TYPE_VIDEO_CAPTURE_MPLANE, true)?;
        self.cap_streaming = true;
        tracing::info!(
            "V4L2 CAPTURE {} {}x{} (visible {}x{}), {} buffers",
            fourcc_str(pixfmt),
            cw,
            ch,
            visible.0,
            visible.1,
            self.capture.len()
        );
        if self.state == State::Init {
            self.state = State::Decoding;
        }
        Ok(())
    }

    fn poll_events(&mut self) -> Result<()> {
        loop {
            let mut ev = Event::zeroed();
            // SAFETY: valid struct.
            if unsafe { xioctl(self.raw(), VIDIOC_DQEVENT, &mut ev) }.is_err() {
                return Ok(());
            }
            match ev.type_ {
                EVENT_SOURCE_CHANGE if ev.src_changes() & EVENT_SRC_CH_RESOLUTION != 0 => {
                    if self.cap_streaming {
                        // Mid-stream change: finish dequeuing up to LAST first.
                        self.pending_source_change = true;
                    } else {
                        self.setup_capture()?;
                    }
                }
                EVENT_EOS if self.state == State::Draining && !self.cap_streaming => {
                    self.state = State::Drained;
                }
                _ => {}
            }
        }
    }

    fn requeue_released(&mut self) -> Result<()> {
        while let Ok((gen, idx)) = self.release_rx.try_recv() {
            if gen == self.generation
                && idx < self.capture.len()
                && !self.capture[idx].queued
                && self.cap_streaming
            {
                self.queue_capture(idx)?;
            }
        }
        Ok(())
    }

    fn decoder_cmd(&mut self, cmd: u32) -> std::io::Result<()> {
        let mut c = DecoderCmd {
            cmd,
            flags: 0,
            raw: [0; 8],
        };
        // SAFETY: valid struct.
        unsafe { xioctl(self.raw(), VIDIOC_DECODER_CMD, &mut c) }
    }
}

impl VideoDecoder for V4l2Decoder {
    fn path(&self) -> DecoderPath {
        DecoderPath::Hardware {
            device: self.info.path.display().to_string(),
            driver: self.info.driver.clone(),
        }
    }

    fn send_packet(&mut self, pkt: &Packet) -> Result<bool> {
        self.reclaim_output();
        self.poll_events()?;
        let Some(idx) = self.out_free.pop() else {
            return Ok(false);
        };
        let mut data = std::mem::take(&mut self.scratch);
        self.filter.apply(&pkt.data, pkt.keyframe, &mut data)?;
        let buf = &self.out_bufs[idx];
        if data.len() > buf.len {
            self.out_free.push(idx);
            self.scratch = data;
            return Err(VideoError::Device(format!(
                "access unit of {} bytes exceeds bitstream buffer",
                self.scratch.len()
            )));
        }
        // SAFETY: the mapping is buf.len bytes and not queued to the driver.
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), buf.ptr, data.len()) };
        let mut planes = [Plane {
            bytesused: data.len() as u32,
            length: buf.len as u32,
            ..Default::default()
        }];
        let mut b = Buffer::new_mplane(
            BUF_TYPE_VIDEO_OUTPUT_MPLANE,
            MEMORY_MMAP,
            idx as u32,
            &mut planes,
        );
        b.timestamp = pts_to_timeval(pkt.pts);
        b.field = FIELD_NONE;
        if pkt.keyframe {
            b.flags |= BUF_FLAG_KEYFRAME;
        }
        self.scratch = data;
        // SAFETY: valid buffer + planes.
        if let Err(e) = unsafe { xioctl(self.raw(), VIDIOC_QBUF, &mut b) } {
            self.out_free.push(idx);
            return Err(VideoError::Device(format!("QBUF(OUTPUT): {e}")));
        }
        if self.state == State::Drained {
            self.state = State::Decoding;
        }
        Ok(true)
    }

    fn receive_frame(&mut self) -> Result<Option<DecodedFrame>> {
        self.poll_events()?;
        self.requeue_released()?;
        let Some(fmt) = self.cap_fmt else {
            return Ok(None);
        };
        if !self.cap_streaming {
            return Ok(None);
        }
        loop {
            let mut planes = vec![Plane::default(); fmt.num_planes];
            let mut b =
                Buffer::new_mplane(BUF_TYPE_VIDEO_CAPTURE_MPLANE, MEMORY_MMAP, 0, &mut planes);
            // SAFETY: valid buffer + planes.
            match unsafe { xioctl(self.raw(), VIDIOC_DQBUF, &mut b) } {
                Ok(()) => {}
                Err(e) if e.raw_os_error() == Some(libc::EAGAIN) => return Ok(None),
                Err(e) if e.raw_os_error() == Some(libc::EPIPE) => {
                    // Past the LAST buffer.
                    if self.pending_source_change {
                        self.pending_source_change = false;
                        self.setup_capture()?;
                        continue;
                    }
                    self.state = State::Drained;
                    return Ok(None);
                }
                Err(e) => return Err(VideoError::Device(format!("DQBUF(CAPTURE): {e}"))),
            }
            let idx = b.index as usize;
            if idx >= self.capture.len() {
                continue;
            }
            self.capture[idx].queued = false;
            let last = b.flags & BUF_FLAG_LAST != 0;
            let empty = planes[0].bytesused == 0;
            if b.flags & BUF_FLAG_ERROR != 0 || empty {
                if last {
                    if self.pending_source_change {
                        self.pending_source_change = false;
                        self.setup_capture()?;
                        continue;
                    }
                    self.state = State::Drained;
                    // Keep the buffer available for a restart after drain.
                    let _ = self.queue_capture(idx);
                    return Ok(None);
                }
                self.queue_capture(idx)?;
                continue;
            }
            let (fourcc, modifier) =
                drm_format_for(fmt.pixfmt).unwrap_or((drm::FORMAT_NV12, drm::MOD_LINEAR));
            let raw_fds: Vec<RawFd> = self.capture[idx]
                .fds
                .iter()
                .map(|f| f.as_raw_fd())
                .collect();
            let offsets = [
                planes[0].data_offset,
                planes.get(1).map_or(0, |p| p.data_offset),
            ];
            let held = self.capture[idx].fds.clone();
            let tx = self.release_tx.clone();
            let gen = self.generation;
            let frame = DmaBufFrame {
                planes: dmabuf_planes(&fmt, &raw_fds, offsets, self.info.is_qcom()),
                fourcc,
                modifier,
                width: fmt.visible.0,
                height: fmt.visible.1,
                coded_width: fmt.coded_width,
                coded_height: fmt.coded_height,
                pts: timeval_to_pts(b.timestamp),
                lease: Some(FrameLease::new(move || {
                    drop(held);
                    let _ = tx.send((gen, idx));
                })),
            };
            if last {
                if self.pending_source_change {
                    self.pending_source_change = false;
                    // The frame keeps its buffer alive through the realloc.
                    self.setup_capture()?;
                } else {
                    self.state = State::Drained;
                }
            }
            return Ok(Some(DecodedFrame::DmaBuf(frame)));
        }
    }

    fn drain(&mut self) -> Result<()> {
        if matches!(self.state, State::Draining | State::Drained) {
            return Ok(());
        }
        if !self.cap_streaming {
            // Nothing was ever decoded.
            self.state = State::Drained;
            return Ok(());
        }
        if let Err(e) = self.decoder_cmd(DEC_CMD_STOP) {
            tracing::warn!("V4L2_DEC_CMD_STOP failed ({e}); treating stream as drained");
            self.state = State::Drained;
            return Ok(());
        }
        self.state = State::Draining;
        Ok(())
    }

    fn is_drained(&self) -> bool {
        self.state == State::Drained
    }

    fn flush(&mut self) -> Result<()> {
        // Drop queued bitstream.
        self.stream(BUF_TYPE_VIDEO_OUTPUT_MPLANE, false)?;
        self.out_free = (0..self.out_bufs.len()).collect();
        self.stream(BUF_TYPE_VIDEO_OUTPUT_MPLANE, true)?;
        if self.cap_streaming {
            self.stream(BUF_TYPE_VIDEO_CAPTURE_MPLANE, false)?;
            for c in &mut self.capture {
                c.queued = false;
            }
            self.stream(BUF_TYPE_VIDEO_CAPTURE_MPLANE, true)?;
            // Pending release notices are superseded by the scan below
            // (drain first so a release racing the scan is never lost).
            while self.release_rx.try_recv().is_ok() {}
            // Re-queue every buffer not currently held by a frame; held ones
            // come back through their lease.
            for i in 0..self.capture.len() {
                if Arc::strong_count(&self.capture[i].fds[0]) == 1 {
                    self.queue_capture(i)?;
                }
            }
            self.state = State::Decoding;
        }
        if self.state == State::Drained || self.state == State::Draining {
            let _ = self.decoder_cmd(DEC_CMD_START);
            self.state = if self.cap_fmt.is_some() {
                State::Decoding
            } else {
                State::Init
            };
        }
        self.pending_source_change = false;
        Ok(())
    }

    fn wait(&mut self, timeout: Duration) {
        let mut p = libc::pollfd {
            fd: self.raw(),
            events: libc::POLLIN | libc::POLLOUT | libc::POLLPRI,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        unsafe { libc::poll(&mut p, 1, timeout.as_millis().min(i32::MAX as u128) as i32) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_roundtrip() {
        for us in [0i64, 1, 33_366, 3_600_000_123, -2002] {
            assert_eq!(timeval_to_pts(pts_to_timeval(MediaTime(us))), MediaTime(us));
        }
    }

    #[test]
    fn nv12_plane_layout() {
        let fmt = CaptureFormat {
            pixfmt: PIX_FMT_NV12,
            coded_width: 3840,
            coded_height: 2160,
            visible: (3840, 2160),
            num_planes: 1,
            bytesperline: [3840, 0],
            sizeimage: [3840 * 2160 * 3 / 2, 0],
        };
        let p = dmabuf_planes(&fmt, &[5], [0, 0], false);
        assert_eq!(
            p[1],
            DmaBufPlane {
                fd: 5,
                offset: 3840 * 2160,
                pitch: 3840
            }
        );
        // qcom: luma scanlines aligned to 32 (2160 → 2176).
        let fmt = CaptureFormat {
            coded_height: 2160,
            ..fmt
        };
        assert_eq!(
            dmabuf_planes(&fmt, &[5], [0, 0], true)[1].offset,
            3840 * 2176
        );
        let fmt = CaptureFormat {
            num_planes: 2,
            bytesperline: [3840, 3840],
            ..fmt
        };
        let p = dmabuf_planes(&fmt, &[5, 6], [0, 0], true);
        assert_eq!(p[1].fd, 6);
        assert_eq!(p[1].offset, 0);
    }

    #[test]
    fn format_mapping() {
        assert_eq!(coded_format(&CodecId::Hevc), Some(PIX_FMT_HEVC));
        assert_eq!(coded_format(&CodecId::Aac), None);
        assert_eq!(
            drm_format_for(PIX_FMT_QC10C),
            Some((drm::FORMAT_P010, drm::MOD_QCOM_COMPRESSED))
        );
        let info = V4l2DeviceInfo {
            path: "/dev/video0".into(),
            driver: "iris".into(),
            card: "Iris decoder".into(),
            coded_formats: vec![PIX_FMT_HEVC, PIX_FMT_AV1],
            raw_formats: vec![PIX_FMT_NV12],
            max_sizes: vec![(PIX_FMT_HEVC, 8192, 4320)],
        };
        assert!(info.supports(PIX_FMT_HEVC, 7680, 3840));
        assert!(!info.supports(PIX_FMT_HEVC, 8192, 8192));
        assert!(
            info.supports(PIX_FMT_AV1, 8192, 8192),
            "no size info → assume yes"
        );
        assert!(!info.supports(PIX_FMT_VP9, 1920, 1080));
        assert!(info.is_qcom());
    }

    #[test]
    fn enumerate_does_not_panic_without_devices() {
        // CI has no M2M devices; this must simply return an empty list.
        let _ = enumerate_devices();
    }
}
