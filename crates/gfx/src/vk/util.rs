//! Small resource helpers: buffers, images, deferred destruction, barriers.

use super::{GpuContext, Result};
use ash::vk;

/// A buffer with its own memory, optionally persistently mapped.
pub(crate) struct Buffer {
    pub buffer: vk::Buffer,
    pub memory: vk::DeviceMemory,
    pub size: u64,
    pub mapped: *mut u8,
}

impl Buffer {
    pub unsafe fn new(
        gpu: &GpuContext,
        size: u64,
        usage: vk::BufferUsageFlags,
        host_visible: bool,
    ) -> Result<Buffer> {
        let d = &gpu.device;
        let info = vk::BufferCreateInfo::default()
            .size(size.max(4))
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = d.create_buffer(&info, None)?;
        let req = d.get_buffer_memory_requirements(buffer);
        let flags = if host_visible {
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT
        } else {
            vk::MemoryPropertyFlags::DEVICE_LOCAL
        };
        let ty = match gpu.memory_type(req.memory_type_bits, flags) {
            Ok(t) => t,
            Err(e) => {
                d.destroy_buffer(buffer, None);
                return Err(e);
            }
        };
        let memory = match d.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(ty),
            None,
        ) {
            Ok(m) => m,
            Err(e) => {
                d.destroy_buffer(buffer, None);
                return Err(e.into());
            }
        };
        d.bind_buffer_memory(buffer, memory, 0)?;
        let mapped = if host_visible {
            d.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())? as *mut u8
        } else {
            std::ptr::null_mut()
        };
        Ok(Buffer {
            buffer,
            memory,
            size,
            mapped,
        })
    }

    /// Copy `bytes` to `offset` of a mapped buffer.
    pub unsafe fn write(&self, offset: usize, bytes: &[u8]) {
        debug_assert!(!self.mapped.is_null());
        debug_assert!(offset + bytes.len() <= self.size as usize);
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.mapped.add(offset), bytes.len());
    }

    pub fn into_garbage(self) -> Garbage {
        Garbage::Buffer(self.buffer, self.memory)
    }
}

/// A 2D image with dedicated memory and a default view.
pub(crate) struct Image {
    pub image: vk::Image,
    pub memory: vk::DeviceMemory,
    pub view: vk::ImageView,
    pub width: u32,
    pub height: u32,
}

impl Image {
    pub unsafe fn new(
        gpu: &GpuContext,
        format: vk::Format,
        width: u32,
        height: u32,
        usage: vk::ImageUsageFlags,
    ) -> Result<Image> {
        let d = &gpu.device;
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
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = d.create_image(&info, None)?;
        let req = d.get_image_memory_requirements(image);
        let alloc = gpu
            .memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
            .and_then(|ty| {
                Ok(d.allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(req.size)
                        .memory_type_index(ty),
                    None,
                )?)
            });
        let memory = match alloc {
            Ok(m) => m,
            Err(e) => {
                d.destroy_image(image, None);
                return Err(e);
            }
        };
        d.bind_image_memory(image, memory, 0)?;
        let view = create_view(gpu, image, format, 0)?;
        Ok(Image {
            image,
            memory,
            view,
            width,
            height,
        })
    }

    pub fn into_garbage(self) -> Garbage {
        Garbage::Image {
            image: self.image,
            memory: self.memory,
            view: self.view,
        }
    }
}

/// 2D colour view of one array layer.
pub(crate) unsafe fn create_view(
    gpu: &GpuContext,
    image: vk::Image,
    format: vk::Format,
    layer: u32,
) -> Result<vk::ImageView> {
    let info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(format)
        .subresource_range(color_range(layer));
    Ok(gpu.device.create_image_view(&info, None)?)
}

pub(crate) fn color_range(layer: u32) -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: layer,
        layer_count: 1,
    }
}

/// Resources whose destruction must wait until the GPU is done with the
/// frame that last used them.
pub(crate) enum Garbage {
    Buffer(vk::Buffer, vk::DeviceMemory),
    Image {
        image: vk::Image,
        memory: vk::DeviceMemory,
        view: vk::ImageView,
    },
    View(vk::ImageView),
    DescriptorSet(vk::DescriptorSet),
}

