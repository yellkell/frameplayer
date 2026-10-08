//! Vulkan: driver identity, extensions that matter for zero-copy video and
//! foveation, Vulkan Video decode capabilities, and DRM format modifiers for
//! the decoder's output formats.

use ash::vk;
use serde::Serialize;
use std::ffi::{CStr, CString};

#[derive(Serialize, Default)]
pub struct VkReport {
    pub loader_api_version: Option<String>,
    pub instance_extensions: Vec<String>,
    pub devices: Vec<VkDevice>,
    pub errors: Vec<String>,
}

#[derive(Serialize, Default)]
pub struct VkDevice {
    pub name: String,
    pub device_type: String,
    pub api_version: String,
    pub driver_version_raw: u32,
    pub driver_name: Option<String>,
    pub driver_info: Option<String>,
    pub max_image_dimension_2d: u32,
    pub device_local_heap_mib: u64,
    /// Extensions from [`INTERESTING`] that the device offers.
    pub notable_extensions: Vec<String>,
    pub extension_count: usize,
    pub multiview: Option<bool>,
    pub sampler_ycbcr_conversion: Option<bool>,
    pub queue_families: Vec<QueueFamily>,
    pub video_decode: Vec<VideoCaps>,
    pub format_modifiers: Vec<FormatModifiers>,
}

#[derive(Serialize)]
pub struct QueueFamily {
    pub index: u32,
    pub flags: String,
    pub count: u32,
    pub video_codecs: Option<String>,
}

