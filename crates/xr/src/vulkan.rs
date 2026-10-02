//! Vulkan bring-up through `XR_KHR_vulkan_enable2`: the runtime creates the
//! instance and device (adding whatever it needs), we add the DMA-BUF import
//! extensions and the 1.3 features fp-gfx relies on.

use crate::select::{select_device_extensions, DeviceExtensions};
use crate::{XrContext, XrError};
use ash::vk::{self, Handle};
use openxr as xr;
use std::ffi::{c_char, CStr};

/// Raw Vulkan handles shared by fp-xr (session binding) and fp-gfx
/// (rendering). Destroys the device and instance on drop, so it must be
/// dropped *after* the XR session and the renderer.
pub struct VulkanContext {
    pub entry: ash::Entry,
    pub instance: ash::Instance,
    pub physical_device: vk::PhysicalDevice,
    pub device: ash::Device,
    pub queue_family_index: u32,
    pub queue_index: u32,
    /// Negotiated instance API version.
    pub api_version: u32,
    pub device_extensions: DeviceExtensions,
    /// `samplerYcbcrConversion` was enabled.
    pub sampler_ycbcr_conversion: bool,
    pub device_name: String,
}

impl VulkanContext {
    /// Create instance + device for the XR system.
    pub fn new(xr_ctx: &XrContext) -> Result<VulkanContext, XrError> {
        let xri = &xr_ctx.instance;
        let system = xr_ctx.system;
        let reqs = xri.graphics_requirements::<xr::Vulkan>(system)?;
        let min = reqs.min_api_version_supported;
        tracing::info!(
            "XR Vulkan requirements: min {}.{} max {}.{}",
            min.major(),
            min.minor(),
            reqs.max_api_version_supported.major(),
            reqs.max_api_version_supported.minor()
        );
        if min.major() > 1 || (min.major() == 1 && min.minor() > 3) {
            return Err(XrError::Unsupported(format!(
                "runtime needs Vulkan {}.{}",
                min.major(),
                min.minor()
            )));
        }
        // fp-gfx uses dynamic rendering + synchronization2 from core 1.3.
        let api_version = vk::make_api_version(0, 1, 3, 0);

        unsafe {
            let entry = ash::Entry::load()
                .map_err(|e| XrError::Vulkan(format!("loading libvulkan: {e}")))?;
            let gipa: xr::sys::platform::VkGetInstanceProcAddr =
                std::mem::transmute(entry.static_fn().get_instance_proc_addr);

            let app_info = vk::ApplicationInfo::default()
                .application_name(c"FramePlayer")
                .application_version(1)
                .engine_name(c"fp-gfx")
                .engine_version(1)
                .api_version(api_version);
            let create_info = vk::InstanceCreateInfo::default().application_info(&app_info);
            let raw_instance = xri
                .create_vulkan_instance(system, gipa, &create_info as *const _ as *const _)?
                .map_err(|r| {
                    XrError::Vulkan(format!(
                        "xrCreateVulkanInstanceKHR: {:?}",
                        vk::Result::from_raw(r)
                    ))
                })?;
            let instance = ash::Instance::load(
                entry.static_fn(),
                vk::Instance::from_raw(raw_instance as u64),
            );

            let raw_pd = match xri.vulkan_graphics_device(system, raw_instance) {
                Ok(p) => p,
                Err(e) => {
                    instance.destroy_instance(None);
                    return Err(e.into());
                }
            };
            let physical_device = vk::PhysicalDevice::from_raw(raw_pd as u64);
            match Self::create_device(xri, system, gipa, &instance, physical_device, raw_pd) {
                Ok((device, queue_family_index, device_extensions, ycbcr, name)) => {
                    Ok(VulkanContext {
                        entry,
                        instance,
                        physical_device,
                        device,
                        queue_family_index,
                        queue_index: 0,
                        api_version,
                        device_extensions,
                        sampler_ycbcr_conversion: ycbcr,
                        device_name: name,
                    })
                }
                Err(e) => {
                    instance.destroy_instance(None);
                    Err(e)
                }
            }
        }
    }

