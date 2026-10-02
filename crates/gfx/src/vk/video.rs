//! Video input: DMA-BUF import of decoder buffers, CPU-upload plane images,
//! and the RGBA16F conversion target.
//!
//! Each plane of an NV12/P010 frame becomes its own single-plane image
//! (R8 + RG8, or R16 + RG16 for P010) imported with an explicit DRM format
//! modifier layout. This avoids multi-planar formats and YCbCr samplers
//! entirely (naga emits separate images/samplers, which cannot carry an
//! immutable YCbCr sampler) and works for the linear layouts V4L2 decoders
//! export.
// [verify] Which modifier the Frame's decoder exports. If it is a Qualcomm
// UBWC (compressed) modifier, per-plane import will be refused by
// `supports_modifier`; that case needs a multi-planar import with the
// exporter's full plane list (incl. metadata planes) and plane-aspect views.

use super::util::{create_view, Garbage, Image};
use super::{GfxError, GpuContext, Result};
use crate::color::ColorPush;
use crate::frame::{DmaBufFrame, DmaBufPlane, PixelFormat};
use ash::vk;
use std::os::fd::{BorrowedFd, IntoRawFd};

/// Single-plane formats used for `(luma, chroma)` of a pixel format.
pub(crate) fn plane_formats(f: PixelFormat) -> [vk::Format; 2] {
    match f {
        PixelFormat::Nv12 => [vk::Format::R8_UNORM, vk::Format::R8G8_UNORM],
        PixelFormat::P010 => [vk::Format::R16_UNORM, vk::Format::R16G16_UNORM],
    }
}

/// Bytes per texel of each plane.
pub(crate) fn plane_texel_bytes(f: PixelFormat) -> [usize; 2] {
    let b = f.bytes_per_component();
    [b, 2 * b]
}

pub(crate) struct ImportedPlane {
    pub image: vk::Image,
    pub memory: vk::DeviceMemory,
    pub view: vk::ImageView,
}

impl ImportedPlane {
    pub fn into_garbage(self) -> Garbage {
        Garbage::Image {
            image: self.image,
            memory: self.memory,
            view: self.view,
        }
    }
}

/// A decoder buffer imported once and reused while the decoder recycles it.
pub(crate) struct ImportedFrame {
    pub desc: DmaBufFrame,
    pub planes: [ImportedPlane; 2],
    /// Compute descriptor set (planes + output).
    pub set: vk::DescriptorSet,
    /// False until the first acquire barrier (from PREINITIALIZED).
    pub transitioned: bool,
    pub last_used: u64,
}

impl ImportedFrame {
    /// Whether a new descriptor describes the same memory layout.
    pub fn matches(&self, f: &DmaBufFrame) -> bool {
        let a = &self.desc;
        a.width == f.width
            && a.height == f.height
            && a.format == f.format
            && a.modifier == f.modifier
            && a.planes
                .iter()
                .zip(&f.planes)
                .all(|(p, q)| p.offset == q.offset && p.pitch == q.pitch)
    }

    pub fn into_garbage(self, out: &mut Vec<Garbage>) {
        let [a, b] = self.planes;
        out.push(Garbage::DescriptorSet(self.set));
        out.push(a.into_garbage());
        out.push(b.into_garbage());
    }
}

/// Device-local plane images filled from CPU frames.
pub(crate) struct CpuPlanes {
    pub format: PixelFormat,
    pub width: u32,
    pub height: u32,
    pub planes: [Image; 2],
    pub set: vk::DescriptorSet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    None,
    Cpu,
    DmaBuf(u64),
}

