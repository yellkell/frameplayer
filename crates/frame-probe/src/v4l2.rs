//! V4L2 memory-to-memory decoders and cameras, read directly with ioctls.
//! Answers "is there a hardware video decoder, which codecs and sizes, and can
//! an unprivileged app open it".

use crate::util::{IOC_READ, IOC_READWRITE, fixed_bytes, fourcc, ioc};
use serde::Serialize;

#[repr(C)]
#[derive(Default)]
pub struct V4l2Capability {
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
pub struct V4l2Fmtdesc {
    pub index: u32,
    pub type_: u32,
    pub flags: u32,
    pub description: [u8; 32],
    pub pixelformat: u32,
    pub mbus_code: u32,
    pub reserved: [u32; 3],
}

#[repr(C)]
#[derive(Default)]
pub struct V4l2Frmsizeenum {
    pub index: u32,
    pub pixel_format: u32,
    pub type_: u32,
    /// discrete: [w, h, ..]; stepwise: [min_w, max_w, step_w, min_h, max_h, step_h]
    pub u: [u32; 6],
    pub reserved: [u32; 2],
}

pub const VIDIOC_QUERYCAP: u64 = ioc(IOC_READ, b'V', 0, std::mem::size_of::<V4l2Capability>());
pub const VIDIOC_ENUM_FMT: u64 = ioc(IOC_READWRITE, b'V', 2, std::mem::size_of::<V4l2Fmtdesc>());
pub const VIDIOC_ENUM_FRAMESIZES: u64 = ioc(
    IOC_READWRITE,
    b'V',
    74,
    std::mem::size_of::<V4l2Frmsizeenum>(),
);

const CAP_VIDEO_CAPTURE: u32 = 0x1;
const CAP_VIDEO_OUTPUT: u32 = 0x2;
const CAP_VIDEO_CAPTURE_MPLANE: u32 = 0x1000;
const CAP_VIDEO_OUTPUT_MPLANE: u32 = 0x2000;
const CAP_VIDEO_M2M_MPLANE: u32 = 0x4000;
const CAP_VIDEO_M2M: u32 = 0x8000;
const CAP_STREAMING: u32 = 0x0400_0000;
const CAP_DEVICE_CAPS: u32 = 0x8000_0000;

const BUF_VIDEO_CAPTURE: u32 = 1;
const BUF_VIDEO_OUTPUT: u32 = 2;
const BUF_VIDEO_CAPTURE_MPLANE: u32 = 9;
const BUF_VIDEO_OUTPUT_MPLANE: u32 = 10;

const FMT_FLAG_COMPRESSED: u32 = 0x1;
const FRMSIZE_TYPE_DISCRETE: u32 = 1;

#[derive(Serialize)]
pub struct V4l2Report {
    pub devices: Vec<V4l2Device>,
    pub media_nodes: Vec<String>,
}

#[derive(Serialize, Default)]
pub struct V4l2Device {
    pub node: String,
    pub mode: String,
    pub owner_group: Option<String>,
    pub open_error: Option<String>,
    pub driver: String,
    pub card: String,
    pub bus_info: String,
    pub kind: String,
    pub caps: Vec<&'static str>,
    /// Bitstream formats the device accepts (decoder input).
    pub compressed_in: Vec<Format>,
    /// Raw formats on the other queue (decoder output / camera frames).
    pub raw_out: Vec<Format>,
}

#[derive(Serialize)]
pub struct Format {
    pub fourcc: String,
    pub description: String,
    pub compressed: bool,
    pub max_size: Option<(u32, u32)>,
}

pub fn probe() -> V4l2Report {
    let mut nodes: Vec<String> = list_dev("video");
    nodes.sort_by_key(|n| {
        n.trim_start_matches("/dev/video")
            .parse::<u32>()
            .unwrap_or(u32::MAX)
    });
    let mut media_nodes = list_dev("media");
    media_nodes.sort();
    V4l2Report {
        devices: nodes.iter().map(|n| probe_node(n)).collect(),
        media_nodes,
    }
}

fn list_dev(prefix: &str) -> Vec<String> {
    std::fs::read_dir("/dev")
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| {
                    n.starts_with(prefix) && n[prefix.len()..].chars().all(|c| c.is_ascii_digit())
                })
                .map(|n| format!("/dev/{n}"))
                .collect()
        })
        .unwrap_or_default()
}

fn probe_node(node: &str) -> V4l2Device {
    let mut d = V4l2Device {
        node: node.to_string(),
        ..Default::default()
    };
    if let Ok(md) = std::fs::metadata(node) {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        d.mode = format!("{:o}", md.permissions().mode() & 0o777);
        d.owner_group = group_name(md.gid());
    }
    let c = std::ffi::CString::new(node).expect("path");
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        d.open_error = Some(crate::util::last_os_error());
        return d;
    }
    let mut cap = V4l2Capability::default();
    if unsafe { libc::ioctl(fd, VIDIOC_QUERYCAP as _, &mut cap) } == 0 {
        d.driver = fixed_bytes(&cap.driver);
        d.card = fixed_bytes(&cap.card);
        d.bus_info = fixed_bytes(&cap.bus_info);
        let caps = if cap.capabilities & CAP_DEVICE_CAPS != 0 {
            cap.device_caps
        } else {
            cap.capabilities
        };
        d.caps = cap_names(caps);
        let m2m = caps & (CAP_VIDEO_M2M | CAP_VIDEO_M2M_MPLANE) != 0;
        let mplane =
            caps & (CAP_VIDEO_M2M_MPLANE | CAP_VIDEO_CAPTURE_MPLANE | CAP_VIDEO_OUTPUT_MPLANE) != 0;
        let (out_q, cap_q) = if mplane {
            (BUF_VIDEO_OUTPUT_MPLANE, BUF_VIDEO_CAPTURE_MPLANE)
        } else {
            (BUF_VIDEO_OUTPUT, BUF_VIDEO_CAPTURE)
        };
        let inputs = if m2m {
            enum_formats(fd, out_q)
        } else {
            Vec::new()
        };
        let outputs = enum_formats(fd, cap_q);
        d.kind = classify(m2m, &inputs, &outputs).to_string();
        d.compressed_in = inputs;
        d.raw_out = outputs;
    } else {
        d.open_error = Some(format!("VIDIOC_QUERYCAP: {}", crate::util::last_os_error()));
    }
    unsafe { libc::close(fd) };
    d
}

