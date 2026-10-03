//! Hardware video decode through V4L2: device nodes and permissions,
//! `VIDIOC_QUERYCAP`, formats on both queues (including the CAPTURE
//! formats offered per coded format, where Qualcomm UBWC `QC08C`/`QC10C`
//! would show up), frame sizes, `/dev/dma_heap`, and real decodes of the
//! embedded clips through fp-video's `V4l2Decoder` (output fourcc, DRM
//! modifier, plane layout, DMA-BUF export, timing). Answers P2, P3, P4.

use crate::clips::{self, Clip};
use crate::report::{CheckOutput, Status};
use crate::runner::CheckContext;
use crate::util::{self, Out};
use fp_video::decode::v4l2::{self, sys::*, V4l2DeviceInfo};
use fp_video::{
    DecodedFrame, DecoderOptions, DecoderRequest, DmaBufFrame, TrackKind, VideoDecoder,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Stateless (request API) coded formats: fp-video does not drive these.
const STATELESS: &[&[u8; 4]] = &[b"S264", b"S265", b"VP9F", b"AV1F", b"VP8F", b"MG2S"];

const CAP_NAMES: &[(u32, &str)] = &[
    (0x0000_0001, "VIDEO_CAPTURE"),
    (0x0000_0002, "VIDEO_OUTPUT"),
    (0x0000_1000, "VIDEO_CAPTURE_MPLANE"),
    (0x0000_2000, "VIDEO_OUTPUT_MPLANE"),
    (0x0000_4000, "VIDEO_M2M_MPLANE"),
    (0x0000_8000, "VIDEO_M2M"),
    (0x0080_0000, "META_CAPTURE"),
    (0x0100_0000, "READWRITE"),
    (0x0400_0000, "STREAMING"),
    (0x2000_0000, "IO_MC"),
];

/// Names of the set V4L2 capability bits.
pub fn cap_names(caps: u32) -> Vec<&'static str> {
    CAP_NAMES
        .iter()
        .filter(|(b, _)| caps & b != 0)
        .map(|(_, n)| *n)
        .collect()
}

/// `0x00060c00` → `6.12.0`.
pub fn kernel_version_str(v: u32) -> String {
    format!("{}.{}.{}", v >> 16, (v >> 8) & 0xff, v & 0xff)
}

fn fourcc_bytes(f: u32) -> [u8; 4] {
    f.to_le_bytes()
}

pub fn is_stateless_format(f: u32) -> bool {
    STATELESS.iter().any(|s| **s == fourcc_bytes(f))
}

fn open_rw(path: &Path) -> std::io::Result<OwnedFd> {
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
    // SAFETY: fresh descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `FOURCC[c]` for compressed formats, plus the driver's description for
/// vendor formats we do not know.
fn enum_formats(fd: RawFd, type_: u32) -> Vec<String> {
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
        // SAFETY: valid v4l2_fmtdesc.
        if unsafe { xioctl(fd, VIDIOC_ENUM_FMT, &mut d) }.is_err() {
            break;
        }
        let mut s = fourcc_str(d.pixelformat);
        if d.flags & FMT_FLAG_COMPRESSED != 0 {
            s.push_str("[c]");
        }
        let known = [
            PIX_FMT_H264,
            PIX_FMT_HEVC,
            PIX_FMT_VP8,
            PIX_FMT_VP9,
            PIX_FMT_AV1,
            PIX_FMT_NV12,
            PIX_FMT_NV12M,
            PIX_FMT_P010,
        ];
        if !known.contains(&d.pixelformat) {
            s.push_str(&format!(" ({})", cstr(&d.description)));
        }
        out.push(s);
    }
    out
}