    #[allow(clippy::type_complexity)]
    unsafe fn create_device(
        xri: &xr::Instance,
        system: xr::SystemId,
        gipa: xr::sys::platform::VkGetInstanceProcAddr,
        instance: &ash::Instance,
        pd: vk::PhysicalDevice,
        raw_pd: xr::sys::platform::VkPhysicalDevice,
    ) -> Result<(ash::Device, u32, DeviceExtensions, bool, String), XrError> {
        let props = instance.get_physical_device_properties(pd);
        let name = CStr::from_ptr(props.device_name.as_ptr())
            .to_string_lossy()
            .into_owned();
        let dev_api = props.api_version;
        tracing::info!(
            "Vulkan device: {name} (API {}.{}.{}, driver {:#x})",
            vk::api_version_major(dev_api),
            vk::api_version_minor(dev_api),
            vk::api_version_patch(dev_api),
            props.driver_version
        );
        if vk::api_version_minor(dev_api) < 3 && vk::api_version_major(dev_api) == 1 {
            return Err(XrError::Unsupported(format!(
                "{name} only supports Vulkan 1.{}",
                vk::api_version_minor(dev_api)
            )));
        }

        let families = instance.get_physical_device_queue_family_properties(pd);
        let qf = families
            .iter()
            .position(|f| {
                f.queue_flags
                    .contains(vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE)
            })
            .ok_or_else(|| XrError::Unsupported("no graphics+compute queue".into()))?
            as u32;

        let available = instance.enumerate_device_extension_properties(pd)?;
        let avail_names: Vec<&CStr> = available
            .iter()
            .map(|e| CStr::from_ptr(e.extension_name.as_ptr()))
            .collect();
        let (ext_names, exts) = select_device_extensions(&avail_names);
        tracing::info!("Vulkan zero-copy extensions: {exts:?}");
        let ext_ptrs: Vec<*const c_char> = ext_names.iter().map(|n| n.as_ptr()).collect();

        // Query and enable features.
        let mut f11 = vk::PhysicalDeviceVulkan11Features::default();
        let mut f13 = vk::PhysicalDeviceVulkan13Features::default();
        let mut f2 = vk::PhysicalDeviceFeatures2::default()
            .push_next(&mut f11)
            .push_next(&mut f13);
        instance.get_physical_device_features2(pd, &mut f2);
        if f13.dynamic_rendering == 0 || f13.synchronization2 == 0 {
            return Err(XrError::Unsupported(
                "dynamicRendering / synchronization2 not supported".into(),
            ));
        }
        let ycbcr = f11.sampler_ycbcr_conversion != 0;
        let mut en11 =
            vk::PhysicalDeviceVulkan11Features::default().sampler_ycbcr_conversion(ycbcr);
        let mut en13 = vk::PhysicalDeviceVulkan13Features::default()
            .dynamic_rendering(true)
            .synchronization2(true);

        let priorities = [1.0f32];
        let queues = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(qf)
            .queue_priorities(&priorities)];
        let dci = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queues)
            .enabled_extension_names(&ext_ptrs)
            .push_next(&mut en11)
            .push_next(&mut en13);
        let raw_device = xri
            .create_vulkan_device(system, gipa, raw_pd, &dci as *const _ as *const _)?
            .map_err(|r| {
                XrError::Vulkan(format!(
                    "xrCreateVulkanDeviceKHR: {:?}",
                    vk::Result::from_raw(r)
                ))
            })?;
        let device = ash::Device::load(instance.fp_v1_0(), vk::Device::from_raw(raw_device as u64));
        Ok((device, qf, exts, ycbcr, name))
    }

    /// Session binding info for `xrCreateSession`.
    pub fn session_create_info(&self) -> xr::vulkan::SessionCreateInfo {
        xr::vulkan::SessionCreateInfo {
            instance: self.instance.handle().as_raw() as _,
            physical_device: self.physical_device.as_raw() as _,
            device: self.device.handle().as_raw() as _,
            queue_family_index: self.queue_family_index,
            queue_index: self.queue_index,
        }
    }
}

impl Drop for VulkanContext {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}
