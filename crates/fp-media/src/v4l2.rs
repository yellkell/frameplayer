//! Native V4L2 stateful (memory-to-memory) video decoder for H.264 and HEVC.
//!
//! FFmpeg's `*_v4l2m2m` wrappers cannot open large streams on the Steam
//! Frame: the qcom iris driver's admission check (`check_core_mbps_mbpf`)
//! multiplies the *new* session's macroblocks per frame by the number of
//! open decoder instances, Steam's own `vrlink` among them, so an 8192x4096
//! stream fails `VIDIOC_REQBUFS` with `ENOMEM` as soon as one other instance
//! exists, although the hardware decodes it at well over 60 fps.
//!
//! This decoder drives the device directly and works around the check:
//!
//! 1. The OUTPUT (bitstream) queue is configured for the stream's size. When
//!    `REQBUFS` fails with `ENOMEM`, the declared height is lowered to the
//!    largest 32-aligned height the driver accepts ([`capped_height`]).
//! 2. In that case an embedded IDR picture (the "primer") is queued ahead of
//!    the stream: a black HEVC frame of 8192x4080, which has just few enough
//!    macroblocks to pass the check when CAPTURE is allocated, yet needs the
//!    same 8192x4096 buffers as the stream ([`primer_matches`]).
//! 3. When the real stream's `SOURCE_CHANGE` arrives, the existing CAPTURE
//!    buffers are reused ([`can_reuse_capture`]): decoding resumes with
//!    `V4L2_DEC_CMD_START`, which skips the check `STREAMON` would run again.
//!
//! Reuse is only allowed when the new CAPTURE format is exactly the one the
//! buffers were allocated for. Resuming a much larger stream on buffers of a
//! smaller one (even with big enough CAPTURE buffers, e.g. imported DMA-BUFs)
//! makes the firmware write past its internal buffers: SMMU faults and a
//! firmware restart that takes down every other decoder session (tested).
//! Any other resolution change reallocates CAPTURE the standard way.
//!
//! Seeks never restart a queue (that would re-run the check): queued
//! bitstream from before the seek is decoded and its frames are dropped by
//! timestamp. Decoded NV12 frames are copied out of the driver's buffers into
//! pooled `AVFrame`s, so the rest of the pipeline sees ordinary NV12 frames
//! and the CAPTURE buffers go straight back to the decoder.

/// Bitstream (OUTPUT) buffers to request.
pub(crate) const OUTPUT_BUFFERS: u32 = 6;
/// Decoded-picture (CAPTURE) buffers to request (at least the driver minimum).
pub(crate) const CAPTURE_BUFFERS: u32 = 8;
/// Heights are lowered in steps the driver keeps without rounding up.
pub(crate) const HEIGHT_STEP: u32 = 32;
/// The smallest declared OUTPUT height worth trying: below this the
/// bitstream buffers get too small for large streams' packets.
pub(crate) const MIN_DECLARED_HEIGHT: u32 = 512;

/// Codecs this decoder handles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Codec {
    H264,
    Hevc,
}

impl Codec {
    pub(crate) fn from_ffmpeg(id: fp_ffmpeg_sys::AVCodecID) -> Option<Codec> {
        match id {
            fp_ffmpeg_sys::AV_CODEC_ID_H264 => Some(Codec::H264),
            fp_ffmpeg_sys::AV_CODEC_ID_HEVC => Some(Codec::Hevc),
            _ => None,
        }
    }

    pub(crate) fn fourcc(self) -> u32 {
        match self {
            Codec::H264 => fourcc(b"H264"),
            Codec::Hevc => fourcc(b"HEVC"),
        }
    }

    /// The bitstream filter turning MP4/MKV length-prefixed NAL units into
    /// the Annex B byte stream V4L2 decoders take.
    pub(crate) fn annexb_filter(self) -> &'static std::ffi::CStr {
        match self {
            Codec::H264 => c"h264_mp4toannexb",
            Codec::Hevc => c"hevc_mp4toannexb",
        }
    }

    /// The primer: one black IDR picture (Annex B with parameter sets, no
    /// SEI) and its size. Only HEVC has one; 8K H.264 is not something the
    /// Frame needs. Generated with
    /// `ffmpeg -f lavfi -i color=c=black:s=8192x4080:r=1 -frames:v 1
    /// -pix_fmt yuv420p -c:v libx265 -profile:v main -x265-params info=0 -f hevc`.
    pub(crate) fn primer(self) -> Option<(&'static [u8], (u32, u32))> {
        match self {
            Codec::H264 => None,
            Codec::Hevc => Some((
                include_bytes!("../assets/primer_8192x4080.hevc"),
                (8192, 4080),
            )),
        }
    }
}

pub(crate) const fn fourcc(c: &[u8; 4]) -> u32 {
    (c[0] as u32) | ((c[1] as u32) << 8) | ((c[2] as u32) << 16) | ((c[3] as u32) << 24)
}

/// 16x16 macroblocks in a frame, as the driver's admission check counts them.
pub(crate) fn macroblocks(width: u32, height: u32) -> u64 {
    width.div_ceil(16) as u64 * height.div_ceil(16) as u64
}

/// The largest height, a multiple of [`HEIGHT_STEP`] below `height` and at
/// least `min_height`, for which `fits` holds. `fits` is assumed monotonic
/// (if a height fits, every smaller one does too); it is called O(log n)
/// times. `None` when not even `min_height` fits.
pub(crate) fn capped_height(
    height: u32,
    min_height: u32,
    mut fits: impl FnMut(u32) -> bool,
) -> Option<u32> {
    let lo_steps = min_height.div_ceil(HEIGHT_STEP).max(1);
    // Strictly below `height`: the full height already failed.
    let hi_steps = height.saturating_sub(1) / HEIGHT_STEP;
    if hi_steps < lo_steps || !fits(lo_steps * HEIGHT_STEP) {
        return None;
    }
    let (mut good, mut bad) = (lo_steps, hi_steps + 1);
    while bad - good > 1 {
        let mid = good + (bad - good) / 2;
        if fits(mid * HEIGHT_STEP) {
            good = mid;
        } else {
            bad = mid;
        }
    }
    Some(good * HEIGHT_STEP)
}

/// Whether a primer of size `primer` can stand in for a stream of size
/// `stream`: same width, the same height once aligned to 64-line coding tree
/// blocks (so CAPTURE and internal buffers are identical), and fewer
/// macroblocks (so it passes the admission check where the stream fails).
pub(crate) fn primer_matches(stream: (u32, u32), primer: (u32, u32)) -> bool {
    stream.0 == primer.0
        && stream.1.next_multiple_of(64) == primer.1.next_multiple_of(64)
        && macroblocks(primer.0, primer.1) < macroblocks(stream.0, stream.1)
}

/// Whether CAPTURE buffers already allocated can serve the new stream format
/// after a resolution change, so decoding can resume without `REQBUFS`: the
/// coded size must be the one they were allocated for, every buffer large
/// enough, and enough of them.
pub(crate) fn can_reuse_capture(
    allocated_for: (u32, u32),
    have_count: u32,
    smallest_buffer: usize,
    new_format: (u32, u32),
    need_min_count: u32,
    need_size: usize,
) -> bool {
    allocated_for == new_format
        && have_count > 0
        && have_count >= need_min_count
        && need_size > 0
        && smallest_buffer >= need_size
}

/// How an NV12 picture sits in a single-plane CAPTURE buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Nv12Layout {
    /// Visible size.
    pub width: u32,
    pub height: u32,
    /// Bytes per row of both planes.
    pub stride: usize,
    /// Offset of the interleaved UV plane.
    pub uv_offset: usize,
}