impl Garbage {
    pub unsafe fn destroy(self, gpu: &GpuContext, pool: vk::DescriptorPool) {
        let d = &gpu.device;
        match self {
            Garbage::Buffer(b, m) => {
                d.destroy_buffer(b, None);
                d.free_memory(m, None);
            }
            Garbage::Image {
                image,
                memory,
                view,
            } => {
                d.destroy_image_view(view, None);
                d.destroy_image(image, None);
                d.free_memory(memory, None);
            }
            Garbage::View(v) => d.destroy_image_view(v, None),
            Garbage::DescriptorSet(s) => {
                let _ = d.free_descriptor_sets(pool, &[s]);
            }
        }
    }
}

/// One image layout transition / queue ownership transfer.
#[derive(Clone, Copy)]
pub(crate) struct Transition {
    pub image: vk::Image,
    pub layer: u32,
    pub old: vk::ImageLayout,
    pub new: vk::ImageLayout,
    pub src_stage: vk::PipelineStageFlags2,
    pub src_access: vk::AccessFlags2,
    pub dst_stage: vk::PipelineStageFlags2,
    pub dst_access: vk::AccessFlags2,
    pub src_queue: u32,
    pub dst_queue: u32,
}

impl Transition {
    pub fn new(image: vk::Image, old: vk::ImageLayout, new: vk::ImageLayout) -> Transition {
        Transition {
            image,
            layer: 0,
            old,
            new,
            src_stage: vk::PipelineStageFlags2::NONE,
            src_access: vk::AccessFlags2::NONE,
            dst_stage: vk::PipelineStageFlags2::NONE,
            dst_access: vk::AccessFlags2::NONE,
            src_queue: vk::QUEUE_FAMILY_IGNORED,
            dst_queue: vk::QUEUE_FAMILY_IGNORED,
        }
    }
    pub fn layer(mut self, layer: u32) -> Self {
        self.layer = layer;
        self
    }
    pub fn src(mut self, stage: vk::PipelineStageFlags2, access: vk::AccessFlags2) -> Self {
        self.src_stage = stage;
        self.src_access = access;
        self
    }
    pub fn dst(mut self, stage: vk::PipelineStageFlags2, access: vk::AccessFlags2) -> Self {
        self.dst_stage = stage;
        self.dst_access = access;
        self
    }
    pub fn queues(mut self, src: u32, dst: u32) -> Self {
        self.src_queue = src;
        self.dst_queue = dst;
        self
    }
}

/// Record up to 4 transitions in one `vkCmdPipelineBarrier2` without allocating.
pub(crate) unsafe fn barrier(gpu: &GpuContext, cmd: vk::CommandBuffer, ts: &[Transition]) {
    debug_assert!(ts.len() <= 4);
    let mut arr = [vk::ImageMemoryBarrier2::default(); 4];
    for (slot, t) in arr.iter_mut().zip(ts) {
        *slot = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(t.src_stage)
            .src_access_mask(t.src_access)
            .dst_stage_mask(t.dst_stage)
            .dst_access_mask(t.dst_access)
            .old_layout(t.old)
            .new_layout(t.new)
            .src_queue_family_index(t.src_queue)
            .dst_queue_family_index(t.dst_queue)
            .image(t.image)
            .subresource_range(color_range(t.layer));
    }
    let dep = vk::DependencyInfo::default().image_memory_barriers(&arr[..ts.len()]);
    gpu.device.cmd_pipeline_barrier2(cmd, &dep);
}

/// Round `v` up to a multiple of `align` (power of two or not).
pub(crate) fn align_up(v: u64, align: u64) -> u64 {
    if align <= 1 {
        v
    } else {
        v.div_ceil(align) * align
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alignment() {
        assert_eq!(align_up(0, 16), 0);
        assert_eq!(align_up(1, 16), 16);
        assert_eq!(align_up(17, 16), 32);
        assert_eq!(align_up(10, 12), 12);
        assert_eq!(align_up(7, 1), 7);
    }
}
