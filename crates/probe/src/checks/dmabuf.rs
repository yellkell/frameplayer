//! The zero-copy question (P4): can a frame the V4L2 decoder exports as
//! DMA-BUF be imported into Vulkan on this GPU?
//!
//! 1. Decode the first frame of an embedded clip through fp-video's V4L2
//!    decoder and keep it (and the decoder) alive.
//! 2. Create a headless Vulkan 1.3 device with the zero-copy extensions
//!    (selected by fp-xr's `select_device_extensions`, as in the app).
//! 3. Import each plane step by step (image format query, fd memory
//!    properties, create image, allocate/import, bind), recording the exact
//!    `VkResult` of the step that fails; also try a single 2-plane
//!    `G8_B8R8_2PLANE_420` import (the route a UBWC buffer would need).
//! 4. Run the real fp-gfx path: `Renderer::upload_video_frame` with the
//!    DMA-BUF frame + `convert_video` (YUV→RGBA compute) + submit + wait.
//!
//! As a control, the same steps run on a linear buffer allocated from
//! `/dev/dma_heap/system` (separates "Vulkan cannot import DMA-BUF at all"
//! from "Vulkan rejects this decoder's buffers").

use super::vulkan::HeadlessGpu;
use crate::clips;
use crate::report::{CheckOutput, Status};
use crate::runner::CheckContext;
use crate::util::Out;
use ash::vk;
use fp_video::decode::drm;
use fp_video::{DecodedFrame, DecoderOptions};
use serde_json::{json, Value};
use std::ffi::CStr;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::time::Duration;

/// A logical device on [`HeadlessGpu`] set up like fp-xr does it.
pub struct HeadlessDevice {
    pub device: ash::Device,
    pub queue_family: u32,
    pub exts: fp_xr::DeviceExtensions,
    pub ycbcr: bool,
    pub gfx_features: bool,
    pub ext_fd: Option<ash::khr::external_memory_fd::Device>,
}

impl HeadlessDevice {
    pub fn new(gpu: &HeadlessGpu) -> Result<HeadlessDevice, String> {
        let i = &gpu.instance;
        // SAFETY: valid handles throughout.
        unsafe {
            let fams = i.get_physical_device_queue_family_properties(gpu.physical);
            let qf = fams
                .iter()
                .position(|f| {
                    f.queue_flags
                        .contains(vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE)
                })
                .ok_or("no graphics+compute queue family")? as u32;
            let avail = i
                .enumerate_device_extension_properties(gpu.physical)
                .map_err(|e| format!("{e:?}"))?;
            let names: Vec<&CStr> = avail
                .iter()
                .map(|e| CStr::from_ptr(e.extension_name.as_ptr()))
                .collect();
            let (enable, exts) = fp_xr::select::select_device_extensions(&names);
            let ptrs: Vec<_> = enable.iter().map(|n| n.as_ptr()).collect();
            let props = i.get_physical_device_properties(gpu.physical);
            let api13 =
                props.api_version >= vk::API_VERSION_1_3 && gpu.api_version >= vk::API_VERSION_1_3;
            let mut f11 = vk::PhysicalDeviceVulkan11Features::default();
            let mut f13 = vk::PhysicalDeviceVulkan13Features::default();
            if api13 {
                let mut f2 = vk::PhysicalDeviceFeatures2::default()
                    .push_next(&mut f11)
                    .push_next(&mut f13);
                i.get_physical_device_features2(gpu.physical, &mut f2);
            }
            let gfx = f13.dynamic_rendering != 0 && f13.synchronization2 != 0;
            let ycbcr = f11.sampler_ycbcr_conversion != 0;
            let mut en11 =
                vk::PhysicalDeviceVulkan11Features::default().sampler_ycbcr_conversion(ycbcr);
            let mut en13 = vk::PhysicalDeviceVulkan13Features::default()
                .dynamic_rendering(gfx)
                .synchronization2(gfx);
            let prio = [1.0f32];
            let q = [vk::DeviceQueueCreateInfo::default()
                .queue_family_index(qf)
                .queue_priorities(&prio)];
            let mut ci = vk::DeviceCreateInfo::default()
                .queue_create_infos(&q)
                .enabled_extension_names(&ptrs);
            if api13 {
                ci = ci.push_next(&mut en11).push_next(&mut en13);
            }
            let device = i
                .create_device(gpu.physical, &ci, None)
                .map_err(|e| format!("vkCreateDevice: {e:?}"))?;
            let ext_fd = exts
                .external_memory_fd
                .then(|| ash::khr::external_memory_fd::Device::new(i, &device));
            Ok(HeadlessDevice {
                device,
                queue_family: qf,
                exts,
                ycbcr,
                gfx_features: gfx,
                ext_fd,
            })
        }
    }
}