impl Nv12Layout {
    /// From the CAPTURE format (`fmt_height` is the buffer's luma height,
    /// already aligned by the driver) and the visible (compose) size.
    pub(crate) fn new(
        stride: usize,
        fmt_height: u32,
        visible: (u32, u32),
        sizeimage: usize,
    ) -> Option<Nv12Layout> {
        let (width, height) = visible;
        let l = Nv12Layout {
            width,
            height,
            stride,
            uv_offset: stride * fmt_height.max(height) as usize,
        };
        (width > 0 && height > 0 && stride >= width as usize && l.end() <= sizeimage).then_some(l)
    }

    /// Bytes from the start of the buffer to the end of the visible chroma.
    pub(crate) fn end(&self) -> usize {
        self.uv_offset + self.stride * self.height.div_ceil(2) as usize
    }

    /// Bytes of a tightly stacked copy (Y rows then UV rows, same stride).
    pub(crate) fn copy_size(&self) -> usize {
        self.stride * (self.height as usize + self.height.div_ceil(2) as usize)
    }
}

/// Maps the V4L2 buffer timestamps we hand out to packet timing. Ids count
/// up from 1; 0 marks the primer. Seeks raise a floor below which ids are
/// stale.
#[derive(Default)]
pub(crate) struct Timestamps {
    next: u64,
    floor: u64,
    map: std::collections::BTreeMap<u64, (i64, i64)>,
}

/// Ids kept before the oldest entries are forgotten (packets that never
/// produced a frame, e.g. dropped by the decoder).
const TIMESTAMPS_KEPT: usize = 256;

impl Timestamps {
    pub(crate) fn insert(&mut self, pts: i64, duration: i64) -> u64 {
        self.next = self.next.max(self.floor) + 1;
        self.map.insert(self.next, (pts, duration));
        while self.map.len() > TIMESTAMPS_KEPT {
            self.map.pop_first();
        }
        self.next
    }

    /// Timing of a decoded frame; `None` for the primer and stale ids.
    pub(crate) fn get(&self, id: u64) -> Option<(i64, i64)> {
        if id <= self.floor {
            return None;
        }
        self.map.get(&id).copied()
    }

    /// Forgets everything queued so far (a seek).
    pub(crate) fn invalidate(&mut self) {
        self.floor = self.next;
        self.map.clear();
    }

    /// V4L2 timestamps are a `timeval`; ids travel in it unchanged.
    pub(crate) fn to_timeval(id: u64) -> (i64, i64) {
        ((id / 1_000_000) as i64, (id % 1_000_000) as i64)
    }

    pub(crate) fn from_timeval(sec: i64, usec: i64) -> u64 {
        (sec.max(0) as u64) * 1_000_000 + usec.max(0) as u64
    }
}

#[cfg(target_os = "linux")]
pub(crate) use linux::V4l2Decoder;

/// Whether the native decoder may be tried at all: Linux only, and
/// `FRAMEPLAYER_V4L2=0` turns it off.
pub(crate) fn enabled() -> bool {
    cfg!(target_os = "linux") && !matches!(std::env::var("FRAMEPLAYER_V4L2").as_deref(), Ok("0"))
}

#[cfg(target_os = "linux")]
mod sys {
    //! The slice of `linux/videodev2.h` this decoder uses (64-bit layouts).

    pub const BUF_TYPE_CAPTURE: u32 = 1;
    pub const BUF_TYPE_CAPTURE_MPLANE: u32 = 9;
    pub const BUF_TYPE_OUTPUT_MPLANE: u32 = 10;
    pub const MEMORY_MMAP: u32 = 1;
    pub const FIELD_NONE: u32 = 1;

    pub const CAP_VIDEO_M2M_MPLANE: u32 = 0x4000;
    pub const CAP_STREAMING: u32 = 0x0400_0000;
    pub const CAP_DEVICE_CAPS: u32 = 0x8000_0000;

    pub const BUF_FLAG_ERROR: u32 = 0x40;
    pub const BUF_FLAG_LAST: u32 = 0x0010_0000;

    pub const EVENT_EOS: u32 = 2;
    pub const EVENT_SOURCE_CHANGE: u32 = 5;

    pub const DEC_CMD_START: u32 = 0;
    pub const DEC_CMD_STOP: u32 = 1;

    pub const CID_MIN_BUFFERS_FOR_CAPTURE: u32 = 0x0098_0900 + 39;
    pub const SEL_TGT_COMPOSE: u32 = 0x0100;

    #[repr(C)]
    #[derive(Default)]
    pub struct Capability {
        pub driver: [u8; 16],
        pub card: [u8; 32],
        pub bus_info: [u8; 32],
        pub version: u32,
        pub capabilities: u32,
        pub device_caps: u32,
        pub reserved: [u32; 3],
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct FmtDesc {
        pub index: u32,
        pub type_: u32,
        pub flags: u32,
        pub description: [u8; 32],
        pub pixelformat: u32,
        pub mbus_code: u32,
        pub reserved: [u32; 3],
    }

    /// `struct v4l2_format`; the union starts at offset 8 on 64-bit.
    #[repr(C)]
    pub struct Format {
        pub type_: u32,
        pub pad: u32,
        pub raw: [u8; 200],
    }

    impl Format {
        pub fn new(type_: u32) -> Format {
            Format {
                type_,
                pad: 0,
                raw: [0; 200],
            }
        }
        fn u32_at(&self, o: usize) -> u32 {
            u32::from_ne_bytes([
                self.raw[o],
                self.raw[o + 1],
                self.raw[o + 2],
                self.raw[o + 3],
            ])
        }
        fn set_u32_at(&mut self, o: usize, v: u32) {
            self.raw[o..o + 4].copy_from_slice(&v.to_ne_bytes());
        }
        // struct v4l2_pix_format_mplane (packed): width, height, pixelformat,
        // field, colorspace, plane_fmt[8] {sizeimage, bytesperline, u16[6]},
        // num_planes, ...
        pub fn width(&self) -> u32 {
            self.u32_at(0)
        }
        pub fn height(&self) -> u32 {
            self.u32_at(4)
        }
        pub fn pixelformat(&self) -> u32 {
            self.u32_at(8)
        }
        pub fn sizeimage(&self, plane: usize) -> u32 {
            self.u32_at(20 + 20 * plane)
        }
        pub fn bytesperline(&self, plane: usize) -> u32 {
            self.u32_at(24 + 20 * plane)
        }
        pub fn num_planes(&self) -> u8 {
            self.raw[180]
        }
        pub fn set_size(&mut self, w: u32, h: u32) {
            self.set_u32_at(0, w);
            self.set_u32_at(4, h);
        }
        pub fn set_pixelformat(&mut self, f: u32) {
            self.set_u32_at(8, f);
        }
        pub fn set_field(&mut self, f: u32) {
            self.set_u32_at(12, f);
        }
        pub fn set_sizeimage(&mut self, plane: usize, v: u32) {
            self.set_u32_at(20 + 20 * plane, v);
        }
        pub fn set_num_planes(&mut self, n: u8) {
            self.raw[180] = n;
        }
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct RequestBuffers {
        pub count: u32,
        pub type_: u32,
        pub memory: u32,
        pub capabilities: u32,
        pub flags: u8,
        pub reserved: [u8; 3],
    }

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct Plane {
        pub bytesused: u32,
        pub length: u32,
        /// Union of mem_offset (u32), userptr and fd; mem_offset is the low
        /// 32 bits on little-endian machines.
        pub m: u64,
        pub data_offset: u32,
        pub reserved: [u32; 11],
    }