fn frame_sizes(fd: RawFd, pixfmt: u32) -> Option<String> {
    let mut parts = Vec::new();
    for index in 0..16 {
        let mut f = FrmSizeEnum {
            index,
            pixel_format: pixfmt,
            ..Default::default()
        };
        // SAFETY: valid v4l2_frmsizeenum.
        if unsafe { xioctl(fd, VIDIOC_ENUM_FRAMESIZES, &mut f) }.is_err() {
            break;
        }
        if f.type_ == FRMSIZE_TYPE_DISCRETE {
            parts.push(format!("{}x{}", f.u[0], f.u[1]));
        } else {
            // stepwise/continuous: min_w max_w step_w min_h max_h step_h
            parts.push(format!(
                "{}x{}..{}x{} step {}x{}",
                f.u[0], f.u[3], f.u[1], f.u[4], f.u[2], f.u[5]
            ));
            break;
        }
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// CAPTURE formats offered after `S_FMT(OUTPUT)` with `coded`.
fn capture_formats_for(fd: RawFd, coded: u32) -> Result<Vec<String>, String> {
    let mut f = Format::new(BUF_TYPE_VIDEO_OUTPUT_MPLANE);
    let mut pm = f.pix_mp();
    pm.width = 1920;
    pm.height = 1080;
    pm.pixelformat = coded;
    pm.field = FIELD_NONE;
    pm.num_planes = 1;
    pm.plane_fmt[0].sizeimage = 2 << 20;
    f.set_pix_mp(pm);
    // SAFETY: valid v4l2_format.
    unsafe { xioctl(fd, VIDIOC_S_FMT, &mut f) }.map_err(|e| format!("S_FMT: {e}"))?;
    Ok(enum_formats(fd, BUF_TYPE_VIDEO_CAPTURE_MPLANE))
}

/// Everything QUERYCAP / ENUM_* report for one node.
fn describe_node(path: &Path) -> Value {
    let fd = match open_rw(path) {
        Ok(fd) => fd,
        Err(e) => return json!({ "open_error": e.to_string() }),
    };
    let raw = fd.as_raw_fd();
    // SAFETY: zeroed POD.
    let mut cap: Capability = unsafe { std::mem::zeroed() };
    // SAFETY: valid v4l2_capability.
    if let Err(e) = unsafe { xioctl(raw, VIDIOC_QUERYCAP, &mut cap) } {
        return json!({ "querycap_error": e.to_string() });
    }
    let caps = if cap.capabilities & CAP_DEVICE_CAPS != 0 {
        cap.device_caps
    } else {
        cap.capabilities
    };
    let mut v = json!({
        "driver": cstr(&cap.driver),
        "card": cstr(&cap.card),
        "bus_info": cstr(&cap.bus_info),
        "driver_version": kernel_version_str(cap.version),
        "caps": cap_names(caps),
    });
    let mplane = caps & (0x4000 | 0x1000 | 0x2000) != 0;
    if mplane {
        let out = enum_formats(raw, BUF_TYPE_VIDEO_OUTPUT_MPLANE);
        let capf = enum_formats(raw, BUF_TYPE_VIDEO_CAPTURE_MPLANE);
        v["output_mplane_formats"] = json!(out);
        v["capture_mplane_formats"] = json!(capf);
    } else if caps & 0x8000 != 0 || caps & 0x3 != 0 {
        v["output_formats"] = json!(enum_formats(raw, 2));
        v["capture_formats"] = json!(enum_formats(raw, 1));
    }
    if caps & 0x4000 != 0 {
        // Per coded format: frame sizes and CAPTURE formats.
        let mut per = serde_json::Map::new();
        for idx in 0..32 {
            let mut d = FmtDesc {
                index: idx,
                type_: BUF_TYPE_VIDEO_OUTPUT_MPLANE,
                flags: 0,
                description: [0; 32],
                pixelformat: 0,
                mbus_code: 0,
                reserved: [0; 3],
            };
            // SAFETY: valid struct.
            if unsafe { xioctl(raw, VIDIOC_ENUM_FMT, &mut d) }.is_err() {
                break;
            }
            if d.flags & FMT_FLAG_COMPRESSED == 0 {
                continue;
            }
            let coded = d.pixelformat;
            // A fresh fd per coded format keeps S_FMT state independent.
            let caps_for = match open_rw(path) {
                Ok(f2) => capture_formats_for(f2.as_raw_fd(), coded),
                Err(e) => Err(e.to_string()),
            };
            per.insert(
                fourcc_str(coded),
                json!({
                    "sizes": frame_sizes(raw, coded),
                    "capture": caps_for.unwrap_or_else(|e| vec![e]),
                }),
            );
        }
        v["coded"] = Value::Object(per);
    }
    v
}

fn compact_node(path: &Path) -> String {
    match util::node_info(path) {
        Some(n) => format!(
            "{} {} {}:{} rw={}",
            n.path,
            n.mode,
            n.owner,
            n.group,
            if n.rw { "yes" } else { "no" }
        ),
        None => format!("{} (stat failed)", path.display()),
    }
}

/// Outcome of decoding one clip.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DecodeStats {
    pub clip: String,
    pub device: Option<String>,
    pub driver: Option<String>,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub packets_in: u32,
    pub frames_out: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_frame_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ms_per_frame: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<FrameDesc>,
    /// Earlier devices that failed for this clip.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub other_devices: Vec<String>,
}

/// Layout of a decoded DMA-BUF frame.
#[derive(Debug, Clone, Default, Serialize)]
pub struct FrameDesc {
    pub kind: &'static str,
    pub fourcc: String,
    pub modifier: String,
    pub size: String,
    pub coded_size: String,
    /// `offset/pitch` per plane.
    pub planes: Vec<String>,
    pub distinct_fds: usize,
    pub dmabuf_bytes: Option<u64>,
    /// Mean of sampled luma values (0..255) if the buffer is linear and
    /// mappable — proves real pixels were written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub luma_mean: Option<f64>,
}

pub fn modifier_str(m: u64) -> String {
    match m {
        0 => "0x0 (LINEAR)".into(),
        fp_video::decode::drm::MOD_QCOM_COMPRESSED => format!("{m:#x} (QCOM_COMPRESSED/UBWC)"),
        _ => format!("{m:#x}"),
    }
}

pub fn describe_frame(f: &DecodedFrame) -> FrameDesc {
    match f {
        DecodedFrame::DmaBuf(d) => {
            let mut fds: Vec<RawFd> = d.planes.iter().map(|p| p.fd).collect();
            fds.dedup();
            let size = d.planes.first().and_then(|p| dmabuf_size(p.fd));
            FrameDesc {
                kind: "dmabuf",
                fourcc: fourcc_str(d.fourcc),
                modifier: modifier_str(d.modifier),
                size: format!("{}x{}", d.width, d.height),
                coded_size: format!("{}x{}", d.coded_width, d.coded_height),
                planes: d
                    .planes
                    .iter()
                    .map(|p| format!("{}/{}", p.offset, p.pitch))
                    .collect(),
                distinct_fds: fds.len(),
                dmabuf_bytes: size,
                luma_mean: luma_mean(d),
            }
        }
        DecodedFrame::Cpu(c) => FrameDesc {
            kind: "cpu",
            fourcc: format!("{:?}", c.format),
            modifier: "-".into(),
            size: format!("{}x{}", c.width, c.height),
            coded_size: String::new(),
            planes: c.strides.iter().map(|s| format!("0/{s}")).collect(),
            distinct_fds: 0,
            dmabuf_bytes: None,
            luma_mean: None,
        },
    }
}

fn dmabuf_size(fd: RawFd) -> Option<u64> {
    // SAFETY: lseek on a borrowed fd; restored to 0 afterwards.
    unsafe {
        let end = libc::lseek(fd, 0, libc::SEEK_END);
        libc::lseek(fd, 0, libc::SEEK_SET);
        (end > 0).then_some(end as u64)
    }
}

const DMA_BUF_IOCTL_SYNC: libc::c_ulong = 0x4008_6200;
const DMA_BUF_SYNC_READ: u64 = 1;
const DMA_BUF_SYNC_END: u64 = 4;

/// Sample the luma plane of a linear NV12/P010 frame through mmap.
fn luma_mean(d: &DmaBufFrame) -> Option<f64> {
    if d.modifier != 0 || d.planes.is_empty() {
        return None;
    }
    let p = d.planes[0];
    let len = dmabuf_size(p.fd)? as usize;
    let ten = d.fourcc == fp_video::decode::drm::FORMAT_P010;
    let bpp = if ten { 2 } else { 1 };
    // SAFETY: read-only shared mapping of a DMA-BUF we hold; all reads are
    // bounds-checked against `len`.
    unsafe {
        let ptr = libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            p.fd,
            0,
        );
        if ptr == libc::MAP_FAILED {
            return None;
        }
        let mut sync = DMA_BUF_SYNC_READ;
        libc::ioctl(p.fd, DMA_BUF_IOCTL_SYNC as _, &mut sync);
        let base = ptr as *const u8;
        let (mut sum, mut n) = (0u64, 0u64);
        for y in (0..d.height as usize).step_by(7) {
            for x in (0..d.width as usize).step_by(5) {
                let off = p.offset as usize + y * p.pitch as usize + x * bpp;
                if off + bpp > len {
                    continue;
                }
                let v = if ten {
                    (u16::from_le_bytes([*base.add(off), *base.add(off + 1)]) >> 8) as u64
                } else {
                    *base.add(off) as u64
                };
                sum += v;
                n += 1;
            }
        }
        let mut sync = DMA_BUF_SYNC_READ | DMA_BUF_SYNC_END;
        libc::ioctl(p.fd, DMA_BUF_IOCTL_SYNC as _, &mut sync);
        libc::munmap(ptr, len);
        (n > 0).then(|| ((sum as f64 / n as f64) * 10.0).round() / 10.0)
    }
}

