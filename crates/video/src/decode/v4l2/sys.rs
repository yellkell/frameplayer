//! Raw V4L2 UAPI definitions (`linux/videodev2.h`) for 64-bit Linux
//! (aarch64 / x86_64 share these layouts). Only what the stateful decoder
//! needs. Struct sizes and ioctl numbers are unit-tested against the values
//! the kernel headers produce.

#![allow(dead_code)]

use std::os::fd::RawFd;

pub const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

// Pixel formats.
pub const PIX_FMT_H264: u32 = fourcc(b'H', b'2', b'6', b'4');
pub const PIX_FMT_HEVC: u32 = fourcc(b'H', b'E', b'V', b'C');
pub const PIX_FMT_VP8: u32 = fourcc(b'V', b'P', b'8', b'0');
pub const PIX_FMT_VP9: u32 = fourcc(b'V', b'P', b'9', b'0');
pub const PIX_FMT_AV1: u32 = fourcc(b'A', b'V', b'0', b'1');
pub const PIX_FMT_NV12: u32 = fourcc(b'N', b'V', b'1', b'2');
pub const PIX_FMT_NV12M: u32 = fourcc(b'N', b'M', b'1', b'2');
pub const PIX_FMT_P010: u32 = fourcc(b'P', b'0', b'1', b'0');
/// Qualcomm UBWC-compressed NV12 / P010 (venus/iris).
pub const PIX_FMT_QC08C: u32 = fourcc(b'Q', b'0', b'8', b'C');
pub const PIX_FMT_QC10C: u32 = fourcc(b'Q', b'1', b'0', b'C');

// Capabilities.
pub const CAP_VIDEO_M2M_MPLANE: u32 = 0x0000_4000;
pub const CAP_STREAMING: u32 = 0x0400_0000;
pub const CAP_DEVICE_CAPS: u32 = 0x8000_0000;

// Buffer types / memory.
pub const BUF_TYPE_VIDEO_CAPTURE_MPLANE: u32 = 9;
pub const BUF_TYPE_VIDEO_OUTPUT_MPLANE: u32 = 10;
pub const MEMORY_MMAP: u32 = 1;
pub const MEMORY_DMABUF: u32 = 4;
pub const FIELD_NONE: u32 = 1;

// Buffer flags.
pub const BUF_FLAG_MAPPED: u32 = 0x0000_0001;
pub const BUF_FLAG_QUEUED: u32 = 0x0000_0002;
pub const BUF_FLAG_DONE: u32 = 0x0000_0004;
pub const BUF_FLAG_KEYFRAME: u32 = 0x0000_0008;
pub const BUF_FLAG_ERROR: u32 = 0x0000_0040;
pub const BUF_FLAG_TIMESTAMP_COPY: u32 = 0x0000_4000;
pub const BUF_FLAG_LAST: u32 = 0x0010_0000;

pub const FMT_FLAG_COMPRESSED: u32 = 0x0001;

// Events.
pub const EVENT_EOS: u32 = 2;
pub const EVENT_SOURCE_CHANGE: u32 = 5;
pub const EVENT_SRC_CH_RESOLUTION: u32 = 1;

// Decoder commands.
pub const DEC_CMD_START: u32 = 0;
pub const DEC_CMD_STOP: u32 = 1;

pub const CID_MIN_BUFFERS_FOR_CAPTURE: u32 = 0x0098_0900 + 39;
pub const SEL_TGT_COMPOSE: u32 = 0x0100;

pub const FRMSIZE_TYPE_DISCRETE: u32 = 1;

