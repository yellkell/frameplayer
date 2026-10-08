//! Buffers, images and barriers.

use crate::gpu::Gpu;
use crate::{Error, Result, VkContext};
use ash::vk;
use gpu_allocator::MemoryLocation;
use gpu_allocator::vulkan::{Allocation, AllocationCreateDesc, AllocationScheme};

pub struct Buffer {
    pub buffer: vk::Buffer,
    pub size: u64,
    alloc: Option<Allocation>,
}

impl Buffer {
    pub fn new(
        gpu: &Gpu,
        size: u64,
        usage: vk::BufferUsageFlags,
        location: MemoryLocation,
        name: &str,
    ) -> Result<Buffer> {
        let size = size.max(16);
        // SAFETY: valid create info.
        let buffer = unsafe {
            gpu.device.create_buffer(
                &vk::BufferCreateInfo::default().size(size).usage(usage),
                None,
            )
        }
        .ctx("create buffer")?;
        // SAFETY: valid buffer.
        let requirements = unsafe { gpu.device.get_buffer_memory_requirements(buffer) };
        let alloc = gpu
            .alloc()
            .allocate(&AllocationCreateDesc {
                name,
                requirements,
                location,
                linear: true,
                allocation_scheme: AllocationScheme::GpuAllocatorManaged,
            })
            .map_err(|e| Error::Alloc(e.to_string()))?;
        // SAFETY: memory from the allocator matches the requirements.
        unsafe {
            gpu.device
                .bind_buffer_memory(buffer, alloc.memory(), alloc.offset())
        }
        .ctx("bind buffer")?;
        Ok(Buffer {
            buffer,
            size,
            alloc: Some(alloc),
        })
    }

    /// The mapped bytes of a host-visible buffer.
    pub fn bytes(&mut self) -> &mut [u8] {
        match self.alloc.as_mut().and_then(|a| a.mapped_slice_mut()) {
            Some(s) => s,
            None => &mut [],
        }
    }

    pub fn destroy(&mut self, gpu: &Gpu) {
        if let Some(a) = self.alloc.take() {
            let _ = gpu.alloc().free(a);
        }
        // SAFETY: the buffer is no longer in use (callers wait for fences).
        unsafe { gpu.device.destroy_buffer(self.buffer, None) };
        self.buffer = vk::Buffer::null();
    }
}

pub struct Image {
    pub image: vk::Image,
    pub format: vk::Format,
    pub extent: vk::Extent2D,
    pub mips: u32,
    /// One view per requested format, all mips.
    pub views: Vec<vk::ImageView>,
    /// Single-mip view of level 0 in the first format (render target).
    pub level0: vk::ImageView,
    alloc: Option<Allocation>,
}