/// Import one plane of a DMA-BUF as a single-plane sampled image.
///
/// # Safety
/// `plane.fd` must be a valid DMA-BUF descriptor for the duration of the call; it
/// is duplicated, and the duplicate's ownership passes to Vulkan on success.
pub(crate) unsafe fn import_plane(
    gpu: &GpuContext,
    plane: DmaBufPlane,
    modifier: u64,
    format: vk::Format,
    width: u32,
    height: u32,
) -> Result<ImportedPlane> {
    let ext_fd = gpu
        .ext_mem_fd
        .as_ref()
        .ok_or_else(|| GfxError::Unsupported("VK_KHR_external_memory_fd".into()))?;
    if !gpu.supports_modifier(format, modifier) {
        return Err(GfxError::Unsupported(format!(
            "format {format:?} with DRM modifier {modifier:#x}"
        )));
    }
    let d = &gpu.device;
    let DmaBufPlane { fd, offset, pitch } = plane;
    let handle = vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT;
    let layouts = [vk::SubresourceLayout {
        offset,
        size: 0,
        row_pitch: pitch,
        array_pitch: 0,
        depth_pitch: 0,
    }];
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
        // Contents written by the decoder must be preserved by the first
        // transition, hence PREINITIALIZED rather than UNDEFINED.
        .initial_layout(vk::ImageLayout::PREINITIALIZED)
        .push_next(&mut external)
        .push_next(&mut explicit);
    let image = d.create_image(&info, None)?;

    let result = (|| -> Result<(vk::DeviceMemory, vk::ImageView)> {
        let req = d.get_image_memory_requirements(image);
        let mut fd_props = vk::MemoryFdPropertiesKHR::default();
        ext_fd.get_memory_fd_properties(handle, fd, &mut fd_props)?;
        let bits = req.memory_type_bits & fd_props.memory_type_bits;
        let ty = gpu
            .memory_type(bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
            .or_else(|_| gpu.memory_type(bits, vk::MemoryPropertyFlags::empty()))?;
        let owned = BorrowedFd::borrow_raw(fd).try_clone_to_owned()?;
        let mut import = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(handle)
            .fd(std::os::fd::AsRawFd::as_raw_fd(&owned));
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(ty)
            .push_next(&mut import)
            .push_next(&mut dedicated);
        let memory = d.allocate_memory(&alloc, None)?;
        // Vulkan now owns the duplicate.
        let _ = owned.into_raw_fd();
        if let Err(e) = d.bind_image_memory(image, memory, 0) {
            d.free_memory(memory, None);
            return Err(e.into());
        }
        match create_view(gpu, image, format, 0) {
            Ok(view) => Ok((memory, view)),
            Err(e) => {
                d.free_memory(memory, None);
                Err(e)
            }
        }
    })();
    match result {
        Ok((memory, view)) => Ok(ImportedPlane {
            image,
            memory,
            view,
        }),
        Err(e) => {
            d.destroy_image(image, None);
            Err(e)
        }
    }
}

/// Import both planes of a frame.
pub(crate) unsafe fn import_frame(gpu: &GpuContext, f: &DmaBufFrame) -> Result<[ImportedPlane; 2]> {
    let formats = plane_formats(f.format);
    let mk = |i: usize| {
        let (w, h) = f.format.plane_extent(i, f.width, f.height);
        import_plane(gpu, f.planes[i], f.modifier, formats[i], w, h)
    };
    let y = mk(0)?;
    match mk(1) {
        Ok(c) => Ok([y, c]),
        Err(e) => {
            let d = &gpu.device;
            d.destroy_image_view(y.view, None);
            d.destroy_image(y.image, None);
            d.free_memory(y.memory, None);
            Err(e)
        }
    }
}

/// Write a compute descriptor set: planes + sampler + output storage image.
pub(crate) unsafe fn write_yuv_set(
    gpu: &GpuContext,
    set: vk::DescriptorSet,
    luma: vk::ImageView,
    chroma: vk::ImageView,
    luma_layout: vk::ImageLayout,
    sampler: vk::Sampler,
    output: vk::ImageView,
) {
    let img = |v| {
        [vk::DescriptorImageInfo {
            sampler: vk::Sampler::null(),
            image_view: v,
            image_layout: luma_layout,
        }]
    };
    let l = img(luma);
    let c = img(chroma);
    let s = [vk::DescriptorImageInfo {
        sampler,
        image_view: vk::ImageView::null(),
        image_layout: vk::ImageLayout::UNDEFINED,
    }];
    let o = [vk::DescriptorImageInfo {
        sampler: vk::Sampler::null(),
        image_view: output,
        image_layout: vk::ImageLayout::GENERAL,
    }];
    fn w<'a>(
        set: vk::DescriptorSet,
        b: u32,
        t: vk::DescriptorType,
        info: &'a [vk::DescriptorImageInfo],
    ) -> vk::WriteDescriptorSet<'a> {
        vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(b)
            .descriptor_type(t)
            .image_info(info)
    }
    let writes = [
        w(set, 0, vk::DescriptorType::SAMPLED_IMAGE, &l),
        w(set, 1, vk::DescriptorType::SAMPLED_IMAGE, &c),
        w(set, 2, vk::DescriptorType::SAMPLER, &s),
        w(set, 3, vk::DescriptorType::STORAGE_IMAGE, &o),
    ];
    gpu.device.update_descriptor_sets(&writes, &[]);
}