pub const VIDEO_MAX_PLANES: usize = 8;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
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
#[derive(Debug, Clone, Copy)]
pub struct FmtDesc {
    pub index: u32,
    pub type_: u32,
    pub flags: u32,
    pub description: [u8; 32],
    pub pixelformat: u32,
    pub mbus_code: u32,
    pub reserved: [u32; 3],
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct PlanePixFormat {
    pub sizeimage: u32,
    pub bytesperline: u32,
    pub reserved: [u16; 6],
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct PixFormatMplane {
    pub width: u32,
    pub height: u32,
    pub pixelformat: u32,
    pub field: u32,
    pub colorspace: u32,
    pub plane_fmt: [PlanePixFormat; VIDEO_MAX_PLANES],
    pub num_planes: u8,
    pub flags: u8,
    pub ycbcr_enc: u8,
    pub quantization: u8,
    pub xfer_func: u8,
    pub reserved: [u8; 7],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union FormatUnion {
    pub pix_mp: PixFormatMplane,
    pub raw_data: [u8; 200],
    // `struct v4l2_window` contains pointers: 8-byte alignment.
    _align: [u64; 25],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Format {
    pub type_: u32,
    pub fmt: FormatUnion,
}

impl Format {
    pub fn new(type_: u32) -> Self {
        Format {
            type_,
            fmt: FormatUnion { raw_data: [0; 200] },
        }
    }
    pub fn pix_mp(&self) -> PixFormatMplane {
        // SAFETY: all-bytes-valid POD union.
        unsafe { self.fmt.pix_mp }
    }
    pub fn set_pix_mp(&mut self, p: PixFormatMplane) {
        self.fmt.pix_mp = p;
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestBuffers {
    pub count: u32,
    pub type_: u32,
    pub memory: u32,
    pub capabilities: u32,
    pub flags: u8,
    pub reserved: [u8; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union PlaneM {
    pub mem_offset: u32,
    pub userptr: libc::c_ulong,
    pub fd: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Plane {
    pub bytesused: u32,
    pub length: u32,
    pub m: PlaneM,
    pub data_offset: u32,
    pub reserved: [u32; 11],
}

impl Default for Plane {
    fn default() -> Self {
        Plane {
            bytesused: 0,
            length: 0,
            m: PlaneM { userptr: 0 },
            data_offset: 0,
            reserved: [0; 11],
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Timecode {
    pub type_: u32,
    pub flags: u32,
    pub frames: u8,
    pub seconds: u8,
    pub minutes: u8,
    pub hours: u8,
    pub userbits: [u8; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union BufferM {
    pub offset: u32,
    pub userptr: libc::c_ulong,
    pub planes: *mut Plane,
    pub fd: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Buffer {
    pub index: u32,
    pub type_: u32,
    pub bytesused: u32,
    pub flags: u32,
    pub field: u32,
    pub timestamp: libc::timeval,
    pub timecode: Timecode,
    pub sequence: u32,
    pub memory: u32,
    pub m: BufferM,
    pub length: u32,
    pub reserved2: u32,
    pub request_fd: i32,
}

impl Buffer {
    /// A multi-planar buffer descriptor pointing at `planes`.
    pub fn new_mplane(type_: u32, memory: u32, index: u32, planes: &mut [Plane]) -> Self {
        Buffer {
            index,
            type_,
            bytesused: 0,
            flags: 0,
            field: 0,
            timestamp: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            timecode: Timecode::default(),
            sequence: 0,
            memory,
            m: BufferM {
                planes: planes.as_mut_ptr(),
            },
            length: planes.len() as u32,
            reserved2: 0,
            request_fd: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ExportBuffer {
    pub type_: u32,
    pub index: u32,
    pub plane: u32,
    pub flags: u32,
    pub fd: RawFd,
    pub reserved: [u32; 11],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct EventSubscription {
    pub type_: u32,
    pub id: u32,
    pub flags: u32,
    pub reserved: [u32; 5],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union EventUnion {
    pub data: [u8; 64],
    _align: [u64; 8],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Event {
    pub type_: u32,
    pub u: EventUnion,
    pub pending: u32,
    pub sequence: u32,
    pub timestamp: libc::timespec,
    pub id: u32,
    pub reserved: [u32; 8],
}

impl Event {
    pub fn zeroed() -> Self {
        // SAFETY: POD.
        unsafe { std::mem::zeroed() }
    }
    /// `u.src_change.changes`.
    pub fn src_changes(&self) -> u32 {
        // SAFETY: POD union.
        let d = unsafe { self.u.data };
        u32::from_ne_bytes([d[0], d[1], d[2], d[3]])
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DecoderCmd {
    pub cmd: u32,
    pub flags: u32,
    pub raw: [u64; 8],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Selection {
    pub type_: u32,
    pub target: u32,
    pub flags: u32,
    pub r: Rect,
    pub reserved: [u32; 9],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Control {
    pub id: u32,
    pub value: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct FrmSizeEnum {
    pub index: u32,
    pub pixel_format: u32,
    pub type_: u32,
    /// discrete: [w, h, ..]; stepwise: [min_w, max_w, step_w, min_h, max_h, step_h].
    pub u: [u32; 6],
    pub reserved: [u32; 2],
}

// ioctl request encoding (asm-generic).
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;
const fn ioc(dir: u32, nr: u32, size: usize) -> libc::c_ulong {
    ((dir << 30) | ((size as u32) << 16) | ((b'V' as u32) << 8) | nr) as libc::c_ulong
}
const fn ior<T>(nr: u32) -> libc::c_ulong {
    ioc(IOC_READ, nr, std::mem::size_of::<T>())
}
const fn iow<T>(nr: u32) -> libc::c_ulong {
    ioc(IOC_WRITE, nr, std::mem::size_of::<T>())
}
const fn iowr<T>(nr: u32) -> libc::c_ulong {
    ioc(IOC_READ | IOC_WRITE, nr, std::mem::size_of::<T>())
}

pub const VIDIOC_QUERYCAP: libc::c_ulong = ior::<Capability>(0);
pub const VIDIOC_ENUM_FMT: libc::c_ulong = iowr::<FmtDesc>(2);
pub const VIDIOC_G_FMT: libc::c_ulong = iowr::<Format>(4);
pub const VIDIOC_S_FMT: libc::c_ulong = iowr::<Format>(5);
pub const VIDIOC_REQBUFS: libc::c_ulong = iowr::<RequestBuffers>(8);
pub const VIDIOC_QUERYBUF: libc::c_ulong = iowr::<Buffer>(9);
pub const VIDIOC_QBUF: libc::c_ulong = iowr::<Buffer>(15);
pub const VIDIOC_EXPBUF: libc::c_ulong = iowr::<ExportBuffer>(16);
pub const VIDIOC_DQBUF: libc::c_ulong = iowr::<Buffer>(17);
pub const VIDIOC_STREAMON: libc::c_ulong = iow::<libc::c_int>(18);
pub const VIDIOC_STREAMOFF: libc::c_ulong = iow::<libc::c_int>(19);
pub const VIDIOC_G_CTRL: libc::c_ulong = iowr::<Control>(27);
pub const VIDIOC_ENUM_FRAMESIZES: libc::c_ulong = iowr::<FrmSizeEnum>(74);
pub const VIDIOC_DQEVENT: libc::c_ulong = ior::<Event>(89);
pub const VIDIOC_SUBSCRIBE_EVENT: libc::c_ulong = iow::<EventSubscription>(90);
pub const VIDIOC_G_SELECTION: libc::c_ulong = iowr::<Selection>(94);
pub const VIDIOC_DECODER_CMD: libc::c_ulong = iowr::<DecoderCmd>(96);

/// `ioctl` that retries on `EINTR` and maps failures to `io::Error`.
///
/// # Safety
/// `arg` must point to a value of the type the request expects.
pub unsafe fn xioctl<T>(fd: RawFd, req: libc::c_ulong, arg: *mut T) -> std::io::Result<()> {
    loop {
        // The request parameter type differs between libc targets (c_ulong vs c_int).
        let r = libc::ioctl(fd, req as _, arg);
        if r == -1 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(e);
        }
        return Ok(());
    }
}

/// Fixed-size C string field → `String`.
pub fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

pub fn fourcc_str(f: u32) -> String {
    f.to_le_bytes()
        .iter()
        .map(|&c| if c.is_ascii_graphic() { c as char } else { '?' })
        .collect()
}

#[cfg(all(test, target_pointer_width = "64"))]
mod tests {
    use super::*;
    use std::mem::size_of;

    #[test]
    fn struct_sizes_match_kernel_abi() {
        assert_eq!(size_of::<Capability>(), 104);
        assert_eq!(size_of::<FmtDesc>(), 64);
        assert_eq!(size_of::<PlanePixFormat>(), 20);
        assert_eq!(size_of::<PixFormatMplane>(), 192);
        assert_eq!(size_of::<Format>(), 208);
        assert_eq!(size_of::<RequestBuffers>(), 20);
        assert_eq!(size_of::<Plane>(), 64);
        assert_eq!(size_of::<Buffer>(), 88);
        assert_eq!(size_of::<ExportBuffer>(), 64);
        assert_eq!(size_of::<EventSubscription>(), 32);
        assert_eq!(size_of::<Event>(), 136);
        assert_eq!(size_of::<DecoderCmd>(), 72);
        assert_eq!(size_of::<Selection>(), 64);
        assert_eq!(size_of::<FrmSizeEnum>(), 44);
    }

    #[test]
    fn ioctl_numbers_match_kernel_headers() {
        assert_eq!(VIDIOC_QUERYCAP, 0x8068_5600);
        assert_eq!(VIDIOC_ENUM_FMT, 0xc040_5602);
        assert_eq!(VIDIOC_G_FMT, 0xc0d0_5604);
        assert_eq!(VIDIOC_S_FMT, 0xc0d0_5605);
        assert_eq!(VIDIOC_REQBUFS, 0xc014_5608);
        assert_eq!(VIDIOC_QUERYBUF, 0xc058_5609);
        assert_eq!(VIDIOC_QBUF, 0xc058_560f);
        assert_eq!(VIDIOC_EXPBUF, 0xc040_5610);
        assert_eq!(VIDIOC_DQBUF, 0xc058_5611);
        assert_eq!(VIDIOC_STREAMON, 0x4004_5612);
        assert_eq!(VIDIOC_STREAMOFF, 0x4004_5613);
        assert_eq!(VIDIOC_G_CTRL, 0xc008_561b);
        assert_eq!(VIDIOC_ENUM_FRAMESIZES, 0xc02c_564a);
        assert_eq!(VIDIOC_DQEVENT, 0x8088_5659);
        assert_eq!(VIDIOC_SUBSCRIBE_EVENT, 0x4020_565a);
        assert_eq!(VIDIOC_G_SELECTION, 0xc040_565e);
        assert_eq!(VIDIOC_DECODER_CMD, 0xc048_5660);
    }

    #[test]
    fn field_offsets() {
        let b = Buffer::new_mplane(0, 0, 0, &mut []);
        let base = &b as *const _ as usize;
        assert_eq!(&b.timestamp as *const _ as usize - base, 24);
        assert_eq!(&b.sequence as *const _ as usize - base, 56);
        assert_eq!(&b.m as *const _ as usize - base, 64);
        assert_eq!(&b.length as *const _ as usize - base, 72);
        let f = Format::new(0);
        assert_eq!(&f.fmt as *const _ as usize - &f as *const _ as usize, 8);
        assert_eq!(fourcc_str(PIX_FMT_HEVC), "HEVC");
    }
}