impl Drop for HeadlessDevice {
    fn drop(&mut self) {
        // SAFETY: everything created on this device was destroyed already.
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
        }
    }
}

/// One plane to import.
#[derive(Debug, Clone, Copy)]
pub struct PlaneSpec {
    pub fd: RawFd,
    pub offset: u64,
    pub pitch: u64,
}

/// Step-by-step import of `planes` as one image of `format`. Returns the
/// list of `step: result` strings and whether everything succeeded.
pub fn manual_import(
    gpu: &HeadlessGpu,
    dev: &HeadlessDevice,
    format: vk::Format,
    modifier: u64,
    width: u32,
    height: u32,
    planes: &[PlaneSpec],
) -> (Vec<String>, bool) {
    let mut steps = Vec::new();
    let (r, importable) = gpu.dmabuf_image_support(format, modifier);
    steps.push(format!(
        "GetPhysicalDeviceImageFormatProperties2: {r:?}, importable={importable}"
    ));
    let Some(ext_fd) = dev.ext_fd.as_ref() else {
        steps.push("VK_KHR_external_memory_fd not enabled".into());
        return (steps, false);
    };
    if !dev.exts.image_drm_format_modifier {
        steps.push("VK_EXT_image_drm_format_modifier not enabled".into());
        return (steps, false);
    }
    let d = &dev.device;
    let handle = vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT;
    let layouts: Vec<vk::SubresourceLayout> = planes
        .iter()
        .map(|p| vk::SubresourceLayout {
            offset: p.offset,
            size: 0,
            row_pitch: p.pitch,
            array_pitch: 0,
            depth_pitch: 0,
        })
        .collect();
    let mut explicit = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
        .drm_format_modifier(modifier)
        .plane_layouts(&layouts);
    let mut external = vk::ExternalMemoryImageCreateInfo::default().handle_types(handle);
    let info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(format)
        .extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(vk::ImageUsageFlags::SAMPLED)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut external)
        .push_next(&mut explicit);
    // SAFETY: valid create info; every object is destroyed before return.
    unsafe {
        let mut fd_props = vk::MemoryFdPropertiesKHR::default();
        let r = ext_fd.get_memory_fd_properties(handle, planes[0].fd, &mut fd_props);
        steps.push(format!(
            "GetMemoryFdPropertiesKHR: {:?}, memoryTypeBits={:#x}",
            r.err().unwrap_or(vk::Result::SUCCESS),
            fd_props.memory_type_bits
        ));
        let image = match d.create_image(&info, None) {
            Ok(i) => {
                steps.push("CreateImage: SUCCESS".into());
                i
            }
            Err(e) => {
                steps.push(format!("CreateImage: {e:?}"));
                return (steps, false);
            }
        };
        let req = d.get_image_memory_requirements(image);
        let bits = req.memory_type_bits & fd_props.memory_type_bits;
        steps.push(format!(
            "memory requirements: size {} typeBits {:#x} (with fd: {bits:#x})",
            req.size, req.memory_type_bits
        ));
        let mem_props = gpu
            .instance
            .get_physical_device_memory_properties(gpu.physical);
        let ty = (0..mem_props.memory_type_count).find(|t| bits & (1 << t) != 0);
        let Some(ty) = ty else {
            steps.push("no compatible memory type".into());
            d.destroy_image(image, None);
            return (steps, false);
        };
        let dup = match std::os::fd::BorrowedFd::borrow_raw(planes[0].fd).try_clone_to_owned() {
            Ok(f) => f,
            Err(e) => {
                steps.push(format!("dup fd: {e}"));
                d.destroy_image(image, None);
                return (steps, false);
            }
        };
        let mut import = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(handle)
            .fd(dup.as_raw_fd());
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(ty)
            .push_next(&mut import)
            .push_next(&mut dedicated);
        let mem = match d.allocate_memory(&alloc, None) {
            Ok(m) => {
                let _ = dup.into_raw_fd(); // owned by Vulkan now
                steps.push("AllocateMemory(import): SUCCESS".into());
                m
            }
            Err(e) => {
                steps.push(format!("AllocateMemory(import): {e:?}"));
                d.destroy_image(image, None);
                return (steps, false);
            }
        };
        let ok = match d.bind_image_memory(image, mem, 0) {
            Ok(()) => {
                steps.push("BindImageMemory: SUCCESS".into());
                true
            }
            Err(e) => {
                steps.push(format!("BindImageMemory: {e:?}"));
                false
            }
        };
        if ok {
            let mut mp = vk::ImageDrmFormatModifierPropertiesEXT::default();
            let loader = ash::ext::image_drm_format_modifier::Device::new(&gpu.instance, d);
            if loader
                .get_image_drm_format_modifier_properties(image, &mut mp)
                .is_ok()
            {
                steps.push(format!(
                    "image modifier reported back: {:#x}",
                    mp.drm_format_modifier
                ));
            }
        }
        d.free_memory(mem, None);
        d.destroy_image(image, None);
        (steps, ok)
    }
}