/// Write a texture descriptor set (sampled image + sampler).
pub(crate) unsafe fn write_tex_set(
    gpu: &GpuContext,
    set: vk::DescriptorSet,
    view: vk::ImageView,
    sampler: vk::Sampler,
) {
    let i = [vk::DescriptorImageInfo {
        sampler: vk::Sampler::null(),
        image_view: view,
        image_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
    }];
    let s = [vk::DescriptorImageInfo {
        sampler,
        image_view: vk::ImageView::null(),
        image_layout: vk::ImageLayout::UNDEFINED,
    }];
    let writes = [
        vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
            .image_info(&i),
        vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(1)
            .descriptor_type(vk::DescriptorType::SAMPLER)
            .image_info(&s),
    ];
    gpu.device.update_descriptor_sets(&writes, &[]);
}

/// Conversion target and bookkeeping for the current video.
pub(crate) struct VideoState {
    pub output: Option<Image>,
    /// Texture set over `output` for the projection and UI passes.
    pub output_set: vk::DescriptorSet,
    pub source: Source,
    pub format: PixelFormat,
    pub color: crate::color::ColorInfo,
    pub width: u32,
    pub height: u32,
    /// A new frame was uploaded since the last conversion.
    pub dirty: bool,
    /// Parameters of the last conversion; a change re-runs it (live sliders
    /// while paused).
    pub last_push: Option<ColorPush>,
}

impl VideoState {
    pub fn new() -> VideoState {
        VideoState {
            output: None,
            output_set: vk::DescriptorSet::null(),
            source: Source::None,
            format: PixelFormat::Nv12,
            color: crate::color::ColorInfo::SDR_709,
            width: 0,
            height: 0,
            dirty: false,
            last_push: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::ColorInfo;

    #[test]
    fn plane_format_table() {
        assert_eq!(
            plane_formats(PixelFormat::P010)[1],
            vk::Format::R16G16_UNORM
        );
        assert_eq!(plane_texel_bytes(PixelFormat::P010), [2, 4]);
        assert_eq!(plane_texel_bytes(PixelFormat::Nv12), [1, 2]);
    }

    #[test]
    fn import_matching() {
        let desc = DmaBufFrame {
            buffer_id: 1,
            width: 3840,
            height: 2160,
            format: PixelFormat::Nv12,
            modifier: 0,
            planes: [
                DmaBufPlane {
                    fd: 3,
                    offset: 0,
                    pitch: 3840,
                },
                DmaBufPlane {
                    fd: 3,
                    offset: 3840 * 2160,
                    pitch: 3840,
                },
            ],
            color: ColorInfo::SDR_709,
        };
        let plane = || ImportedPlane {
            image: vk::Image::null(),
            memory: vk::DeviceMemory::null(),
            view: vk::ImageView::null(),
        };
        let f = ImportedFrame {
            desc,
            planes: [plane(), plane()],
            set: vk::DescriptorSet::null(),
            transitioned: false,
            last_used: 0,
        };
        let mut other = desc;
        other.planes[0].fd = 9; // fd numbers may differ between dequeues
        assert!(f.matches(&other));
        other.planes[1].offset += 64;
        assert!(!f.matches(&other));
    }
}