    #[repr(C)]
    pub struct Buffer {
        pub index: u32,
        pub type_: u32,
        pub bytesused: u32,
        pub flags: u32,
        pub field: u32,
        pub pad0: u32,
        /// struct timeval {tv_sec, tv_usec}.
        pub timestamp: [i64; 2],
        pub timecode: [u32; 4],
        pub sequence: u32,
        pub memory: u32,
        /// Pointer to the planes array for multi-planar buffers.
        pub m: usize,
        pub length: u32,
        pub reserved2: u32,
        pub request_fd: u32,
        pub pad1: u32,
    }

    impl Buffer {
        /// A multi-planar MMAP buffer descriptor pointing at `planes`.
        pub fn new(type_: u32, index: u32, planes: &mut [Plane]) -> Buffer {
            Buffer {
                index,
                type_,
                bytesused: 0,
                flags: 0,
                field: 0,
                pad0: 0,
                timestamp: [0; 2],
                timecode: [0; 4],
                sequence: 0,
                memory: MEMORY_MMAP,
                m: planes.as_mut_ptr() as usize,
                length: planes.len() as u32,
                reserved2: 0,
                request_fd: 0,
                pad1: 0,
            }
        }
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct EventSubscription {
        pub type_: u32,
        pub id: u32,
        pub flags: u32,
        pub reserved: [u32; 5],
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct Event {
        pub type_: u32,
        pub pad: u32,
        pub u: [u64; 8],
        pub pending: u32,
        pub sequence: u32,
        pub timestamp: [i64; 2],
        pub id: u32,
        pub reserved: [u32; 8],
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct DecoderCmd {
        pub cmd: u32,
        pub flags: u32,
        pub raw: [u32; 16],
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct Control {
        pub id: u32,
        pub value: i32,
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct Selection {
        pub type_: u32,
        pub target: u32,
        pub flags: u32,
        pub left: i32,
        pub top: i32,
        pub width: u32,
        pub height: u32,
        pub reserved: [u32; 9],
    }

    const WRITE: u64 = 1;
    const READ: u64 = 2;
    const fn ioc(dir: u64, nr: u64, size: usize) -> u64 {
        (dir << 30) | ((size as u64) << 16) | ((b'V' as u64) << 8) | nr
    }
    const fn ior<T>(nr: u64) -> u64 {
        ioc(READ, nr, std::mem::size_of::<T>())
    }
    const fn iow<T>(nr: u64) -> u64 {
        ioc(WRITE, nr, std::mem::size_of::<T>())
    }
    const fn iowr<T>(nr: u64) -> u64 {
        ioc(READ | WRITE, nr, std::mem::size_of::<T>())
    }

    pub const VIDIOC_QUERYCAP: u64 = ior::<Capability>(0);
    pub const VIDIOC_ENUM_FMT: u64 = iowr::<FmtDesc>(2);
    pub const VIDIOC_G_FMT: u64 = iowr::<Format>(4);
    pub const VIDIOC_S_FMT: u64 = iowr::<Format>(5);
    pub const VIDIOC_REQBUFS: u64 = iowr::<RequestBuffers>(8);
    pub const VIDIOC_QUERYBUF: u64 = iowr::<Buffer>(9);
    pub const VIDIOC_QBUF: u64 = iowr::<Buffer>(15);
    pub const VIDIOC_DQBUF: u64 = iowr::<Buffer>(17);
    pub const VIDIOC_STREAMON: u64 = iow::<i32>(18);
    pub const VIDIOC_STREAMOFF: u64 = iow::<i32>(19);
    pub const VIDIOC_G_CTRL: u64 = iowr::<Control>(27);
    pub const VIDIOC_DQEVENT: u64 = ior::<Event>(89);
    pub const VIDIOC_SUBSCRIBE_EVENT: u64 = iow::<EventSubscription>(90);
    pub const VIDIOC_G_SELECTION: u64 = iowr::<Selection>(94);
    pub const VIDIOC_DECODER_CMD: u64 = iowr::<DecoderCmd>(96);

    // The kernel's sizes; the ioctl numbers encode them.
    const _: () = assert!(std::mem::size_of::<Capability>() == 104);
    const _: () = assert!(std::mem::size_of::<FmtDesc>() == 64);
    const _: () = assert!(std::mem::size_of::<Format>() == 208);
    const _: () = assert!(std::mem::size_of::<RequestBuffers>() == 20);
    const _: () = assert!(std::mem::size_of::<Plane>() == 64);
    const _: () = assert!(std::mem::size_of::<Buffer>() == 88);
    const _: () = assert!(std::mem::size_of::<EventSubscription>() == 32);
    const _: () = assert!(std::mem::size_of::<Event>() == 136);
    const _: () = assert!(std::mem::size_of::<DecoderCmd>() == 72);
    const _: () = assert!(std::mem::size_of::<Control>() == 8);
    const _: () = assert!(std::mem::size_of::<Selection>() == 64);

    // Known values from videodev2.h, as a check on the encoding.
    const _: () = assert!(VIDIOC_QUERYCAP == 0x8068_5600);
    const _: () = assert!(VIDIOC_S_FMT == 0xc0d0_5605);
    const _: () = assert!(VIDIOC_QBUF == 0xc058_560f);
    const _: () = assert!(VIDIOC_STREAMON == 0x4004_5612);
    const _: () = assert!(VIDIOC_DQEVENT == 0x8088_5659);
}

#[cfg(target_os = "linux")]
mod linux {
    use super::sys::*;
    use super::*;
    use crate::decode::Packet;
    use crate::{Error, Result};
    use fp_ffmpeg_sys as ff;
    use std::collections::VecDeque;
    use std::ffi::c_void;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::time::{Duration, Instant};

    const NV12: u32 = fourcc(b"NV12");
    /// Give up when the device makes no progress for this long.
    const STALL: Duration = Duration::from_secs(5);

    fn ioctl<T>(fd: &OwnedFd, req: u64, arg: &mut T) -> std::io::Result<()> {
        loop {
            // SAFETY: `arg` is the kernel struct matching `req` (sizes are
            // asserted in `sys`); any pointers inside it outlive the call.
            let r = unsafe {
                libc::ioctl(
                    fd.as_raw_fd(),
                    req as libc::Ioctl,
                    arg as *mut T as *mut c_void,
                )
            };
            if r >= 0 {
                return Ok(());
            }
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EINTR) {
                return Err(e);
            }
        }
    }

    fn err(what: &str, e: std::io::Error) -> Error {
        Error::Unsupported(format!("V4L2 {what}: {e}"))
    }

    fn fourcc_str(f: u32) -> String {
        f.to_le_bytes().iter().map(|&b| b as char).collect()
    }

    /// An mmapped V4L2 buffer plane.
    struct Mapping {
        ptr: *mut u8,
        len: usize,
    }

    impl Mapping {
        fn new(fd: &OwnedFd, len: usize, offset: u32, write: bool) -> std::io::Result<Mapping> {
            let prot = if write {
                libc::PROT_READ | libc::PROT_WRITE
            } else {
                libc::PROT_READ
            };
            // SAFETY: maps a buffer the driver exported at `offset`.
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    prot,
                    libc::MAP_SHARED,
                    fd.as_raw_fd(),
                    offset as libc::off_t,
                )
            };
            if ptr == libc::MAP_FAILED {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Mapping {
                ptr: ptr as *mut u8,
                len,
            })
        }
    }

    impl Drop for Mapping {
        fn drop(&mut self) {
            // SAFETY: unmaps our own mapping.
            unsafe { libc::munmap(self.ptr as *mut c_void, self.len) };
        }
    }

    struct Capture {
        bufs: Vec<Mapping>,
        /// Coded size (CAPTURE format) the buffers were allocated for.
        allocated_for: (u32, u32),
        layout: Nv12Layout,
        /// Buffer we hold after a LAST flag until decoding resumes.
        held: Option<u32>,
    }

    /// One dequeued CAPTURE buffer.
    struct Dequeued {
        index: u32,
        id: u64,
        bytesused: usize,
        flags: u32,
    }

    pub(crate) struct V4l2Decoder {
        // Field order matters for drop: mappings before the fd.
        out_bufs: Vec<Mapping>,
        cap: Option<Capture>,
        fd: OwnedFd,
        pub device: String,
        codec: Codec,
        stream_size: (u32, u32),
        /// The primer trick is active (declared OUTPUT size was lowered).
        primed: bool,
        bsf: *mut ff::AVBSFContext,
        out_free: Vec<u32>,
        /// Filtered packets waiting for a free OUTPUT buffer, with their ids.
        pending: VecDeque<(Packet, u64)>,
        ts: Timestamps,
        /// After a seek, frames before the first new packet's pts are
        /// leading pictures of the new random access point: dropped.
        pts_floor: Option<i64>,
        arm_floor: bool,
        /// A resolution change was signalled; waiting for the LAST buffer.
        drc_pending: bool,
        /// End of stream: bitstream filter drained / STOP sent / LAST seen.
        eos_input: bool,
        stop_sent: bool,
        finished: bool,
        pool: *mut ff::AVBufferPool,
        pool_size: usize,
        color: (
            ff::AVColorRange,
            ff::AVColorSpace,
            ff::AVColorTransferCharacteristic,
            ff::AVColorPrimaries,
        ),
        pub frames: u64,
    }

    // SAFETY: one thread drives a decoder at a time; the raw pointers are
    // owned FFmpeg objects.
    unsafe impl Send for V4l2Decoder {}

    impl Drop for V4l2Decoder {
        fn drop(&mut self) {
            for t in [BUF_TYPE_OUTPUT_MPLANE, BUF_TYPE_CAPTURE_MPLANE] {
                let mut t = t as i32;
                let _ = ioctl(&self.fd, VIDIOC_STREAMOFF, &mut t);
            }
            // SAFETY: frees our own filter and pool (the pool lives on until
            // frames still holding its buffers are freed).
            unsafe {
                ff::av_bsf_free(&mut self.bsf);
                ff::av_buffer_pool_uninit(&mut self.pool);
            }
        }
    }

    fn query_caps(fd: &OwnedFd) -> Option<u32> {
        let mut c = Capability::default();
        ioctl(fd, VIDIOC_QUERYCAP, &mut c).ok()?;
        Some(if c.capabilities & CAP_DEVICE_CAPS != 0 {
            c.device_caps
        } else {
            c.capabilities
        })
    }

    fn formats(fd: &OwnedFd, type_: u32) -> Vec<u32> {
        let mut out = Vec::new();
        for index in 0..64 {
            let mut d = FmtDesc {
                index,
                type_,
                ..Default::default()
            };
            if ioctl(fd, VIDIOC_ENUM_FMT, &mut d).is_err() {
                break;
            }
            out.push(d.pixelformat);
        }
        out
    }

    fn open_node(path: &str) -> std::io::Result<OwnedFd> {
        let c = std::ffi::CString::new(path).unwrap_or_default();
        // SAFETY: plain open(2) of a device node.
        let fd = unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: we own the new descriptor.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// Whether `fd` is a multi-planar M2M decoder for `codec` with NV12 out.
    fn suitable(fd: &OwnedFd, codec: Codec) -> bool {
        let Some(caps) = query_caps(fd) else {
            return false;
        };
        caps & CAP_VIDEO_M2M_MPLANE != 0
            && caps & CAP_STREAMING != 0
            && formats(fd, BUF_TYPE_OUTPUT_MPLANE).contains(&codec.fourcc())
            && formats(fd, BUF_TYPE_CAPTURE_MPLANE).contains(&NV12)
    }

    /// `/dev/video-dec0` (the Frame's decoder), else the first `/dev/video*`
    /// M2M device decoding `codec`. `FRAMEPLAYER_V4L2_DEVICE` overrides.
    fn find_device(codec: Codec) -> Result<(String, OwnedFd)> {
        let mut paths: Vec<String> = Vec::new();
        if let Ok(p) = std::env::var("FRAMEPLAYER_V4L2_DEVICE") {
            paths.push(p);
        } else {
            paths.push("/dev/video-dec0".into());
            let mut nodes: Vec<(u32, String)> = std::fs::read_dir("/dev")
                .map(|d| {
                    d.flatten()
                        .filter_map(|e| {
                            let n = e.file_name().to_string_lossy().into_owned();
                            let num = n.strip_prefix("video")?.parse().ok()?;
                            Some((num, format!("/dev/{n}")))
                        })
                        .collect()
                })
                .unwrap_or_default();
            nodes.sort();
            paths.extend(nodes.into_iter().map(|(_, p)| p));
        }
        for p in paths {
            if let Ok(fd) = open_node(&p)
                && suitable(&fd, codec)
            {
                return Ok((p, fd));
            }
        }
        Err(Error::Unsupported(format!(
            "no V4L2 decoder for {codec:?} with NV12 output"
        )))
    }

    impl V4l2Decoder {
        /// Opens the device and configures the bitstream queue. CAPTURE is
        /// set up later, when the decoder reports the stream's format.
        pub(crate) fn open(
            par: *const ff::AVCodecParameters,
            time_base: ff::AVRational,
        ) -> Result<V4l2Decoder> {
            // SAFETY: par belongs to an open stream.
            let p = unsafe { &*par };
            let codec = Codec::from_ffmpeg(p.codec_id)
                .ok_or_else(|| Error::Unsupported("codec not handled by V4L2".into()))?;
            let eight_bit_420 = [
                ff::AV_PIX_FMT_NONE,
                ff::AV_PIX_FMT_YUV420P,
                ff::AV_PIX_FMT_YUVJ420P,
            ];
            if !eight_bit_420.contains(&p.format) {
                return Err(Error::Unsupported(format!(
                    "V4L2 path takes 8-bit 4:2:0 only (pixel format {})",
                    p.format
                )));
            }
            if p.width <= 0 || p.height <= 0 {
                return Err(Error::Unsupported("V4L2: stream size unknown".into()));
            }
            let stream_size = (p.width as u32, p.height as u32);
            let (device, fd) = find_device(codec)?;
            let mut dec = V4l2Decoder {
                out_bufs: Vec::new(),
                cap: None,
                fd,
                device,
                codec,
                stream_size,
                primed: false,
                bsf: std::ptr::null_mut(),
                out_free: Vec::new(),
                pending: VecDeque::new(),
                ts: Timestamps::default(),
                pts_floor: None,
                arm_floor: false,
                drc_pending: false,
                eos_input: false,
                stop_sent: false,
                finished: false,
                pool: std::ptr::null_mut(),
                pool_size: 0,
                color: (p.color_range, p.color_space, p.color_trc, p.color_primaries),
                frames: 0,
            };
            dec.init_bsf(par, time_base)?;
            for t in [EVENT_SOURCE_CHANGE, EVENT_EOS] {
                let mut s = EventSubscription {
                    type_: t,
                    ..Default::default()
                };
                ioctl(&dec.fd, VIDIOC_SUBSCRIBE_EVENT, &mut s)
                    .map_err(|e| err("subscribe event", e))?;
            }
            dec.setup_output()?;
            let mut t = BUF_TYPE_OUTPUT_MPLANE as i32;
            ioctl(&dec.fd, VIDIOC_STREAMON, &mut t).map_err(|e| err("STREAMON output", e))?;
            if dec.primed
                && let Some((primer, _)) = codec.primer()
            {
                dec.queue_bitstream(primer, 0)?;
            }
            log::info!(
                "V4L2 decoder {} for {codec:?} {}x{}{}",
                dec.device,
                stream_size.0,
                stream_size.1,
                if dec.primed {
                    " (admission workaround: primer)"
                } else {
                    ""
                }
            );
            Ok(dec)
        }

        fn init_bsf(
            &mut self,
            par: *const ff::AVCodecParameters,
            time_base: ff::AVRational,
        ) -> Result<()> {
            // SAFETY: standard bitstream filter setup on our own context.
            unsafe {
                let f = ff::av_bsf_get_by_name(self.codec.annexb_filter().as_ptr());
                if f.is_null() {
                    return Err(Error::Unsupported("Annex B bitstream filter missing".into()));
                }
                crate::check(ff::av_bsf_alloc(f, &mut self.bsf), "bsf alloc")?;
                crate::check(
                    ff::avcodec_parameters_copy((*self.bsf).par_in, par),
                    "bsf parameters",
                )?;
                (*self.bsf).time_base_in = time_base;
                crate::check(ff::av_bsf_init(self.bsf), "bsf init")?;
            }
            Ok(())
        }

        /// S_FMT + REQBUFS on the OUTPUT queue with the declared size.
        fn try_output(&self, width: u32, height: u32) -> std::io::Result<u32> {
            let mut f = Format::new(BUF_TYPE_OUTPUT_MPLANE);
            f.set_size(width, height);
            f.set_pixelformat(self.codec.fourcc());
            f.set_field(FIELD_NONE);
            f.set_num_planes(1);
            ioctl(&self.fd, VIDIOC_S_FMT, &mut f)?;
            let mut r = RequestBuffers {
                count: OUTPUT_BUFFERS,
                type_: BUF_TYPE_OUTPUT_MPLANE,
                memory: MEMORY_MMAP,
                ..Default::default()
            };
            ioctl(&self.fd, VIDIOC_REQBUFS, &mut r)?;
            Ok(r.count)
        }

        fn free_output(&self) {
            let mut r = RequestBuffers {
                count: 0,
                type_: BUF_TYPE_OUTPUT_MPLANE,
                memory: MEMORY_MMAP,
                ..Default::default()
            };
            let _ = ioctl(&self.fd, VIDIOC_REQBUFS, &mut r);
        }

        fn setup_output(&mut self) -> Result<()> {
            let (w, h) = self.stream_size;
            let count = match self.try_output(w, h) {
                Ok(n) => n,
                Err(e) if e.raw_os_error() == Some(libc::ENOMEM) => {
                    // The admission check: retry with a lower declared size
                    // and open CAPTURE through the primer, if one fits.
                    let Some((_, primer)) = self
                        .codec
                        .primer()
                        .filter(|(_, size)| primer_matches((w, h), *size))
                    else {
                        return Err(err("REQBUFS output", e));
                    };
                    let mut fits = |cand: u32| {
                        let ok = self.try_output(w, cand).is_ok();
                        if ok {
                            self.free_output();
                        }
                        ok
                    };
                    let capped = capped_height(h, MIN_DECLARED_HEIGHT.min(h), &mut fits)
                        .ok_or_else(|| {
                            Error::Unsupported(format!(
                                "V4L2 REQBUFS output {w}x{h}: {e} (no smaller size admitted)"
                            ))
                        })?;
                    if capped + HEIGHT_STEP <= primer.1 {
                        // Not even the primer would be admitted.
                        return Err(Error::Unsupported(format!(
                            "V4L2 {w}x{h}: too many other decoder sessions ({e}; \
                             only {w}x{capped} admitted)"
                        )));
                    }
                    log::info!(
                        "V4L2 {w}x{h} refused by the driver's admission check ({e}); \
                         declaring {w}x{capped} and priming"
                    );
                    self.primed = true;
                    self.try_output(w, capped)
                        .map_err(|e| err("REQBUFS output (lowered)", e))?
                }
                Err(e) => return Err(err("REQBUFS output", e)),
            };
            for i in 0..count {
                let mut planes = [Plane::default(); 1];
                let mut b = Buffer::new(BUF_TYPE_OUTPUT_MPLANE, i, &mut planes);
                ioctl(&self.fd, VIDIOC_QUERYBUF, &mut b).map_err(|e| err("QUERYBUF output", e))?;
                let m = Mapping::new(
                    &self.fd,
                    planes[0].length as usize,
                    planes[0].m as u32,
                    true,
                )
                .map_err(|e| err("mmap output", e))?;
                self.out_bufs.push(m);
                self.out_free.push(i);
            }
            self.out_free.reverse();
            Ok(())
        }

        /// Copies `data` into a free OUTPUT buffer and queues it.
        fn queue_bitstream(&mut self, data: &[u8], id: u64) -> Result<()> {
            let Some(index) = self.out_free.pop() else {
                return Err(Error::Unsupported("V4L2: no free output buffer".into()));
            };
            let m = &self.out_bufs[index as usize];
            if data.len() > m.len {
                return Err(Error::Unsupported(format!(
                    "V4L2: packet of {} bytes exceeds the {} byte bitstream buffer",
                    data.len(),
                    m.len
                )));
            }
            // SAFETY: the buffer is mapped writable for m.len bytes and is
            // not queued (owned by us until QBUF).
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), m.ptr, data.len()) };
            let mut planes = [Plane {
                bytesused: data.len() as u32,
                length: m.len as u32,
                ..Default::default()
            }];
            let mut b = Buffer::new(BUF_TYPE_OUTPUT_MPLANE, index, &mut planes);
            let (sec, usec) = Timestamps::to_timeval(id);
            b.timestamp = [sec, usec];
            if let Err(e) = ioctl(&self.fd, VIDIOC_QBUF, &mut b) {
                self.out_free.push(index);
                return Err(err("QBUF output", e));
            }
            Ok(())
        }