/// A decoder that produced at least one frame, kept alive with that frame.
pub struct Decoded {
    pub decoder: Box<dyn VideoDecoder>,
    pub first: DecodedFrame,
    pub stats: DecodeStats,
}

/// Decode `clip` on the first V4L2 device that accepts it. With
/// `keep_first` the decoder and first frame are returned (for import tests).
pub fn decode_clip(
    clip: &Clip,
    opts: &DecoderOptions,
    devices: &[V4l2DeviceInfo],
    keep_first: bool,
    budget: Duration,
) -> (DecodeStats, Option<Decoded>) {
    let mut stats = DecodeStats {
        clip: clip.name.into(),
        ..Default::default()
    };
    let mut demux = match clips::open(clip) {
        Ok(d) => d,
        Err(e) => {
            stats.error = Some(format!("demux: {e}"));
            return (stats, None);
        }
    };
    let Some(track) = demux.default_track(TrackKind::Video).cloned() else {
        stats.error = Some("clip has no video track".into());
        return (stats, None);
    };
    let req = DecoderRequest::from_track(&track);
    let coded = v4l2::coded_format(&req.codec).unwrap_or(0);
    let mut packets = Vec::new();
    while let Ok(Some(p)) = demux.read_packet() {
        if p.track == track.id {
            packets.push(p);
        }
    }
    let candidates: Vec<&V4l2DeviceInfo> = devices
        .iter()
        .filter(|d| d.coded_formats.contains(&coded))
        .collect();
    if candidates.is_empty() {
        stats.error = Some(format!(
            "no V4L2 device lists {} on its OUTPUT queue",
            fourcc_str(coded)
        ));
        return (stats, None);
    }
    for dev in candidates {
        let mut dec = match v4l2::V4l2Decoder::open(dev.clone(), &req, opts) {
            Ok(d) => d,
            Err(e) => {
                stats
                    .other_devices
                    .push(format!("{}: open: {e}", dev.path.display()));
                continue;
            }
        };
        stats.device = Some(dev.path.display().to_string());
        stats.driver = Some(dev.driver.clone());
        let start = Instant::now();
        let deadline = start + budget;
        let mut sent = 0usize;
        let mut first: Option<DecodedFrame> = None;
        let mut err: Option<String> = None;
        let mut drained_called = false;
        let mut last_frame_at = start;
        loop {
            if Instant::now() > deadline {
                err = Some(format!(
                    "timed out after {:.1} s ({} of {} packets sent)",
                    budget.as_secs_f64(),
                    sent,
                    packets.len()
                ));
                break;
            }
            let mut progressed = false;
            while sent < packets.len() {
                match dec.send_packet(&packets[sent]) {
                    Ok(true) => {
                        sent += 1;
                        progressed = true;
                    }
                    Ok(false) => break,
                    Err(e) => {
                        err = Some(format!("send packet {sent}: {e}"));
                        break;
                    }
                }
            }
            if err.is_some() {
                break;
            }
            loop {
                match dec.receive_frame() {
                    Ok(Some(f)) => {
                        progressed = true;
                        stats.frames_out += 1;
                        last_frame_at = Instant::now();
                        if first.is_none() {
                            stats.first_frame_ms =
                                Some(round1(start.elapsed().as_secs_f64() * 1000.0));
                            stats.output = Some(describe_frame(&f));
                            if keep_first {
                                first = Some(f);
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        err = Some(format!("receive frame: {e}"));
                        break;
                    }
                }
            }
            if err.is_some() {
                break;
            }
            if sent == packets.len() && !drained_called {
                if let Err(e) = dec.drain() {
                    err = Some(format!("drain: {e}"));
                    break;
                }
                drained_called = true;
            }
            if drained_called && dec.is_drained() {
                break;
            }
            if !progressed {
                dec.wait(Duration::from_millis(10));
            }
        }
        stats.packets_in = sent as u32;
        if stats.frames_out > 0 {
            let span = last_frame_at.duration_since(start).as_secs_f64() * 1000.0;
            stats.ms_per_frame = Some(round1(span / stats.frames_out as f64));
        }
        stats.ok = err.is_none() && stats.frames_out >= clip.frames;
        if err.is_none() && !stats.ok {
            err = Some(format!(
                "only {} of {} frames came out",
                stats.frames_out, clip.frames
            ));
        }
        stats.error = err;
        let keep = keep_first && first.is_some();
        if stats.frames_out > 0 || stats.error.is_none() {
            let decoded = if keep {
                Some(Decoded {
                    decoder: Box::new(dec),
                    first: first.expect("checked"),
                    stats: stats.clone(),
                })
            } else {
                None
            };
            return (stats, decoded);
        }
        stats.other_devices.push(format!(
            "{}: {}",
            dev.path.display(),
            stats.error.clone().unwrap_or_default()
        ));
    }
    (stats, None)
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

pub fn run(ctx: &CheckContext) -> CheckOutput {
    let mut o = Out::new();
    let video: Vec<PathBuf> = util::list_prefixed("/dev", "video");
    let media = util::list_prefixed("/dev", "media");
    let dri = util::list_prefixed("/dev/dri", "");
    let heaps = util::list_prefixed("/dev/dma_heap", "");
    o.set(
        "nodes",
        json!({
            "video": video.iter().map(|p| compact_node(p)).collect::<Vec<_>>(),
            "media": media.iter().map(|p| compact_node(p)).collect::<Vec<_>>(),
            "dri": dri.iter().map(|p| compact_node(p)).collect::<Vec<_>>(),
            "dma_heap": heaps.iter().map(|p| compact_node(p)).collect::<Vec<_>>(),
        }),
    );
    let mut described = serde_json::Map::new();
    for p in &video {
        described.insert(p.display().to_string(), describe_node(p));
    }
    o.set("v4l2_devices", Value::Object(described.clone()));
    ctx.partial(&o.snapshot(Status::Unknown, "listed V4L2 nodes"));

    // P3: are the nodes usable by this (non-root) user?
    let denied: Vec<&String> = described
        .iter()
        .filter(|(_, v)| {
            v["open_error"]
                .as_str()
                .is_some_and(|e| e.contains("ermission"))
        })
        .map(|(k, _)| k)
        .collect();
    let devices = v4l2::enumerate_devices();
    let stateful: Vec<String> = devices
        .iter()
        .map(|d| {
            format!(
                "{} ({}: {})",
                d.path.display(),
                d.driver,
                d.coded_formats
                    .iter()
                    .map(|&f| fourcc_str(f))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        })
        .collect();
    let stateless: Vec<String> = described
        .iter()
        .filter(|(_, v)| {
            v["output_mplane_formats"].as_array().is_some_and(|a| {
                a.iter().any(|f| {
                    f.as_str()
                        .is_some_and(|s| STATELESS.iter().any(|x| s.as_bytes().starts_with(*x)))
                })
            })
        })
        .map(|(k, v)| format!("{k} ({})", v["driver"].as_str().unwrap_or("?")))
        .collect();
    // SAFETY: getuid never fails.
    let root = unsafe { libc::getuid() } == 0;
    if video.is_empty() {
        o.finding(
            "v4l2_nodes",
            Status::Fail,
            &["P2", "P3"],
            "no /dev/video* nodes at all",
        );
    } else if !denied.is_empty() && devices.is_empty() {
        o.finding(
            "v4l2_nodes",
            Status::Fail,
            &["P3"],
            format!(
                "{} /dev/video* node(s) exist but opening them is denied for this user (see nodes for owner/group)",
                denied.len()
            ),
        );
    } else {
        o.finding(
            "v4l2_nodes",
            if root { Status::Unknown } else { Status::Pass },
            &["P3"],
            format!(
                "{} /dev/video* node(s), {} openable read-write{}",
                video.len(),
                video.len() - denied.len(),
                if root {
                    " (running as root, so permissions are not proven)"
                } else {
                    ""
                }
            ),
        );
    }
    if devices.is_empty() {
        o.finding(
            "v4l2_decoder",
            Status::Fail,
            &["P2"],
            if stateless.is_empty() {
                "no V4L2 stateful (M2M) decoder found".to_string()
            } else {
                format!(
                    "only stateless V4L2 decoders ({}); fp-video implements the stateful interface",
                    stateless.join(", ")
                )
            },
        );
    } else {
        o.finding(
            "v4l2_decoder",
            Status::Pass,
            &["P2"],
            format!("stateful V4L2 decoder(s): {}", stateful.join("; ")),
        );
    }
    o.set("stateful_decoders", &stateful);
    if !stateless.is_empty() {
        o.set("stateless_decoders", &stateless);
    }

    // Real decodes.
    let opts = DecoderOptions::default();
    let mut results = Vec::new();
    let mut first_desc: Option<FrameDesc> = None;
    if !devices.is_empty() {
        for clip in clips::CLIPS {
            let budget = Duration::from_secs(if clip.width > 4000 { 25 } else { 12 });
            let (s, _) = decode_clip(clip, &opts, &devices, false, budget);
            let id = format!("decode_{}", clip.name);
            let refs: &[&str] = if clip.width > 4000 {
                &["P2", "P17"]
            } else {
                &["P2"]
            };
            let label = format!(
                "{} {}-bit {}x{}",
                clip.codec.name(),
                clip.bit_depth,
                clip.width,
                clip.height
            );
            if s.ok {
                o.finding(
                    &id,
                    Status::Pass,
                    refs,
                    format!(
                        "{label} decodes on {} ({}): {} frames, {} ms/frame, output {} {}",
                        s.driver.as_deref().unwrap_or("?"),
                        s.device.as_deref().unwrap_or("?"),
                        s.frames_out,
                        s.ms_per_frame.unwrap_or(0.0),
                        s.output.as_ref().map_or("?", |f| f.fourcc.as_str()),
                        s.output.as_ref().map_or("?".into(), |f| f.modifier.clone()),
                    ),
                );
            } else {
                o.finding(
                    &id,
                    Status::Fail,
                    refs,
                    format!("{label}: {}", s.error.as_deref().unwrap_or("failed")),
                );
            }
            if first_desc.is_none() {
                first_desc = s.output.clone();
            }
            results.push(s);
            o.set("decodes", &results);
            ctx.partial(&o.snapshot(Status::Unknown, format!("decoded {}", clip.name)));
        }
        // Same HEVC clip again allowing vendor-compressed output: reveals
        // whether the decoder can hand out UBWC (QC08C) buffers.
        if let Some(clip) = clips::by_name("hevc_256x256") {
            let opts_c = DecoderOptions {
                allow_compressed_formats: true,
                ..DecoderOptions::default()
            };
            let (s, _) = decode_clip(clip, &opts_c, &devices, false, Duration::from_secs(12));
            o.set("decode_hevc_allow_ubwc", &s);
        }
    }
    match &first_desc {
        Some(f) => {
            o.finding(
                "capture_format",
                Status::Pass,
                &["P4"],
                format!(
                    "decoder output: {} modifier {}, {} plane(s) {} in {} DMA-BUF fd(s)",
                    f.fourcc,
                    f.modifier,
                    f.planes.len(),
                    f.planes.join(" "),
                    f.distinct_fds
                ),
            );
            o.finding(
                "expbuf",
                if f.kind == "dmabuf" {
                    Status::Pass
                } else {
                    Status::Fail
                },
                &["P4"],
                format!(
                    "VIDIOC_EXPBUF {}",
                    if f.kind == "dmabuf" {
                        "works (frames arrive as DMA-BUF)"
                    } else {
                        "not used"
                    }
                ),
            );
        }
        None if !devices.is_empty() => o.finding(
            "capture_format",
            Status::Fail,
            &["P4"],
            "no frame was decoded, so the CAPTURE format/modifier is unknown",
        ),
        None => {}
    }
    o.set(
        "dma_heaps",
        heaps
            .iter()
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect::<Vec<_>>(),
    );

    let ok: Vec<&str> = results
        .iter()
        .filter(|r| r.ok)
        .map(|r| r.clip.as_str())
        .collect();
    let status = if devices.is_empty() || ok.is_empty() {
        Status::Fail
    } else {
        Status::Pass
    };
    let summary = if devices.is_empty() {
        o.findings
            .iter()
            .find(|f| f.id == "v4l2_decoder")
            .map(|f| f.summary.clone())
            .unwrap_or_default()
    } else {
        format!(
            "{} of {} test clips decode in hardware ({}){}",
            ok.len(),
            results.len(),
            if ok.is_empty() {
                "none".to_string()
            } else {
                ok.join(", ")
            },
            first_desc
                .as_ref()
                .map(|f| format!("; output {} {}", f.fourcc, f.modifier))
                .unwrap_or_default()
        )
    };
    o.finish(status, summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_and_versions() {
        assert_eq!(
            cap_names(0xa420_4000 & !0x8000_0000),
            ["VIDEO_M2M_MPLANE", "STREAMING", "IO_MC"]
        );
        assert_eq!(kernel_version_str(0x0006_0c03), "6.12.3");
        assert!(is_stateless_format(fourcc(b'S', b'2', b'6', b'5')));
        assert!(!is_stateless_format(PIX_FMT_HEVC));
        assert_eq!(modifier_str(0), "0x0 (LINEAR)");
        assert!(modifier_str(fp_video::decode::drm::MOD_QCOM_COMPRESSED).contains("UBWC"));
    }

    #[test]
    fn decode_without_devices_reports_reason() {
        let clip = clips::by_name("hevc_256x256").unwrap();
        let (s, d) = decode_clip(
            clip,
            &DecoderOptions::default(),
            &[],
            false,
            Duration::from_secs(1),
        );
        assert!(!s.ok && d.is_none());
        assert!(s.error.unwrap().contains("HEVC"));
    }
}