/// Run the fp-gfx renderer path on `frame` (one upload + conversion).
pub fn renderer_import(
    gpu: &HeadlessGpu,
    dev: &HeadlessDevice,
    frame: fp_gfx::DmaBufFrame,
) -> Result<(), String> {
    if !dev.gfx_features {
        return Err("device lacks dynamicRendering/synchronization2; fp-gfx cannot run".into());
    }
    let caps = fp_gfx::DeviceCaps {
        dmabuf_import: dev.exts.external_memory_fd && dev.exts.external_memory_dma_buf,
        drm_format_modifier: dev.exts.image_drm_format_modifier,
        queue_family_foreign: dev.exts.queue_family_foreign,
        sampler_ycbcr_conversion: dev.ycbcr,
    };
    // SAFETY: the device was created with 1.3 dynamicRendering +
    // synchronization2 and the extensions in `caps`; it outlives the
    // renderer (dropped at the end of this function).
    let ctx = unsafe {
        fp_gfx::GpuContext::new(
            gpu.instance.clone(),
            gpu.physical,
            dev.device.clone(),
            dev.queue_family,
            0,
            caps,
        )
    };
    let mut r = fp_gfx::Renderer::new(ctx, fp_gfx::RendererConfig::default())
        .map_err(|e| format!("Renderer::new: {e}"))?;
    let step = (|| -> Result<(), String> {
        r.begin_frame().map_err(|e| format!("begin_frame: {e}"))?;
        r.upload_video_frame(&fp_gfx::VideoFrame::DmaBuf(frame))
            .map_err(|e| format!("upload_video_frame: {e}"))?;
        r.convert_video(&fp_core::Corrections::default())
            .map_err(|e| format!("convert_video: {e}"))?;
        r.end_frame().map_err(|e| format!("end_frame: {e}"))?;
        r.wait_idle().map_err(|e| format!("wait_idle: {e}"))
    })();
    if step.is_err() {
        let _ = r.end_frame();
        let _ = r.wait_idle();
    }
    drop(r);
    step
}

/// fp-video frame → fp-gfx frame (same mapping the app's FrameConverter uses).
pub fn to_gfx(f: &fp_video::DmaBufFrame) -> Result<fp_gfx::DmaBufFrame, String> {
    let format = match f.fourcc {
        drm::FORMAT_NV12 => fp_gfx::PixelFormat::Nv12,
        drm::FORMAT_P010 => fp_gfx::PixelFormat::P010,
        other => return Err(format!("unsupported fourcc {other:#x}")),
    };
    if f.planes.len() < 2 {
        return Err(format!("{} plane(s), need 2", f.planes.len()));
    }
    let p = |i: usize| fp_gfx::DmaBufPlane {
        fd: f.planes[i].fd,
        offset: f.planes[i].offset as u64,
        pitch: f.planes[i].pitch as u64,
    };
    let mut color = fp_gfx::ColorInfo::SDR_709;
    if format == fp_gfx::PixelFormat::P010 {
        color.bit_depth = 10;
    }
    Ok(fp_gfx::DmaBufFrame {
        buffer_id: f.buffer_id,
        width: f.width,
        height: f.height,
        format,
        modifier: f.modifier,
        planes: [p(0), p(1)],
        color,
    })
}

/// `_IOWR('H', 0, struct dma_heap_allocation_data)`.
const DMA_HEAP_IOCTL_ALLOC: libc::c_ulong = 0xc018_4800;

#[repr(C)]
struct DmaHeapAlloc {
    len: u64,
    fd: u32,
    fd_flags: u32,
    heap_flags: u64,
}