        /// Reclaims consumed OUTPUT buffers and queues pending packets.
        fn pump_output(&mut self) -> Result<()> {
            loop {
                let mut planes = [Plane::default(); 1];
                let mut b = Buffer::new(BUF_TYPE_OUTPUT_MPLANE, 0, &mut planes);
                match ioctl(&self.fd, VIDIOC_DQBUF, &mut b) {
                    Ok(()) => self.out_free.push(b.index),
                    Err(e) if e.raw_os_error() == Some(libc::EAGAIN) => break,
                    Err(e) if e.raw_os_error() == Some(libc::EPIPE) => break,
                    Err(e) => return Err(err("DQBUF output", e)),
                }
            }
            while !self.out_free.is_empty() {
                let Some((pkt, id)) = self.pending.pop_front() else {
                    break;
                };
                // SAFETY: a valid filtered packet.
                let data = unsafe {
                    let p = &*pkt.as_ptr();
                    std::slice::from_raw_parts(p.data, p.size.max(0) as usize)
                };
                self.queue_bitstream(data, id)?;
            }
            if self.eos_input && self.pending.is_empty() && !self.stop_sent {
                let mut c = DecoderCmd {
                    cmd: DEC_CMD_STOP,
                    ..Default::default()
                };
                ioctl(&self.fd, VIDIOC_DECODER_CMD, &mut c).map_err(|e| err("STOP", e))?;
                self.stop_sent = true;
            }
            Ok(())
        }