#[derive(Serialize)]
pub struct VideoCaps {
    pub profile: String,
    pub supported: bool,
    pub max_coded_extent: Option<(u32, u32)>,
    pub max_dpb_slots: Option<u32>,
    pub decode_flags: Option<String>,
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct FormatModifiers {
    pub format: String,
    pub optimal_features: String,
    /// (modifier, plane count, sampled-image capable)
    pub modifiers: Vec<(String, u32, bool)>,
}

const INTERESTING: &[&str] = &[
    "VK_KHR_video_queue",
    "VK_KHR_video_decode_queue",
    "VK_KHR_video_decode_h264",
    "VK_KHR_video_decode_h265",
    "VK_KHR_video_decode_av1",
    "VK_KHR_video_decode_vp9",
    "VK_KHR_external_memory_fd",
    "VK_EXT_external_memory_dma_buf",
    "VK_EXT_image_drm_format_modifier",
    "VK_EXT_queue_family_foreign",
    "VK_KHR_external_semaphore_fd",
    "VK_KHR_sampler_ycbcr_conversion",
    "VK_EXT_ycbcr_2plane_444_formats",
    "VK_KHR_multiview",
    "VK_EXT_fragment_density_map",
    "VK_EXT_fragment_density_map2",
    "VK_QCOM_fragment_density_map_offset",
    "VK_KHR_fragment_shading_rate",
    "VK_EXT_hdr_metadata",
    "VK_EXT_swapchain_colorspace",
    "VK_KHR_timeline_semaphore",
    "VK_KHR_synchronization2",
    "VK_KHR_dynamic_rendering",
    "VK_EXT_memory_budget",
];

pub fn probe() -> VkReport {
    let mut r = VkReport::default();
    let entry = match unsafe { ash::Entry::load() } {
        Ok(e) => e,
        Err(e) => {
            r.errors.push(format!("loading libvulkan.so.1: {e}"));
            return r;
        }
    };
    let loader_api = unsafe { entry.try_enumerate_instance_version() }
        .ok()
        .flatten()
        .unwrap_or(vk::API_VERSION_1_0);
    r.loader_api_version = Some(version(loader_api));
    if let Ok(exts) = unsafe { entry.enumerate_instance_extension_properties(None) } {
        r.instance_extensions = exts
            .iter()
            .map(|e| name(&e.extension_name_as_c_str()))
            .collect();
        r.instance_extensions.sort();
    }

    let api = loader_api.min(vk::API_VERSION_1_3);
    let app_name = CString::new("frameplayer-probe").unwrap();
    let app = vk::ApplicationInfo::default()
        .application_name(&app_name)
        .api_version(api);
    let ici = vk::InstanceCreateInfo::default().application_info(&app);
    let instance = match unsafe { entry.create_instance(&ici, None) } {
        Ok(i) => i,
        Err(e) => {
            r.errors.push(format!("vkCreateInstance: {e}"));
            return r;
        }
    };
    match unsafe { instance.enumerate_physical_devices() } {
        Ok(pds) => {
            if pds.is_empty() {
                r.errors
                    .push("no Vulkan physical devices (no ICD installed?)".into());
            }
            for pd in pds {
                r.devices.push(probe_device(&entry, &instance, pd));
            }
        }
        Err(e) => r.errors.push(format!("vkEnumeratePhysicalDevices: {e}")),
    }
    unsafe { instance.destroy_instance(None) };
    r
}

fn name(c: &Result<&CStr, std::ffi::FromBytesUntilNulError>) -> String {
    c.map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub fn version(v: u32) -> String {
    format!(
        "{}.{}.{}",
        vk::api_version_major(v),
        vk::api_version_minor(v),
        vk::api_version_patch(v)
    )
}

fn probe_device(entry: &ash::Entry, instance: &ash::Instance, pd: vk::PhysicalDevice) -> VkDevice {
    let mut d = VkDevice::default();
    let props = unsafe { instance.get_physical_device_properties(pd) };
    d.name = name(&props.device_name_as_c_str());
    d.device_type = format!("{:?}", props.device_type);
    d.api_version = version(props.api_version);
    d.driver_version_raw = props.driver_version;
    d.max_image_dimension_2d = props.limits.max_image_dimension2_d;
    let v12 = props.api_version >= vk::API_VERSION_1_2;

    if v12 {
        let mut driver = vk::PhysicalDeviceDriverProperties::default();
        let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut driver);
        unsafe { instance.get_physical_device_properties2(pd, &mut p2) };
        d.driver_name = Some(name(&driver.driver_name_as_c_str()));
        d.driver_info = Some(name(&driver.driver_info_as_c_str()));

        let mut f11 = vk::PhysicalDeviceVulkan11Features::default();
        let mut f2 = vk::PhysicalDeviceFeatures2::default().push_next(&mut f11);
        unsafe { instance.get_physical_device_features2(pd, &mut f2) };
        d.multiview = Some(f11.multiview == vk::TRUE);
        d.sampler_ycbcr_conversion = Some(f11.sampler_ycbcr_conversion == vk::TRUE);
    }

    let mem = unsafe { instance.get_physical_device_memory_properties(pd) };
    d.device_local_heap_mib = mem.memory_heaps[..mem.memory_heap_count as usize]
        .iter()
        .filter(|h| h.flags.contains(vk::MemoryHeapFlags::DEVICE_LOCAL))
        .map(|h| h.size / (1 << 20))
        .sum();

    let exts: Vec<String> = unsafe { instance.enumerate_device_extension_properties(pd) }
        .map(|v| {
            v.iter()
                .map(|e| name(&e.extension_name_as_c_str()))
                .collect()
        })
        .unwrap_or_default();
    d.extension_count = exts.len();
    d.notable_extensions = INTERESTING
        .iter()
        .filter(|n| exts.iter().any(|e| e == *n))
        .map(|s| s.to_string())
        .collect();
    let has = |n: &str| exts.iter().any(|e| e == n);

    // Queue families, with the codecs each video queue handles.
    let video_queue = has("VK_KHR_video_queue");
    let count = unsafe { instance.get_physical_device_queue_family_properties2_len(pd) };
    let mut video_props = vec![vk::QueueFamilyVideoPropertiesKHR::default(); count];
    let mut qf: Vec<vk::QueueFamilyProperties2> = video_props
        .iter_mut()
        .map(|vp| {
            let q = vk::QueueFamilyProperties2::default();
            if video_queue && v12 {
                q.push_next(vp)
            } else {
                q
            }
        })
        .collect();
    unsafe { instance.get_physical_device_queue_family_properties2(pd, &mut qf) };
    for (i, q) in qf.iter().enumerate() {
        let p = q.queue_family_properties;
        d.queue_families.push(QueueFamily {
            index: i as u32,
            flags: format!("{:?}", p.queue_flags),
            count: p.queue_count,
            video_codecs: None,
        });
    }
    drop(qf);
    for (i, vp) in video_props.iter().enumerate() {
        if !vp.video_codec_operations.is_empty() {
            d.queue_families[i].video_codecs = Some(format!("{:?}", vp.video_codec_operations));
        }
    }

    if video_queue && has("VK_KHR_video_decode_queue") {
        d.video_decode = video_caps(entry, instance, pd, &has);
    }
    if v12 {
        d.format_modifiers = [
            vk::Format::G8_B8R8_2PLANE_420_UNORM,                  // NV12
            vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16, // P010
            vk::Format::R8G8B8A8_SRGB,
        ]
        .iter()
        .map(|&f| format_modifiers(instance, pd, f))
        .collect();
    }
    d
}

fn video_caps(
    entry: &ash::Entry,
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    has: &dyn Fn(&str) -> bool,
) -> Vec<VideoCaps> {
    let vq = ash::khr::video_queue::Instance::new(entry, instance);
    let mut out = Vec::new();
    let b8 = vk::VideoComponentBitDepthFlagsKHR::TYPE_8;
    let b10 = vk::VideoComponentBitDepthFlagsKHR::TYPE_10;

    let mut query = |label: &str,
                     op: vk::VideoCodecOperationFlagsKHR,
                     depth,
                     codec_next: *const std::ffi::c_void| {
        let mut profile = vk::VideoProfileInfoKHR::default()
            .video_codec_operation(op)
            .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
            .luma_bit_depth(depth)
            .chroma_bit_depth(depth);
        profile.p_next = codec_next;
        // Codec-specific capability structs are required in the chain.
        let mut h264c = vk::VideoDecodeH264CapabilitiesKHR::default();
        let mut h265c = vk::VideoDecodeH265CapabilitiesKHR::default();
        let mut av1c = vk::VideoDecodeAV1CapabilitiesKHR::default();
        let codec_caps: *mut std::ffi::c_void =
            if op == vk::VideoCodecOperationFlagsKHR::DECODE_H264 {
                &mut h264c as *mut _ as *mut _
            } else if op == vk::VideoCodecOperationFlagsKHR::DECODE_H265 {
                &mut h265c as *mut _ as *mut _
            } else {
                &mut av1c as *mut _ as *mut _
            };
        let mut dec = vk::VideoDecodeCapabilitiesKHR {
            p_next: codec_caps,
            ..Default::default()
        };
        let mut caps = vk::VideoCapabilitiesKHR {
            p_next: &mut dec as *mut _ as *mut _,
            ..Default::default()
        };
        let res = unsafe {
            (vq.fp().get_physical_device_video_capabilities_khr)(pd, &profile, &mut caps)
        };
        out.push(if res == vk::Result::SUCCESS {
            VideoCaps {
                profile: label.into(),
                supported: true,
                max_coded_extent: Some((caps.max_coded_extent.width, caps.max_coded_extent.height)),
                max_dpb_slots: Some(caps.max_dpb_slots),
                decode_flags: Some(format!("{:?}", dec.flags)),
                error: None,
            }
        } else {
            VideoCaps {
                profile: label.into(),
                supported: false,
                max_coded_extent: None,
                max_dpb_slots: None,
                decode_flags: None,
                error: Some(format!("{res:?}")),
            }
        });
    };

    if has("VK_KHR_video_decode_h264") {
        let h = vk::VideoDecodeH264ProfileInfoKHR::default()
            .std_profile_idc(vk::native::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH)
            .picture_layout(vk::VideoDecodeH264PictureLayoutFlagsKHR::PROGRESSIVE);
        query(
            "H.264 High 8-bit",
            vk::VideoCodecOperationFlagsKHR::DECODE_H264,
            b8,
            &h as *const _ as *const _,
        );
    }
    if has("VK_KHR_video_decode_h265") {
        let main = vk::VideoDecodeH265ProfileInfoKHR::default()
            .std_profile_idc(vk::native::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN);
        query(
            "HEVC Main 8-bit",
            vk::VideoCodecOperationFlagsKHR::DECODE_H265,
            b8,
            &main as *const _ as *const _,
        );
        let main10 = vk::VideoDecodeH265ProfileInfoKHR::default()
            .std_profile_idc(vk::native::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN_10);
        query(
            "HEVC Main10 10-bit",
            vk::VideoCodecOperationFlagsKHR::DECODE_H265,
            b10,
            &main10 as *const _ as *const _,
        );
    }
    if has("VK_KHR_video_decode_av1") {
        let av1 = vk::VideoDecodeAV1ProfileInfoKHR::default()
            .std_profile(vk::native::StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_MAIN);
        query(
            "AV1 Main 8-bit",
            vk::VideoCodecOperationFlagsKHR::DECODE_AV1,
            b8,
            &av1 as *const _ as *const _,
        );
        query(
            "AV1 Main 10-bit",
            vk::VideoCodecOperationFlagsKHR::DECODE_AV1,
            b10,
            &av1 as *const _ as *const _,
        );
    }
    out
}

fn format_modifiers(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    format: vk::Format,
) -> FormatModifiers {
    // Two-call pattern: first the count, then the list.
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
    let mut fp2 = vk::FormatProperties2::default().push_next(&mut list);
    unsafe { instance.get_physical_device_format_properties2(pd, format, &mut fp2) };
    let optimal = fp2.format_properties.optimal_tiling_features;
    let n = list.drm_format_modifier_count as usize;
    let mut mods = vec![vk::DrmFormatModifierPropertiesEXT::default(); n];
    if n > 0 {
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
            .drm_format_modifier_properties(&mut mods);
        let mut fp2 = vk::FormatProperties2::default().push_next(&mut list);
        unsafe { instance.get_physical_device_format_properties2(pd, format, &mut fp2) };
    }
    FormatModifiers {
        format: format!("{format:?}"),
        optimal_features: format!("{optimal:?}"),
        modifiers: mods
            .iter()
            .map(|m| {
                (
                    format!("{:#018x}", m.drm_format_modifier),
                    m.drm_format_modifier_plane_count,
                    m.drm_format_modifier_tiling_features
                        .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE),
                )
            })
            .collect(),
    }
}
