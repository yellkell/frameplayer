//! Device/queue wrapper built from raw handles that fp-xr created through
//! `XR_KHR_vulkan_enable2`. fp-gfx never destroys the device or instance;
//! their owner (fp-xr's `VulkanContext`) outlives the renderer.

use super::{GfxError, Result};
use ash::vk;
use std::collections::HashMap;
use std::sync::Mutex;

/// Optional device features the renderer can use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeviceCaps {
    /// `VK_KHR_external_memory_fd` + `VK_EXT_external_memory_dma_buf`.
    pub dmabuf_import: bool,
    /// `VK_EXT_image_drm_format_modifier`.
    pub drm_format_modifier: bool,
    /// `VK_EXT_queue_family_foreign`.
    pub queue_family_foreign: bool,
    /// `samplerYcbcrConversion` feature enabled (core 1.1).
    pub sampler_ycbcr_conversion: bool,
}

impl DeviceCaps {
    /// Zero-copy DMA-BUF import needs all three extensions.
    pub fn zero_copy(&self) -> bool {
        self.dmabuf_import && self.drm_format_modifier
    }
}

/// Shared Vulkan handles plus the extension loaders the renderer needs.
pub struct GpuContext {
    pub instance: ash::Instance,
    pub physical_device: vk::PhysicalDevice,
    pub device: ash::Device,
    pub queue: vk::Queue,
    pub queue_family_index: u32,
    pub caps: DeviceCaps,
    pub(crate) mem_props: vk::PhysicalDeviceMemoryProperties,
    pub(crate) limits: vk::PhysicalDeviceLimits,
    pub(crate) ext_mem_fd: Option<ash::khr::external_memory_fd::Device>,
    modifier_support: Mutex<HashMap<(vk::Format, u64), bool>>,
}

impl GpuContext {
    /// Wrap existing handles.
    ///
    /// # Safety
    /// All handles must be valid, `device` must have been created from
    /// `physical_device` with the 1.3 features `dynamicRendering` and
    /// `synchronization2` enabled and with the extensions `caps` claims, and
    /// they must outlive the returned context and everything built from it.
    pub unsafe fn new(
        instance: ash::Instance,
        physical_device: vk::PhysicalDevice,
        device: ash::Device,
        queue_family_index: u32,
        queue_index: u32,
        caps: DeviceCaps,
    ) -> GpuContext {
        let queue = device.get_device_queue(queue_family_index, queue_index);
        let mem_props = instance.get_physical_device_memory_properties(physical_device);
        let limits = instance
            .get_physical_device_properties(physical_device)
            .limits;
        let ext_mem_fd = caps
            .dmabuf_import
            .then(|| ash::khr::external_memory_fd::Device::new(&instance, &device));
        GpuContext {
            instance,
            physical_device,
            device,
            queue,
            queue_family_index,
            caps,
            mem_props,
            limits,
            ext_mem_fd,
            modifier_support: Mutex::new(HashMap::new()),
        }
    }

    /// Index of a memory type allowed by `type_bits` with `flags`.
    pub(crate) fn memory_type(
        &self,
        type_bits: u32,
        flags: vk::MemoryPropertyFlags,
    ) -> Result<u32> {
        find_memory_type(&self.mem_props, type_bits, flags).ok_or(GfxError::NoMemoryType)
    }

    /// Whether `format` can be sampled with DRM modifier `modifier`
    /// (queried once per pair, then cached).
    pub(crate) fn supports_modifier(&self, format: vk::Format, modifier: u64) -> bool {
        if !self.caps.drm_format_modifier {
            return false;
        }
        let mut cache = self
            .modifier_support
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *cache
            .entry((format, modifier))
            .or_insert_with(|| unsafe { self.query_modifier(format, modifier) })
    }

    unsafe fn query_modifier(&self, format: vk::Format, modifier: u64) -> bool {
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        let mut props = vk::FormatProperties2::default().push_next(&mut list);
        self.instance.get_physical_device_format_properties2(
            self.physical_device,
            format,
            &mut props,
        );
        let count = list.drm_format_modifier_count as usize;
        if count == 0 {
            return false;
        }
        let mut mods = vec![vk::DrmFormatModifierPropertiesEXT::default(); count];
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
            .drm_format_modifier_properties(&mut mods);
        let mut props = vk::FormatProperties2::default().push_next(&mut list);
        self.instance.get_physical_device_format_properties2(
            self.physical_device,
            format,
            &mut props,
        );
        mods.iter().any(|m| {
            m.drm_format_modifier == modifier
                && m.drm_format_modifier_tiling_features
                    .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE)
        })
    }
}

/// Pure helper behind [`GpuContext::memory_type`].
pub(crate) fn find_memory_type(
    props: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
    flags: vk::MemoryPropertyFlags,
) -> Option<u32> {
    (0..props.memory_type_count).find(|&i| {
        type_bits & (1 << i) != 0
            && props.memory_types[i as usize]
                .property_flags
                .contains(flags)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_type_selection() {
        let mut props = vk::PhysicalDeviceMemoryProperties {
            memory_type_count: 3,
            ..Default::default()
        };
        props.memory_types[0].property_flags = vk::MemoryPropertyFlags::DEVICE_LOCAL;
        props.memory_types[1].property_flags =
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        props.memory_types[2].property_flags =
            vk::MemoryPropertyFlags::DEVICE_LOCAL | vk::MemoryPropertyFlags::HOST_VISIBLE;
        let hv = vk::MemoryPropertyFlags::HOST_VISIBLE;
        assert_eq!(find_memory_type(&props, 0b111, hv), Some(1));
        assert_eq!(find_memory_type(&props, 0b101, hv), Some(2));
        assert_eq!(find_memory_type(&props, 0b001, hv), None);
        assert_eq!(
            find_memory_type(&props, 0b111, vk::MemoryPropertyFlags::DEVICE_LOCAL),
            Some(0)
        );
        assert!(DeviceCaps {
            dmabuf_import: true,
            drm_format_modifier: true,
            ..Default::default()
        }
        .zero_copy());
    }
}