        /// Moves everything the bitstream filter has ready into `pending`.
        fn drain_bsf(&mut self) -> Result<()> {
            loop {
                let out = Packet::new();
                // SAFETY: our filter; out is a fresh packet.
                let r = unsafe { ff::av_bsf_receive_packet(self.bsf, out.as_ptr()) };
                if r == ff::AVERROR_EAGAIN || r == ff::AVERROR_EOF_ {
                    return Ok(());
                }
                crate::check(r, "bitstream filter")?;
                // SAFETY: a valid packet from the filter.
                let (pts, dts, duration) = unsafe {
                    let p = &*out.as_ptr();
                    (p.pts, p.dts, p.duration)
                };
                let pts = if pts == ff::AV_NOPTS_VALUE { dts } else { pts };
                if self.arm_floor {
                    self.arm_floor = false;
                    self.pts_floor = (pts != ff::AV_NOPTS_VALUE).then_some(pts);
                }
                let id = self.ts.insert(pts, duration);
                self.pending.push_back((out, id));
            }
        }

        /// Sends a packet, or end of stream when `pkt` is null. Returns false
        /// when the decoder is full: receive frames, then send it again.
        pub(crate) fn send(&mut self, pkt: *const ff::AVPacket) -> Result<bool> {
            self.pump_output()?;
            if !self.pending.is_empty() {
                return Ok(false);
            }
            if pkt.is_null() {
                if !self.eos_input {
                    // SAFETY: a null packet signals EOF to the filter.
                    unsafe { ff::av_bsf_send_packet(self.bsf, std::ptr::null_mut()) };
                    self.drain_bsf()?;
                    self.eos_input = true;
                }
                self.pump_output()?;
                return Ok(true);
            }
            if self.eos_input {
                return Ok(true); // after EOS until a flush
            }
            let copy = Packet::new();
            // SAFETY: references the caller's packet; the filter takes over
            // our reference.
            unsafe {
                crate::check(ff::av_packet_ref(copy.as_ptr(), pkt), "packet ref")?;
                crate::check(ff::av_bsf_send_packet(self.bsf, copy.as_ptr()), "bsf send")?;
            }
            self.drain_bsf()?;
            self.pump_output()?;
            Ok(true)
        }