impl Image {
    /// Creates a 2D image. `view_formats[0]` is the image format; more formats
    /// make the image mutable-format (e.g. UNORM rendering, sRGB sampling).
    pub fn new(
        gpu: &Gpu,
        width: u32,
        height: u32,
        view_formats: &[vk::Format],
        usage: vk::ImageUsageFlags,
        mips: u32,
        name: &str,
    ) -> Result<Image> {
        let format = view_formats[0];
        let mut format_list = vk::ImageFormatListCreateInfo::default().view_formats(view_formats);
        let mut info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: width.max(1),
                height: height.max(1),
                depth: 1,
            })
            .mip_levels(mips.max(1))
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(usage)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        if view_formats.len() > 1 {
            info = info
                .flags(vk::ImageCreateFlags::MUTABLE_FORMAT)
                .push_next(&mut format_list);
        }
        // SAFETY: valid create info.
        let image = unsafe { gpu.device.create_image(&info, None) }.ctx("create image")?;
        // SAFETY: valid image.
        let requirements = unsafe { gpu.device.get_image_memory_requirements(image) };
        let alloc = gpu
            .alloc()
            .allocate(&AllocationCreateDesc {
                name,
                requirements,
                location: MemoryLocation::GpuOnly,
                linear: false,
                allocation_scheme: AllocationScheme::GpuAllocatorManaged,
            })
            .map_err(|e| Error::Alloc(e.to_string()))?;
        // SAFETY: memory matches the requirements.
        unsafe {
            gpu.device
                .bind_image_memory(image, alloc.memory(), alloc.offset())
        }
        .ctx("bind image")?;
        let mut views = Vec::new();
        for &f in view_formats {
            views.push(view(gpu, image, f, 0, mips.max(1))?);
        }
        let level0 = view(gpu, image, format, 0, 1)?;
        Ok(Image {
            image,
            format,
            extent: vk::Extent2D {
                width: width.max(1),
                height: height.max(1),
            },
            mips: mips.max(1),
            views,
            level0,
            alloc: Some(alloc),
        })
    }

    pub fn destroy(&mut self, gpu: &Gpu) {
        // SAFETY: not in use (callers defer destruction past in-flight frames).
        unsafe {
            for v in self.views.drain(..) {
                gpu.device.destroy_image_view(v, None);
            }
            gpu.device.destroy_image_view(self.level0, None);
            gpu.device.destroy_image(self.image, None);
        }
        if let Some(a) = self.alloc.take() {
            let _ = gpu.alloc().free(a);
        }
    }
}

pub fn view(
    gpu: &Gpu,
    image: vk::Image,
    format: vk::Format,
    base_mip: u32,
    mips: u32,
) -> Result<vk::ImageView> {
    // SAFETY: valid image and format compatible with it.
    unsafe {
        gpu.device.create_image_view(
            &vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(format)
                .subresource_range(range(base_mip, mips)),
            None,
        )
    }
    .ctx("create image view")
}

pub fn range(base_mip: u32, mips: u32) -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: base_mip,
        level_count: mips,
        base_array_layer: 0,
        layer_count: 1,
    }
}

/// One image layout transition (synchronization2).
#[allow(clippy::too_many_arguments)]
pub fn barrier(
    gpu: &Gpu,
    cmd: vk::CommandBuffer,
    image: vk::Image,
    sub: vk::ImageSubresourceRange,
    old: vk::ImageLayout,
    new: vk::ImageLayout,
    src: (vk::PipelineStageFlags2, vk::AccessFlags2),
    dst: (vk::PipelineStageFlags2, vk::AccessFlags2),
) {
    let b = [vk::ImageMemoryBarrier2::default()
        .image(image)
        .subresource_range(sub)
        .old_layout(old)
        .new_layout(new)
        .src_stage_mask(src.0)
        .src_access_mask(src.1)
        .dst_stage_mask(dst.0)
        .dst_access_mask(dst.1)];
    // SAFETY: recording into a command buffer in the recording state.
    unsafe {
        gpu.device.cmd_pipeline_barrier2(
            cmd,
            &vk::DependencyInfo::default().image_memory_barriers(&b),
        )
    };
}

pub const TOP: (vk::PipelineStageFlags2, vk::AccessFlags2) =
    (vk::PipelineStageFlags2::TOP_OF_PIPE, vk::AccessFlags2::NONE);
pub const TRANSFER_WRITE: (vk::PipelineStageFlags2, vk::AccessFlags2) = (
    vk::PipelineStageFlags2::TRANSFER,
    vk::AccessFlags2::TRANSFER_WRITE,
);
pub const TRANSFER_READ: (vk::PipelineStageFlags2, vk::AccessFlags2) = (
    vk::PipelineStageFlags2::TRANSFER,
    vk::AccessFlags2::TRANSFER_READ,
);
pub const SHADER_READ: (vk::PipelineStageFlags2, vk::AccessFlags2) = (
    vk::PipelineStageFlags2::FRAGMENT_SHADER,
    vk::AccessFlags2::SHADER_SAMPLED_READ,
);
pub const COLOR_WRITE: (vk::PipelineStageFlags2, vk::AccessFlags2) = (
    vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
    vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
);