/// Allocate `len` bytes from `/dev/dma_heap/system`.
pub fn dma_heap_alloc(len: u64) -> Result<OwnedFd, String> {
    let path = c"/dev/dma_heap/system";
    // SAFETY: open + ioctl with a correctly laid out struct.
    unsafe {
        let heap = libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
        if heap < 0 {
            return Err(format!(
                "open /dev/dma_heap/system: {}",
                std::io::Error::last_os_error()
            ));
        }
        let heap = OwnedFd::from_raw_fd(heap);
        let mut a = DmaHeapAlloc {
            len,
            fd: 0,
            fd_flags: (libc::O_RDWR | libc::O_CLOEXEC) as u32,
            heap_flags: 0,
        };
        if libc::ioctl(heap.as_raw_fd(), DMA_HEAP_IOCTL_ALLOC as _, &mut a) != 0 {
            return Err(format!(
                "DMA_HEAP_IOCTL_ALLOC: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(OwnedFd::from_raw_fd(a.fd as RawFd))
    }
}

pub fn run(ctx: &CheckContext) -> CheckOutput {
    let mut o = Out::new();

    // 1. A real decoder frame.
    let devices = fp_video::decode::v4l2::enumerate_devices();
    let mut decoded = None;
    let mut decode_errors = Vec::new();
    for name in ["hevc_256x256", "h264_256x256", "vp9_256x256", "av1_256x256"] {
        if devices.is_empty() {
            break;
        }
        let clip = clips::by_name(name).expect("embedded clip");
        let (s, d) = super::video::decode_clip(
            clip,
            &DecoderOptions::default(),
            &devices,
            true,
            Duration::from_secs(12),
        );
        match d {
            Some(d) => {
                decoded = Some(d);
                break;
            }
            None => decode_errors.push(format!("{name}: {}", s.error.unwrap_or_default())),
        }
    }
    if let Some(d) = &decoded {
        o.set(
            "decoded_frame",
            json!({
                "clip": d.stats.clip,
                "device": d.stats.device,
                "frame": super::video::describe_frame(&d.first),
            }),
        );
    } else {
        o.set("decode_errors", &decode_errors);
    }
    ctx.partial(&o.snapshot(Status::Unknown, "decoded a frame"));

    // 2. Vulkan device.
    let gpu = match HeadlessGpu::new() {
        Ok(g) => g,
        Err(e) => {
            o.finding(
                "dmabuf_import",
                Status::Unknown,
                &["P4"],
                format!("Vulkan not available: {e}"),
            );
            return o.finish(Status::Unknown, format!("Vulkan not available: {e}"));
        }
    };
    let dev = match HeadlessDevice::new(&gpu) {
        Ok(d) => d,
        Err(e) => {
            o.finding(
                "dmabuf_import",
                Status::Fail,
                &["P4"],
                format!("could not create a Vulkan device: {e}"),
            );
            return o.finish(Status::Fail, format!("Vulkan device creation failed: {e}"));
        }
    };
    o.set("device_extensions_enabled", format!("{:?}", dev.exts));

    // Control: linear buffer from the system DMA heap.
    let (w, h) = (256u32, 256u32);
    match dma_heap_alloc((w * h * 3 / 2) as u64) {
        Ok(fd) => {
            let planes = [PlaneSpec {
                fd: fd.as_raw_fd(),
                offset: 0,
                pitch: w as u64,
            }];
            let (steps, ok) = manual_import(&gpu, &dev, vk::Format::R8_UNORM, 0, w, h, &planes);
            o.set(
                "control_dma_heap_import",
                json!({ "ok": ok, "steps": steps }),
            );
            o.finding(
                "control_import",
                if ok { Status::Pass } else { Status::Fail },
                &["P4"],
                format!(
                    "control: linear DMA-BUF from /dev/dma_heap/system {} into Vulkan as R8 ({})",
                    if ok { "imports" } else { "does NOT import" },
                    steps.last().cloned().unwrap_or_default()
                ),
            );
        }
        Err(e) => o.set("control_dma_heap_import", json!({ "skipped": e })),
    }

    // 3 + 4. The decoder frame.
    let Some(d) = decoded else {
        let msg = if devices.is_empty() {
            "no V4L2 decoder, so no decoder DMA-BUF to import".to_string()
        } else {
            format!("no frame decoded: {}", decode_errors.join("; "))
        };
        o.finding("dmabuf_import", Status::Unknown, &["P4"], msg.clone());
        return o.finish(Status::Unknown, msg);
    };
    let DecodedFrame::DmaBuf(frame) = &d.first else {
        o.finding(
            "dmabuf_import",
            Status::Fail,
            &["P4"],
            "decoder returned a CPU frame",
        );
        return o.finish(Status::Fail, "decoder returned a CPU frame, not a DMA-BUF");
    };
    let ten = frame.fourcc == drm::FORMAT_P010;
    let (yf, cf) = if ten {
        (vk::Format::R16_UNORM, vk::Format::R16G16_UNORM)
    } else {
        (vk::Format::R8_UNORM, vk::Format::R8G8_UNORM)
    };
    let mut per_plane = serde_json::Map::new();
    let mut planes_ok = true;
    for (i, (fmt, pw, ph)) in [
        (yf, frame.width, frame.height),
        (cf, frame.width.div_ceil(2), frame.height.div_ceil(2)),
    ]
    .into_iter()
    .enumerate()
    {
        let Some(p) = frame.planes.get(i) else {
            continue;
        };
        let spec = [PlaneSpec {
            fd: p.fd,
            offset: p.offset as u64,
            pitch: p.pitch as u64,
        }];
        let (steps, ok) = manual_import(&gpu, &dev, fmt, frame.modifier, pw, ph, &spec);
        planes_ok &= ok;
        per_plane.insert(
            format!("plane{i}_{fmt:?}"),
            json!({ "ok": ok, "steps": steps }),
        );
    }
    o.set("per_plane_import", Value::Object(per_plane));
    // Whole-frame 2-plane import (needed for UBWC); only possible with one fd.
    let single_fd = frame.planes.iter().all(|p| p.fd == frame.planes[0].fd);
    if single_fd && frame.planes.len() >= 2 {
        let mp = if ten {
            vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16
        } else {
            vk::Format::G8_B8R8_2PLANE_420_UNORM
        };
        let specs: Vec<PlaneSpec> = frame
            .planes
            .iter()
            .take(2)
            .map(|p| PlaneSpec {
                fd: p.fd,
                offset: p.offset as u64,
                pitch: p.pitch as u64,
            })
            .collect();
        let (steps, ok) = manual_import(
            &gpu,
            &dev,
            mp,
            frame.modifier,
            frame.width,
            frame.height,
            &specs,
        );
        o.set(
            "multiplanar_import",
            json!({ "format": format!("{mp:?}"), "ok": ok, "steps": steps }),
        );
    }
    let renderer = to_gfx(frame).and_then(|g| renderer_import(&gpu, &dev, g));
    o.set(
        "fp_gfx_renderer_import",
        match &renderer {
            Ok(()) => json!({ "ok": true }),
            Err(e) => json!({ "ok": false, "error": e }),
        },
    );
    let fmt_desc = format!(
        "{} {}",
        fp_video::decode::v4l2::sys::fourcc_str(frame.fourcc),
        super::video::modifier_str(frame.modifier)
    );
    let (st, msg) = match &renderer {
        Ok(()) => (
            Status::Pass,
            format!("ZERO-COPY WORKS: decoder DMA-BUF ({fmt_desc}) imported and converted by fp-gfx"),
        ),
        Err(e) => (
            Status::Fail,
            format!(
                "decoder DMA-BUF ({fmt_desc}) NOT importable by fp-gfx: {e}; manual per-plane import {}",
                if planes_ok { "succeeded" } else { "failed too" }
            ),
        ),
    };
    o.finding("dmabuf_import", st, &["P4"], msg.clone());
    drop(dev);
    drop(gpu);
    drop(d);
    o.finish(st, msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heap_ioctl_layout() {
        assert_eq!(std::mem::size_of::<DmaHeapAlloc>(), 24);
    }

    #[test]
    fn frame_mapping() {
        let f = fp_video::DmaBufFrame {
            buffer_id: 7,
            planes: vec![
                fp_video::DmaBufPlane {
                    fd: 5,
                    offset: 0,
                    pitch: 256,
                },
                fp_video::DmaBufPlane {
                    fd: 5,
                    offset: 65536,
                    pitch: 256,
                },
            ],
            fourcc: drm::FORMAT_P010,
            modifier: 0,
            width: 256,
            height: 256,
            coded_width: 256,
            coded_height: 256,
            pts: fp_core::MediaTime::ZERO,
            lease: None,
        };
        let g = to_gfx(&f).unwrap();
        assert_eq!(g.format, fp_gfx::PixelFormat::P010);
        assert_eq!(g.planes[1].offset, 65536);
        assert_eq!(g.color.bit_depth, 10);
        let mut bad = f.clone();
        bad.fourcc = 1;
        assert!(to_gfx(&bad).is_err());
        bad.fourcc = drm::FORMAT_NV12;
        bad.planes.truncate(1);
        assert!(to_gfx(&bad).is_err());
    }
}