        fn dequeue_events(&mut self) -> Result<()> {
            loop {
                let mut ev = Event::default();
                match ioctl(&self.fd, VIDIOC_DQEVENT, &mut ev) {
                    Ok(()) => {}
                    Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(()),
                    Err(e) => return Err(err("DQEVENT", e)),
                }
                if ev.type_ == EVENT_SOURCE_CHANGE {
                    if self.cap.is_none() {
                        self.setup_capture()?;
                    } else {
                        log::debug!("V4L2 source change");
                        self.drc_pending = true;
                    }
                }
            }
        }

        fn g_fmt_capture(&self) -> Result<Format> {
            let mut f = Format::new(BUF_TYPE_CAPTURE_MPLANE);
            ioctl(&self.fd, VIDIOC_G_FMT, &mut f).map_err(|e| err("G_FMT capture", e))?;
            Ok(f)
        }

        fn min_capture_buffers(&self) -> u32 {
            let mut c = Control {
                id: CID_MIN_BUFFERS_FOR_CAPTURE,
                value: 0,
            };
            match ioctl(&self.fd, VIDIOC_G_CTRL, &mut c) {
                Ok(()) if c.value > 0 => c.value as u32,
                _ => 1,
            }
        }

        /// The CAPTURE layout for format `f` (visible size from COMPOSE).
        fn layout(&self, f: &Format) -> Result<Nv12Layout> {
            if f.pixelformat() != NV12 || f.num_planes() != 1 {
                return Err(Error::Unsupported(format!(
                    "V4L2 capture format {} with {} planes",
                    fourcc_str(f.pixelformat()),
                    f.num_planes()
                )));
            }
            let mut s = Selection {
                type_: BUF_TYPE_CAPTURE,
                target: SEL_TGT_COMPOSE,
                ..Default::default()
            };
            let visible = match ioctl(&self.fd, VIDIOC_G_SELECTION, &mut s) {
                Ok(()) if s.width > 0 && s.height > 0 => (s.width, s.height),
                _ => (f.width(), f.height()),
            };
            Nv12Layout::new(
                f.bytesperline(0) as usize,
                f.height(),
                visible,
                f.sizeimage(0) as usize,
            )
            .ok_or_else(|| {
                Error::Unsupported(format!(
                    "V4L2 capture layout {}x{} stride {} size {}",
                    visible.0,
                    visible.1,
                    f.bytesperline(0),
                    f.sizeimage(0)
                ))
            })
        }

        /// First source change: choose NV12, allocate, map and queue CAPTURE.
        fn setup_capture(&mut self) -> Result<()> {
            let mut f = self.g_fmt_capture()?;
            if f.pixelformat() != NV12 {
                f.set_pixelformat(NV12);
                ioctl(&self.fd, VIDIOC_S_FMT, &mut f).map_err(|e| err("S_FMT capture", e))?;
                f = self.g_fmt_capture()?;
            }
            let layout = self.layout(&f)?;
            let min = self.min_capture_buffers();
            let mut r = RequestBuffers {
                count: CAPTURE_BUFFERS.max(min),
                type_: BUF_TYPE_CAPTURE_MPLANE,
                memory: MEMORY_MMAP,
                ..Default::default()
            };
            ioctl(&self.fd, VIDIOC_REQBUFS, &mut r).map_err(|e| err("REQBUFS capture", e))?;
            let mut bufs = Vec::new();
            for i in 0..r.count {
                let mut planes = [Plane::default(); 1];
                let mut b = Buffer::new(BUF_TYPE_CAPTURE_MPLANE, i, &mut planes);
                ioctl(&self.fd, VIDIOC_QUERYBUF, &mut b)
                    .map_err(|e| err("QUERYBUF capture", e))?;
                bufs.push(
                    Mapping::new(
                        &self.fd,
                        planes[0].length as usize,
                        planes[0].m as u32,
                        false,
                    )
                    .map_err(|e| err("mmap capture", e))?,
                );
            }
            self.cap = Some(Capture {
                bufs,
                allocated_for: (f.width(), f.height()),
                layout,
                held: None,
            });
            for i in 0..r.count {
                self.queue_capture(i)?;
            }
            let mut t = BUF_TYPE_CAPTURE_MPLANE as i32;
            ioctl(&self.fd, VIDIOC_STREAMON, &mut t).map_err(|e| err("STREAMON capture", e))?;
            log::info!(
                "V4L2 capture {}x{} (visible {}x{}, stride {}) x{} buffers of {} bytes",
                f.width(),
                f.height(),
                layout.width,
                layout.height,
                layout.stride,
                r.count,
                f.sizeimage(0)
            );
            Ok(())
        }

        fn queue_capture(&self, index: u32) -> Result<()> {
            let Some(cap) = &self.cap else {
                return Ok(());
            };
            let mut planes = [Plane {
                length: cap.bufs[index as usize].len as u32,
                ..Default::default()
            }];
            let mut b = Buffer::new(BUF_TYPE_CAPTURE_MPLANE, index, &mut planes);
            ioctl(&self.fd, VIDIOC_QBUF, &mut b).map_err(|e| err("QBUF capture", e))
        }

        fn dequeue_capture(&self) -> Result<Option<Dequeued>> {
            if self.cap.is_none() {
                return Ok(None);
            }
            let mut planes = [Plane::default(); 1];
            let mut b = Buffer::new(BUF_TYPE_CAPTURE_MPLANE, 0, &mut planes);
            match ioctl(&self.fd, VIDIOC_DQBUF, &mut b) {
                Ok(()) => Ok(Some(Dequeued {
                    index: b.index,
                    id: Timestamps::from_timeval(b.timestamp[0], b.timestamp[1]),
                    bytesused: planes[0].bytesused as usize,
                    flags: b.flags,
                })),
                Err(e) if e.raw_os_error() == Some(libc::EAGAIN) => Ok(None),
                // After LAST, until decoding resumes.
                Err(e) if e.raw_os_error() == Some(libc::EPIPE) => Ok(None),
                Err(e) => Err(err("DQBUF capture", e)),
            }
        }