fn enum_formats(fd: i32, buf_type: u32) -> Vec<Format> {
    let mut out = Vec::new();
    for index in 0..64 {
        let mut f = V4l2Fmtdesc {
            index,
            type_: buf_type,
            ..Default::default()
        };
        if unsafe { libc::ioctl(fd, VIDIOC_ENUM_FMT as _, &mut f) } != 0 {
            break;
        }
        out.push(Format {
            fourcc: fourcc(f.pixelformat),
            description: fixed_bytes(&f.description),
            compressed: f.flags & FMT_FLAG_COMPRESSED != 0,
            max_size: max_frame_size(fd, f.pixelformat),
        });
    }
    out
}

fn max_frame_size(fd: i32, pixfmt: u32) -> Option<(u32, u32)> {
    let mut best: Option<(u32, u32)> = None;
    for index in 0..64 {
        let mut s = V4l2Frmsizeenum {
            index,
            pixel_format: pixfmt,
            ..Default::default()
        };
        if unsafe { libc::ioctl(fd, VIDIOC_ENUM_FRAMESIZES as _, &mut s) } != 0 {
            break;
        }
        let (w, h) = if s.type_ == FRMSIZE_TYPE_DISCRETE {
            (s.u[0], s.u[1])
        } else {
            (s.u[1], s.u[4])
        };
        if best.is_none_or(|(bw, bh)| (w as u64 * h as u64) > (bw as u64 * bh as u64)) {
            best = Some((w, h));
        }
        if s.type_ != FRMSIZE_TYPE_DISCRETE {
            break; // stepwise/continuous: a single entry describes the range
        }
    }
    best
}

pub fn classify(m2m: bool, inputs: &[Format], outputs: &[Format]) -> &'static str {
    let decodes = inputs.iter().any(|f| f.compressed);
    let encodes = outputs.iter().any(|f| f.compressed);
    match (m2m, decodes, encodes) {
        (true, true, _) => "decoder",
        (true, false, true) => "encoder",
        (true, _, _) => "m2m (converter/other)",
        (false, _, _) if !outputs.is_empty() => "capture (camera or grabber)",
        _ => "other",
    }
}

fn cap_names(c: u32) -> Vec<&'static str> {
    [
        (CAP_VIDEO_CAPTURE, "VIDEO_CAPTURE"),
        (CAP_VIDEO_OUTPUT, "VIDEO_OUTPUT"),
        (CAP_VIDEO_CAPTURE_MPLANE, "VIDEO_CAPTURE_MPLANE"),
        (CAP_VIDEO_OUTPUT_MPLANE, "VIDEO_OUTPUT_MPLANE"),
        (CAP_VIDEO_M2M_MPLANE, "VIDEO_M2M_MPLANE"),
        (CAP_VIDEO_M2M, "VIDEO_M2M"),
        (CAP_STREAMING, "STREAMING"),
    ]
    .iter()
    .filter(|(bit, _)| c & bit != 0)
    .map(|(_, n)| *n)
    .collect()
}

fn group_name(gid: u32) -> Option<String> {
    let table = std::fs::read_to_string("/etc/group").ok()?;
    table.lines().find_map(|l| {
        let mut f = l.split(':');
        let name = f.next()?;
        let id = f.nth(1)?.parse::<u32>().ok()?;
        (id == gid).then(|| name.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_sizes_match_kernel_abi() {
        assert_eq!(std::mem::size_of::<V4l2Capability>(), 104);
        assert_eq!(std::mem::size_of::<V4l2Fmtdesc>(), 64);
        assert_eq!(std::mem::size_of::<V4l2Frmsizeenum>(), 44);
    }

    #[test]
    fn ioctl_numbers_match_kernel_headers() {
        assert_eq!(VIDIOC_QUERYCAP, 0x8068_5600);
        assert_eq!(VIDIOC_ENUM_FMT, 0xc040_5602);
        assert_eq!(VIDIOC_ENUM_FRAMESIZES, 0xc02c_564a);
    }

    fn fmt(fourcc: &str, compressed: bool) -> Format {
        Format {
            fourcc: fourcc.into(),
            description: String::new(),
            compressed,
            max_size: None,
        }
    }

    #[test]
    fn classifies_devices() {
        assert_eq!(
            classify(true, &[fmt("HEVC", true)], &[fmt("NV12", false)]),
            "decoder"
        );
        assert_eq!(
            classify(true, &[fmt("NV12", false)], &[fmt("H264", true)]),
            "encoder"
        );
        assert_eq!(
            classify(false, &[], &[fmt("YUYV", false)]),
            "capture (camera or grabber)"
        );
    }
}
