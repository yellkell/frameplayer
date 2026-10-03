//! Vulkan without OpenXR: loader, instance, physical devices, driver
//! identity, device extensions (zero-copy and Vulkan Video ones called
//! out), video-capable queue families and decode capabilities, DRM format
//! modifier support for the plane formats fp-gfx imports, and whether the
//! LINEAR / Qualcomm UBWC modifiers are importable as DMA-BUF.
//! Answers P4 (driver side), P5 and P9.
//!
//! [`HeadlessGpu`] is also used by the `dmabuf_import` check.

use crate::report::{CheckOutput, Status};
use crate::runner::CheckContext;
use crate::util::Out;
use ash::vk;
use serde_json::{json, Value};
use std::ffi::{c_char, CStr};

pub const ZERO_COPY_EXTS: &[&str] = &[
    "VK_KHR_external_memory_fd",
    "VK_EXT_external_memory_dma_buf",
    "VK_EXT_image_drm_format_modifier",
    "VK_EXT_queue_family_foreign",
];

pub const VIDEO_EXTS: &[&str] = &[
    "VK_KHR_video_queue",
    "VK_KHR_video_decode_queue",
    "VK_KHR_video_decode_h264",
    "VK_KHR_video_decode_h265",
    "VK_KHR_video_decode_av1",
    "VK_KHR_video_maintenance1",
];

/// Formats whose DRM modifier support matters for decoder import.
pub const MODIFIER_FORMATS: &[(vk::Format, &str)] = &[
    (vk::Format::R8_UNORM, "R8_UNORM"),
    (vk::Format::R8G8_UNORM, "R8G8_UNORM"),
    (vk::Format::R16_UNORM, "R16_UNORM"),
    (vk::Format::R16G16_UNORM, "R16G16_UNORM"),
    (
        vk::Format::G8_B8R8_2PLANE_420_UNORM,
        "G8_B8R8_2PLANE_420_UNORM",
    ),
    (
        vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16,
        "G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16",
    ),
];

pub const MOD_LINEAR: u64 = 0;
pub const MOD_QCOM_COMPRESSED: u64 = fp_video::decode::drm::MOD_QCOM_COMPRESSED;

pub fn version_str(v: u32) -> String {
    format!(
        "{}.{}.{}",
        vk::api_version_major(v),
        vk::api_version_minor(v),
        vk::api_version_patch(v)
    )
}

/// Short tags for format feature bits relevant here.
pub fn feature_tags(f: vk::FormatFeatureFlags) -> String {
    let mut s = String::new();
    for (bit, c) in [
        (vk::FormatFeatureFlags::SAMPLED_IMAGE, 'S'),
        (vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR, 'L'),
        (vk::FormatFeatureFlags::STORAGE_IMAGE, 'T'),
        (vk::FormatFeatureFlags::COLOR_ATTACHMENT, 'C'),
        (vk::FormatFeatureFlags::TRANSFER_SRC, 'r'),
        (vk::FormatFeatureFlags::TRANSFER_DST, 'w'),
        (vk::FormatFeatureFlags::DISJOINT, 'D'),
        (vk::FormatFeatureFlags::MIDPOINT_CHROMA_SAMPLES, 'M'),
    ] {
        if f.contains(bit) {
            s.push(c);
        }
    }
    s
}