        /// Resumes decoding after a LAST buffer (resolution change or drain).
        fn resume(&mut self) -> Result<()> {
            let mut c = DecoderCmd {
                cmd: DEC_CMD_START,
                ..Default::default()
            };
            ioctl(&self.fd, VIDIOC_DECODER_CMD, &mut c).map_err(|e| err("START", e))?;
            if let Some(i) = self.cap.as_mut().and_then(|c| c.held.take()) {
                self.queue_capture(i)?;
            }
            Ok(())
        }

        /// The LAST buffer of a resolution change: reuse the buffers when
        /// they fit (no admission check), else reallocate.
        fn change_resolution(&mut self) -> Result<()> {
            self.drc_pending = false;
            let f = self.g_fmt_capture()?;
            let min = self.min_capture_buffers();
            let (allocated_for, count, smallest) = self
                .cap
                .as_ref()
                .map(|c| {
                    (
                        c.allocated_for,
                        c.bufs.len() as u32,
                        c.bufs.iter().map(|m| m.len).min().unwrap_or(0),
                    )
                })
                .unwrap_or(((0, 0), 0, 0));
            let need = f.sizeimage(0) as usize;
            let new_format = (f.width(), f.height());
            if f.pixelformat() == NV12
                && can_reuse_capture(allocated_for, count, smallest, new_format, min, need)
            {
                let layout = self.layout(&f)?;
                if layout.end() <= smallest {
                    log::info!(
                        "V4L2 stream now {}x{}: reusing {count} capture buffers",
                        layout.width,
                        layout.height
                    );
                    if let Some(c) = self.cap.as_mut() {
                        c.layout = layout;
                    }
                    return self.resume();
                }
            }
            log::info!(
                "V4L2 stream now {}x{} {}: reallocating capture ({count} x {smallest} bytes, \
                 need {min} x {need})",
                f.width(),
                f.height(),
                fourcc_str(f.pixelformat())
            );
            let mut t = BUF_TYPE_CAPTURE_MPLANE as i32;
            ioctl(&self.fd, VIDIOC_STREAMOFF, &mut t).map_err(|e| err("STREAMOFF capture", e))?;
            self.cap = None; // unmaps
            let mut r = RequestBuffers {
                count: 0,
                type_: BUF_TYPE_CAPTURE_MPLANE,
                memory: MEMORY_MMAP,
                ..Default::default()
            };
            ioctl(&self.fd, VIDIOC_REQBUFS, &mut r).map_err(|e| err("REQBUFS capture 0", e))?;
            self.primed = false;
            self.setup_capture()
        }

        /// Copies a decoded picture into a pooled NV12 `AVFrame`.
        fn export(
            &mut self,
            index: u32,
            frame: *mut ff::AVFrame,
            pts: i64,
            duration: i64,
        ) -> Result<()> {
            let Some(cap) = &self.cap else {
                return Err(Error::Unsupported("V4L2: no capture buffers".into()));
            };
            let l = cap.layout;
            let src = &cap.bufs[index as usize];
            let size = l.copy_size();
            if l.end() > src.len {
                return Err(Error::Unsupported("V4L2: picture exceeds its buffer".into()));
            }
            // SAFETY: pool buffers hold `size` bytes; the source mapping holds
            // l.end() bytes; frame is a valid empty AVFrame we fill.
            unsafe {
                if self.pool.is_null() || self.pool_size != size {
                    ff::av_buffer_pool_uninit(&mut self.pool);
                    self.pool = ff::av_buffer_pool_init(size + 64, None);
                    self.pool_size = size;
                }
                let buf = ff::av_buffer_pool_get(self.pool);
                if buf.is_null() {
                    return Err(Error::Unsupported("V4L2: out of memory for a frame".into()));
                }
                let dst = (*buf).data;
                let y_len = l.stride * l.height as usize;
                let uv_len = l.stride * l.height.div_ceil(2) as usize;
                std::ptr::copy_nonoverlapping(src.ptr, dst, y_len);
                std::ptr::copy_nonoverlapping(src.ptr.add(l.uv_offset), dst.add(y_len), uv_len);
                ff::av_frame_unref(frame);
                let f = &mut *frame;
                f.buf[0] = buf;
                f.data[0] = dst;
                f.data[1] = dst.add(y_len);
                f.linesize[0] = l.stride as i32;
                f.linesize[1] = l.stride as i32;
                f.format = ff::AV_PIX_FMT_NV12;
                f.width = l.width as i32;
                f.height = l.height as i32;
                f.pts = pts;
                f.best_effort_timestamp = pts;
                f.duration = duration;
                (
                    f.color_range,
                    f.colorspace,
                    f.color_trc,
                    f.color_primaries,
                ) = self.color;
            }
            Ok(())
        }

        /// Receives a frame. `Ok(None)`: send more input. `Ok(Some(false))`:
        /// fully drained after end of stream.
        pub(crate) fn receive(&mut self, frame: *mut ff::AVFrame) -> Result<Option<bool>> {
            let mut idle_since: Option<Instant> = None;
            loop {
                if self.finished {
                    return Ok(Some(false));
                }
                self.pump_output()?;
                self.dequeue_events()?;
                if let Some(d) = self.dequeue_capture()? {
                    idle_since = None;
                    if d.flags & BUF_FLAG_LAST != 0 {
                        if let Some(c) = self.cap.as_mut() {
                            c.held = Some(d.index);
                        }
                        // Pick up a source change signalled with this buffer.
                        self.dequeue_events()?;
                        if self.stop_sent && !self.drc_pending {
                            self.finished = true;
                            return Ok(Some(false));
                        }
                        self.change_resolution()?;
                        continue;
                    }
                    let timing = self.ts.get(d.id);
                    let keep = d.bytesused > 0
                        && d.flags & BUF_FLAG_ERROR == 0
                        && timing.is_some_and(|(pts, _)| {
                            self.pts_floor
                                .is_none_or(|floor| pts == ff::AV_NOPTS_VALUE || pts >= floor)
                        });
                    let result = match (keep, timing) {
                        (true, Some((pts, dur))) => self.export(d.index, frame, pts, dur),
                        _ => {
                            self.queue_capture(d.index)?;
                            continue;
                        }
                    };
                    self.queue_capture(d.index)?;
                    result?;
                    self.frames += 1;
                    return Ok(Some(true));
                }
                // Nothing decoded yet. Return for more input unless the
                // bitstream queue is full or we are draining.
                let waiting = !self.pending.is_empty() || self.eos_input || self.drc_pending;
                if !waiting {
                    return Ok(None);
                }
                let since = *idle_since.get_or_insert_with(Instant::now);
                if since.elapsed() > STALL {
                    if self.eos_input {
                        log::warn!("V4L2: no end-of-stream marker from the decoder");
                        self.finished = true;
                        return Ok(Some(false));
                    }
                    return Err(Error::Unsupported("V4L2 decoder stalled".into()));
                }
                self.wait(50);
            }
        }

        fn wait(&self, ms: i32) {
            let mut p = libc::pollfd {
                fd: self.fd.as_raw_fd(),
                events: libc::POLLIN | libc::POLLOUT | libc::POLLPRI,
                revents: 0,
            };
            // SAFETY: one valid pollfd.
            let r = unsafe { libc::poll(&mut p, 1, ms) };
            if r > 0 && p.revents & libc::POLLERR != 0 {
                // Nothing queued on either side yet: avoid a hot loop.
                std::thread::sleep(Duration::from_millis(2));
            }
        }

        /// Discards everything before a seek. Queues keep streaming (a
        /// restart would re-run the driver's admission check): bitstream
        /// already queued still decodes, and its frames are dropped.
        pub(crate) fn flush(&mut self) {
            self.pending.clear();
            // SAFETY: our filter.
            unsafe { ff::av_bsf_flush(self.bsf) };
            self.ts.invalidate();
            self.pts_floor = None;
            self.arm_floor = true;
            if self.stop_sent && !self.finished {
                // Let the drain finish so decoding can be restarted.
                let start = Instant::now();
                while !self.finished && start.elapsed() < Duration::from_secs(2) {
                    match self.dequeue_capture() {
                        Ok(Some(d)) if d.flags & BUF_FLAG_LAST != 0 => {
                            if let Some(c) = self.cap.as_mut() {
                                c.held = Some(d.index);
                            }
                            self.finished = true;
                        }
                        Ok(Some(d)) => {
                            let _ = self.queue_capture(d.index);
                        }
                        Ok(None) => self.wait(20),
                        Err(_) => break,
                    }
                }
            }
            if self.stop_sent
                && let Err(e) = self.resume()
            {
                log::warn!("V4L2 restart after end of stream: {e}");
            }
            self.eos_input = false;
            self.stop_sent = false;
            self.finished = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The iris driver's check: every open instance counts with the new
    /// session's macroblocks.
    fn iris_admits(instances: u64, width: u32) -> impl Fn(u32) -> bool {
        move |h| instances * macroblocks(width, h) <= 261_120
    }

    #[test]
    fn caps_height_like_the_frame_needs() {
        // 8192x4096 with Steam's vrlink open: 2 x 131072 > 261120.
        assert!(!iris_admits(2, 8192)(4096));
        let mut calls = 0;
        let fits = iris_admits(2, 8192);
        let h = capped_height(4096, 512, |c| {
            calls += 1;
            fits(c)
        });
        // 4080 would fit the macroblock budget but is not 32-aligned (the
        // driver rounds it up to 4096); 4064 is the answer measured on the
        // headset.
        assert_eq!(h, Some(4064));
        assert!(calls <= 8, "{calls} probes");
        // Three instances.
        assert_eq!(capped_height(4096, 512, iris_admits(3, 8192)), Some(2720));
        // Nothing small enough.
        assert_eq!(capped_height(4096, 512, |_| false), None);
        // Everything fits: the largest aligned height strictly below.
        assert_eq!(capped_height(4096, 512, |_| true), Some(4064));
        assert_eq!(capped_height(4100, 512, |_| true), Some(4096));
        // Floor above the stream: nothing to try.
        assert_eq!(capped_height(300, 512, |_| true), None);
        assert_eq!(capped_height(1080, 1080, |_| true), None);
    }

    #[test]
    fn capped_height_results_are_aligned_and_admitted() {
        for inst in 2..6 {
            for &(w, h) in &[(8192u32, 4096u32), (7680, 3840), (5760, 2880), (8192, 8192)] {
                let fits = iris_admits(inst, w);
                if let Some(c) = capped_height(h, 512, &fits) {
                    assert_eq!(c % HEIGHT_STEP, 0);
                    assert!(c < h && fits(c));
                    assert!(c + HEIGHT_STEP >= h || !fits(c + HEIGHT_STEP));
                }
            }
        }
    }

    #[test]
    fn reuse_decision() {
        let nv12_8k = 8192 * 4096 * 3 / 2;
        let k8 = (8192, 4096);
        // Measured: primer 8192x4080 and stream 8192x4096 both report an
        // 8192x4096 CAPTURE format; 8 buffers of 50331648 bytes, minimum 7.
        assert!(can_reuse_capture(k8, 8, nv12_8k, k8, 7, nv12_8k));
        // A different coded size never reuses, even when the buffers are big
        // enough: the firmware's internal buffers would not match (it faults).
        assert!(!can_reuse_capture(k8, 8, nv12_8k, (8192, 2304), 7, 8192 * 2304 * 3 / 2));
        assert!(!can_reuse_capture((256, 128), 8, nv12_8k, k8, 7, nv12_8k));
        // Too small or too few: reallocate.
        assert!(!can_reuse_capture(k8, 8, 256 * 128 * 3 / 2, k8, 7, nv12_8k));
        assert!(!can_reuse_capture(k8, 6, nv12_8k, k8, 7, nv12_8k));
        assert!(!can_reuse_capture(k8, 0, nv12_8k, k8, 1, nv12_8k));
        assert!(!can_reuse_capture(k8, 8, nv12_8k, k8, 7, 0));
    }

    #[test]
    fn primer_only_for_streams_with_its_buffers() {
        let primer = Codec::Hevc.primer().unwrap().1;
        assert_eq!(primer, (8192, 4080));
        assert!(primer_matches((8192, 4096), primer));
        assert!(primer_matches((8192, 4088), primer));
        // Smaller than the primer: the primer would not help.
        assert!(!primer_matches((8192, 4080), primer));
        assert!(!primer_matches((8192, 4064), primer));
        // Other sizes need other buffers.
        assert!(!primer_matches((8192, 4320), primer));
        assert!(!primer_matches((7680, 4096), primer));
        assert!(!primer_matches((8192, 8192), primer));
        assert!(Codec::H264.primer().is_none());
    }

    #[test]
    fn nv12_layout_from_driver_format() {
        // What iris reports for an 8192x4080 picture.
        let l = Nv12Layout::new(8192, 4096, (8192, 4080), 50_331_648).unwrap();
        assert_eq!(l.uv_offset, 8192 * 4096);
        assert_eq!(l.end(), 8192 * 4096 + 8192 * 2040);
        assert_eq!(l.copy_size(), 8192 * (4080 + 2040));
        // A buffer too small for the picture is rejected.
        assert!(Nv12Layout::new(8192, 4096, (8192, 4096), 1 << 20).is_none());
        // Stride narrower than the picture is rejected.
        assert!(Nv12Layout::new(1024, 1088, (1920, 1080), 1 << 24).is_none());
        // Odd heights round the chroma up.
        let l = Nv12Layout::new(128, 64, (100, 51), 128 * 96).unwrap();
        assert_eq!(l.end(), 128 * 64 + 128 * 26);
    }

    #[test]
    fn timestamps_round_trip_and_invalidate() {
        let mut t = Timestamps::default();
        let a = t.insert(100, 1);
        let b = t.insert(200, 1);
        assert!(a >= 1 && b > a);
        let (s, u) = Timestamps::to_timeval(b);
        assert_eq!(Timestamps::from_timeval(s, u), b);
        assert_eq!(t.get(a), Some((100, 1)));
        assert_eq!(t.get(0), None, "primer");
        t.invalidate();
        assert_eq!(t.get(b), None, "stale after a seek");
        let c = t.insert(300, 1);
        assert!(c > b);
        assert_eq!(t.get(c), Some((300, 1)));
        let big = 1_234_567_890;
        let (s, u) = Timestamps::to_timeval(big);
        assert_eq!((s, u), (1234, 567_890));
        assert_eq!(Timestamps::from_timeval(s, u), big);
        for i in 0..1000 {
            t.insert(i, 1);
        }
        assert!(t.map.len() <= TIMESTAMPS_KEPT);
    }

    #[test]
    fn primers_match_their_codecs() {
        let (h, _) = Codec::Hevc.primer().unwrap();
        assert!(h.starts_with(&[0, 0, 0, 1, 0x40, 0x01]), "HEVC VPS first");
        // VPS, SPS, PPS, one IDR slice; no SEI.
        let nal_types: Vec<u8> = h
            .windows(4)
            .filter(|w| w[..3] == [0, 0, 1])
            .map(|w| (w[3] >> 1) & 0x3f)
            .collect();
        assert_eq!(nal_types, [32, 33, 34, 20]);
        assert!(h.len() < 16 * 1024);
        assert_eq!(Codec::Hevc.fourcc(), fourcc(b"HEVC"));
        assert_eq!(macroblocks(8192, 4096), 131_072);
        assert_eq!(macroblocks(1920, 1080), 120 * 68);
    }
}