fn cstr_arr(a: &[c_char]) -> String {
    // SAFETY: Vulkan fixed-size strings are NUL-terminated.
    unsafe { CStr::from_ptr(a.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

/// A Vulkan instance + chosen physical device created without OpenXR.
pub struct HeadlessGpu {
    pub entry: ash::Entry,
    pub instance: ash::Instance,
    pub physical: vk::PhysicalDevice,
    pub api_version: u32,
    pub device_exts: Vec<String>,
}

impl HeadlessGpu {
    /// Load the loader, create an instance (API ≤ 1.3) and pick the first
    /// physical device (preferring integrated / discrete GPUs).
    pub fn new() -> Result<HeadlessGpu, String> {
        // SAFETY: loading the system Vulkan loader.
        let entry = unsafe { ash::Entry::load() }.map_err(|e| format!("loading libvulkan: {e}"))?;
        // SAFETY: valid entry.
        let inst_ver = unsafe { entry.try_enumerate_instance_version() }
            .ok()
            .flatten()
            .unwrap_or(vk::API_VERSION_1_0);
        let api = if inst_ver >= vk::API_VERSION_1_3 {
            vk::API_VERSION_1_3
        } else {
            inst_ver
        };
        let app = vk::ApplicationInfo::default()
            .application_name(c"frameplayer-probe")
            .api_version(api);
        let ci = vk::InstanceCreateInfo::default().application_info(&app);
        // SAFETY: valid create info.
        let instance = unsafe { entry.create_instance(&ci, None) }
            .map_err(|e| format!("vkCreateInstance: {e:?}"))?;
        // SAFETY: valid instance.
        let pds = unsafe { instance.enumerate_physical_devices() }.unwrap_or_default();
        let pick = pds.iter().copied().max_by_key(|&pd| {
            // SAFETY: valid handle.
            let t = unsafe { instance.get_physical_device_properties(pd) }.device_type;
            match t {
                vk::PhysicalDeviceType::INTEGRATED_GPU => 3,
                vk::PhysicalDeviceType::DISCRETE_GPU => 2,
                vk::PhysicalDeviceType::VIRTUAL_GPU => 1,
                _ => 0,
            }
        });
        let Some(physical) = pick else {
            // SAFETY: nothing else uses the instance.
            unsafe { instance.destroy_instance(None) };
            return Err("no Vulkan physical devices (no ICD/driver for this GPU)".into());
        };
        // SAFETY: valid handles.
        let device_exts = unsafe { instance.enumerate_device_extension_properties(physical) }
            .unwrap_or_default()
            .iter()
            .map(|e| cstr_arr(&e.extension_name))
            .collect();
        Ok(HeadlessGpu {
            entry,
            instance,
            physical,
            api_version: inst_ver,
            device_exts,
        })
    }

    pub fn has(&self, ext: &str) -> bool {
        self.device_exts.iter().any(|e| e == ext)
    }

    /// Modifiers offered for `format`: `(modifier, planes, features)`.
    pub fn modifiers(&self, format: vk::Format) -> Vec<(u64, u32, vk::FormatFeatureFlags)> {
        if !self.has("VK_EXT_image_drm_format_modifier") {
            return Vec::new();
        }
        // SAFETY: two-call idiom with properly chained structs.
        unsafe {
            let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
            let mut props = vk::FormatProperties2::default().push_next(&mut list);
            self.instance
                .get_physical_device_format_properties2(self.physical, format, &mut props);
            let n = list.drm_format_modifier_count as usize;
            if n == 0 {
                return Vec::new();
            }
            let mut mods = vec![vk::DrmFormatModifierPropertiesEXT::default(); n];
            let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
                .drm_format_modifier_properties(&mut mods);
            let mut props = vk::FormatProperties2::default().push_next(&mut list);
            self.instance
                .get_physical_device_format_properties2(self.physical, format, &mut props);
            mods.iter()
                .map(|m| {
                    (
                        m.drm_format_modifier,
                        m.drm_format_modifier_plane_count,
                        m.drm_format_modifier_tiling_features,
                    )
                })
                .collect()
        }
    }

    /// `vkGetPhysicalDeviceImageFormatProperties2` for a sampled DMA-BUF
    /// image with `modifier`: the result and whether import is allowed.
    pub fn dmabuf_image_support(&self, format: vk::Format, modifier: u64) -> (vk::Result, bool) {
        if !self.has("VK_EXT_image_drm_format_modifier") {
            return (vk::Result::ERROR_EXTENSION_NOT_PRESENT, false);
        }
        let mut mod_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
            .drm_format_modifier(modifier)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let mut ext_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let info = vk::PhysicalDeviceImageFormatInfo2::default()
            .format(format)
            .ty(vk::ImageType::TYPE_2D)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .push_next(&mut ext_info)
            .push_next(&mut mod_info);
        let mut ext_props = vk::ExternalImageFormatProperties::default();
        let mut props = vk::ImageFormatProperties2::default().push_next(&mut ext_props);
        // SAFETY: valid chains.
        let r = unsafe {
            self.instance.get_physical_device_image_format_properties2(
                self.physical,
                &info,
                &mut props,
            )
        };
        let importable = ext_props
            .external_memory_properties
            .external_memory_features
            .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE);
        match r {
            Ok(()) => (vk::Result::SUCCESS, importable),
            Err(e) => (e, false),
        }
    }
}

impl Drop for HeadlessGpu {
    fn drop(&mut self) {
        // SAFETY: devices created from this instance are destroyed first
        // by their owners.
        unsafe { self.instance.destroy_instance(None) };
    }
}

fn video_caps(gpu: &HeadlessGpu) -> Value {
    if !gpu.has("VK_KHR_video_queue") {
        return Value::Null;
    }
    let vq = ash::khr::video_queue::Instance::new(&gpu.entry, &gpu.instance);
    let get = vq.fp().get_physical_device_video_capabilities_khr;
    let mut out = serde_json::Map::new();
    let b8 = vk::VideoComponentBitDepthFlagsKHR::TYPE_8;
    let b10 = vk::VideoComponentBitDepthFlagsKHR::TYPE_10;
    let mut query = |name: &str,
                     op: vk::VideoCodecOperationFlagsKHR,
                     depth,
                     next: *const std::ffi::c_void| {
        let mut profile = vk::VideoProfileInfoKHR::default()
            .video_codec_operation(op)
            .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
            .luma_bit_depth(depth)
            .chroma_bit_depth(depth);
        profile.p_next = next;
        let mut h264 = vk::VideoDecodeH264CapabilitiesKHR::default();
        let mut h265 = vk::VideoDecodeH265CapabilitiesKHR::default();
        let mut av1 = vk::VideoDecodeAV1CapabilitiesKHR::default();
        let mut dec = vk::VideoDecodeCapabilitiesKHR::default();
        let mut caps = vk::VideoCapabilitiesKHR::default();
        dec.p_next = match op {
            vk::VideoCodecOperationFlagsKHR::DECODE_H264 => &mut h264 as *mut _ as *mut _,
            vk::VideoCodecOperationFlagsKHR::DECODE_H265 => &mut h265 as *mut _ as *mut _,
            _ => &mut av1 as *mut _ as *mut _,
        };
        caps.p_next = &mut dec as *mut _ as *mut _;
        // SAFETY: properly chained structs that outlive the call.
        let r = unsafe { get(gpu.physical, &profile, &mut caps) };
        let v = if r == vk::Result::SUCCESS {
            json!({
                "max_coded_extent": format!("{}x{}", caps.max_coded_extent.width, caps.max_coded_extent.height),
                "max_dpb_slots": caps.max_dpb_slots,
                "max_active_refs": caps.max_active_reference_pictures,
            })
        } else {
            json!({ "error": format!("{r:?}") })
        };
        out.insert(name.into(), v);
    };
    if gpu.has("VK_KHR_video_decode_h264") {
        let p = vk::VideoDecodeH264ProfileInfoKHR::default()
            .std_profile_idc(
                ash::vk::native::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH,
            )
            .picture_layout(vk::VideoDecodeH264PictureLayoutFlagsKHR::PROGRESSIVE);
        query(
            "h264_high",
            vk::VideoCodecOperationFlagsKHR::DECODE_H264,
            b8,
            &p as *const _ as *const _,
        );
    }
    if gpu.has("VK_KHR_video_decode_h265") {
        let p = vk::VideoDecodeH265ProfileInfoKHR::default().std_profile_idc(
            ash::vk::native::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN,
        );
        query(
            "h265_main",
            vk::VideoCodecOperationFlagsKHR::DECODE_H265,
            b8,
            &p as *const _ as *const _,
        );
        let p = vk::VideoDecodeH265ProfileInfoKHR::default().std_profile_idc(
            ash::vk::native::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN_10,
        );
        query(
            "h265_main10",
            vk::VideoCodecOperationFlagsKHR::DECODE_H265,
            b10,
            &p as *const _ as *const _,
        );
    }
    if gpu.has("VK_KHR_video_decode_av1") {
        for (name, depth) in [("av1_main_8bit", b8), ("av1_main_10bit", b10)] {
            let p = vk::VideoDecodeAV1ProfileInfoKHR::default()
                .std_profile(ash::vk::native::StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_MAIN);
            query(
                name,
                vk::VideoCodecOperationFlagsKHR::DECODE_AV1,
                depth,
                &p as *const _ as *const _,
            );
        }
    }
    Value::Object(out)
}

pub fn run(_ctx: &CheckContext) -> CheckOutput {
    let mut o = Out::new();
    let gpu = match HeadlessGpu::new() {
        Ok(g) => g,
        Err(e) => {
            let st = if e.contains("libvulkan") {
                Status::Fail
            } else {
                Status::Unknown
            };
            o.finding(
                "vulkan_instance",
                st,
                &["P9"],
                format!("Vulkan not usable here: {e}"),
            );
            o.set("error", &e);
            return o.finish(Status::Unknown, format!("Vulkan not available: {e}"));
        }
    };
    o.finding(
        "vulkan_instance",
        Status::Pass,
        &["P9"],
        format!(
            "vkCreateInstance works outside the Steam Runtime (loader API {})",
            version_str(gpu.api_version)
        ),
    );
    // SAFETY: valid handles.
    let inst_exts: Vec<String> = unsafe { gpu.entry.enumerate_instance_extension_properties(None) }
        .unwrap_or_default()
        .iter()
        .map(|e| cstr_arr(&e.extension_name))
        .collect();
    // SAFETY: valid handles.
    let all_pds = unsafe { gpu.instance.enumerate_physical_devices() }.unwrap_or_default();
    let mut devices = Vec::new();
    for &pd in &all_pds {
        // SAFETY: valid handle.
        let p = unsafe { gpu.instance.get_physical_device_properties(pd) };
        devices.push(format!(
            "{} ({:?}, API {})",
            cstr_arr(&p.device_name),
            p.device_type,
            version_str(p.api_version)
        ));
    }

    // Properties of the chosen device, including driver identity.
    let mut driver = vk::PhysicalDeviceDriverProperties::default();
    let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut driver);
    // SAFETY: valid chain.
    unsafe {
        gpu.instance
            .get_physical_device_properties2(gpu.physical, &mut props2)
    };
    let p = props2.properties;
    let name = cstr_arr(&p.device_name);
    let driver_name = cstr_arr(&driver.driver_name);
    let driver_info = cstr_arr(&driver.driver_info);
    let dev_api = p.api_version;
    o.set(
        "device",
        json!({
            "name": name,
            "type": format!("{:?}", p.device_type),
            "api_version": version_str(dev_api),
            "driver_version_raw": format!("{:#x}", p.driver_version),
            "driver_id": format!("{:?}", driver.driver_id),
            "driver_name": driver_name,
            "driver_info": driver_info,
            "conformance": format!("{}.{}.{}.{}", driver.conformance_version.major, driver.conformance_version.minor, driver.conformance_version.subminor, driver.conformance_version.patch),
            "vendor_id": format!("{:#06x}", p.vendor_id),
            "device_id": format!("{:#06x}", p.device_id),
            "max_image_dimension_2d": p.limits.max_image_dimension2_d,
        }),
    );
    o.set("all_devices", &devices);
    o.set("instance_extensions", inst_exts.join(" "));

    // Features fp-gfx needs.
    let mut f11 = vk::PhysicalDeviceVulkan11Features::default();
    let mut f13 = vk::PhysicalDeviceVulkan13Features::default();
    let api13 = dev_api >= vk::API_VERSION_1_3;
    let feats = if api13 {
        let mut f2 = vk::PhysicalDeviceFeatures2::default()
            .push_next(&mut f11)
            .push_next(&mut f13);
        // SAFETY: valid chain.
        unsafe {
            gpu.instance
                .get_physical_device_features2(gpu.physical, &mut f2)
        };
        json!({
            "dynamic_rendering": f13.dynamic_rendering != 0,
            "synchronization2": f13.synchronization2 != 0,
            "sampler_ycbcr_conversion": f11.sampler_ycbcr_conversion != 0,
        })
    } else {
        json!({ "note": "device API < 1.3" })
    };
    let gfx_ok = api13 && f13.dynamic_rendering != 0 && f13.synchronization2 != 0;
    o.set("features", feats);
    o.finding(
        "vulkan13",
        if gfx_ok { Status::Pass } else { Status::Fail },
        &[],
        format!(
            "{name}: Vulkan {} via {driver_name} {driver_info}; dynamicRendering+synchronization2 {}",
            version_str(dev_api),
            if gfx_ok { "supported" } else { "MISSING (fp-gfx needs them)" }
        ),
    );

    // Extensions.
    let zc: Vec<(&str, bool)> = ZERO_COPY_EXTS.iter().map(|e| (*e, gpu.has(e))).collect();
    let vid: Vec<(&str, bool)> = VIDEO_EXTS.iter().map(|e| (*e, gpu.has(e))).collect();
    o.set(
        "zero_copy_extensions",
        zc.iter()
            .map(|(e, b)| (e.to_string(), *b))
            .collect::<std::collections::BTreeMap<_, _>>(),
    );
    o.set(
        "video_extensions",
        vid.iter()
            .map(|(e, b)| (e.to_string(), *b))
            .collect::<std::collections::BTreeMap<_, _>>(),
    );
    // One space-separated string: compact in the pretty-printed report.
    o.set("device_extensions", gpu.device_exts.join(" "));
    let zc_missing: Vec<&str> = zc
        .iter()
        .filter(|(_, b)| !b)
        .map(|(e, _)| *e)
        .take(3)
        .collect();
    let zc_core_ok = zc.iter().take(3).all(|(_, b)| *b);
    o.finding(
        "zero_copy_extensions",
        if zc_core_ok {
            Status::Pass
        } else {
            Status::Fail
        },
        &["P4"],
        if zc_core_ok {
            format!(
                "DMA-BUF import extensions present (queue_family_foreign: {})",
                if gpu.has("VK_EXT_queue_family_foreign") {
                    "yes"
                } else {
                    "no"
                }
            )
        } else {
            format!("missing {}", zc_missing.join(", "))
        },
    );

    // Queue families incl. video.
    // SAFETY: valid handle; two-call idiom.
    let n = unsafe {
        gpu.instance
            .get_physical_device_queue_family_properties2_len(gpu.physical)
    };
    let mut vprops = vec![vk::QueueFamilyVideoPropertiesKHR::default(); n];
    let mut qprops: Vec<vk::QueueFamilyProperties2> = vprops
        .iter_mut()
        .map(|v| {
            let mut q = vk::QueueFamilyProperties2::default();
            if gpu.has("VK_KHR_video_queue") {
                q.p_next = v as *mut _ as *mut _;
            }
            q
        })
        .collect();
    // SAFETY: output array sized above, chains point into vprops.
    unsafe {
        gpu.instance
            .get_physical_device_queue_family_properties2(gpu.physical, &mut qprops)
    };
    let mut queues = Vec::new();
    let mut video_decode_queue = false;
    for (i, q) in qprops.iter().enumerate() {
        let flags = q.queue_family_properties.queue_flags;
        let dec = flags.contains(vk::QueueFlags::VIDEO_DECODE_KHR);
        video_decode_queue |= dec;
        queues.push(format!(
            "#{i}: {:?} x{}{}",
            flags,
            q.queue_family_properties.queue_count,
            if dec {
                format!(" codecs {:?}", vprops[i].video_codec_operations)
            } else {
                String::new()
            }
        ));
    }
    drop(qprops);
    o.set("queue_families", &queues);
    let vcaps = video_caps(&gpu);
    o.set("video_decode_capabilities", &vcaps);
    let vk_video_codecs: Vec<&str> = [
        ("VK_KHR_video_decode_h264", "H.264"),
        ("VK_KHR_video_decode_h265", "HEVC"),
        ("VK_KHR_video_decode_av1", "AV1"),
    ]
    .iter()
    .filter(|(e, _)| gpu.has(e))
    .map(|(_, n)| *n)
    .collect();
    o.finding(
        "vulkan_video",
        if video_decode_queue && !vk_video_codecs.is_empty() {
            Status::Pass
        } else {
            Status::Fail
        },
        &["P5"],
        if vk_video_codecs.is_empty() {
            "no Vulkan Video decode extensions".to_string()
        } else {
            format!(
                "Vulkan Video decode: {} (decode queue family: {}){}",
                vk_video_codecs.join(", "),
                if video_decode_queue { "yes" } else { "no" },
                vcaps
                    .get("h265_main10")
                    .and_then(|v| v.get("max_coded_extent"))
                    .and_then(|v| v.as_str())
                    .map(|m| format!(", HEVC Main10 max {m}"))
                    .unwrap_or_default()
            )
        },
    );

    // DRM format modifiers.
    let mut mods = serde_json::Map::new();
    for &(f, fname) in MODIFIER_FORMATS {
        let list: Vec<String> = gpu
            .modifiers(f)
            .iter()
            .map(|(m, planes, feat)| format!("{m:#x}/{planes}p/{}", feature_tags(*feat)))
            .collect();
        mods.insert(fname.into(), json!(list));
    }
    o.set("drm_format_modifiers", Value::Object(mods));
    o.set(
        "drm_format_modifiers_legend",
        "modifier/plane count/features: S sampled, L linear filter, T storage, C colour attachment, r/w transfer src/dst, D disjoint, M midpoint chroma",
    );
    let mut import = serde_json::Map::new();
    let mut linear_ok = false;
    let mut ubwc_ok = false;
    for &(f, fname) in MODIFIER_FORMATS {
        for (m, mname) in [(MOD_LINEAR, "linear"), (MOD_QCOM_COMPRESSED, "qcom_ubwc")] {
            let (r, importable) = gpu.dmabuf_image_support(f, m);
            if importable
                && m == MOD_LINEAR
                && (f == vk::Format::R8_UNORM || f == vk::Format::R8G8_UNORM)
            {
                linear_ok = true;
            }
            if importable && m == MOD_QCOM_COMPRESSED && f == vk::Format::G8_B8R8_2PLANE_420_UNORM {
                ubwc_ok = true;
            }
            import.insert(
                format!("{fname}+{mname}"),
                json!(if r == vk::Result::SUCCESS {
                    if importable {
                        "importable".to_string()
                    } else {
                        "supported, not importable".to_string()
                    }
                } else {
                    format!("{r:?}")
                }),
            );
        }
    }
    o.set("dmabuf_image_support", Value::Object(import));
    o.finding(
        "modifier_support",
        if linear_ok {
            Status::Pass
        } else {
            Status::Fail
        },
        &["P4"],
        format!(
            "sampled DMA-BUF images: LINEAR R8/RG8 {}; UBWC NV12 (2-plane) {}",
            if linear_ok {
                "importable"
            } else {
                "NOT importable"
            },
            if ubwc_ok {
                "importable"
            } else {
                "not importable"
            }
        ),
    );

    let status = crate::checks::combine(&o);
    let status = if status == Status::Fail && gfx_ok && zc_core_ok {
        Status::Pass
    } else {
        status
    };
    o.finish(
        status,
        format!(
            "{name} / {driver_name} {driver_info}, Vulkan {}; zero-copy exts {}; Vulkan Video {}",
            version_str(dev_api),
            if zc_core_ok { "yes" } else { "no" },
            if vk_video_codecs.is_empty() {
                "none".to_string()
            } else {
                vk_video_codecs.join("/")
            }
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_and_versions() {
        let f = vk::FormatFeatureFlags::SAMPLED_IMAGE
            | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR
            | vk::FormatFeatureFlags::DISJOINT;
        assert_eq!(feature_tags(f), "SLD");
        assert_eq!(version_str(vk::make_api_version(0, 1, 3, 289)), "1.3.289");
        assert_eq!(MOD_QCOM_COMPRESSED, 0x0500_0000_0000_0001);
    }
}
