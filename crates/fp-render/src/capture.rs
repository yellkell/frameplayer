//! Offscreen eye targets with read-back: for tests, screenshots and the
//! desktop preview.

use crate::Result;
use crate::mem::{self, Buffer, Image};
use crate::renderer::{EyeTarget, Renderer};
use ash::vk;
use gpu_allocator::MemoryLocation;

pub struct OffscreenEyes {
    pub images: [Image; 2],
    readback: Buffer,
}

impl OffscreenEyes {
    pub fn new(r: &Renderer, width: u32, height: u32) -> Result<OffscreenEyes> {
        let gpu = r.gpu();
        let mk = || {
            Image::new(
                gpu,
                width,
                height,
                &[r.color_format()],
                vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC,
                1,
                "offscreen eye",
            )
        };
        let images = [mk()?, mk()?];
        let readback = Buffer::new(
            gpu,
            width as u64 * height as u64 * 4,
            vk::BufferUsageFlags::TRANSFER_DST,
            MemoryLocation::GpuToCpu,
            "readback",
        )?;
        Ok(OffscreenEyes { images, readback })
    }

    pub fn targets(&self) -> [EyeTarget; 2] {
        let t = |i: &Image| EyeTarget {
            image: i.image,
            view: i.views[0],
            extent: i.extent,
            final_layout: vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        };
        [t(&self.images[0]), t(&self.images[1])]
    }

    /// Reads an eye back as tightly packed RGBA8 (sRGB-encoded). Call after
    /// the frame that rendered it has been submitted; waits for the GPU.
    pub fn read(&mut self, r: &Renderer, eye: usize) -> Result<Vec<u8>> {
        r.wait_idle();
        let img = &self.images[eye];
        let (image, extent, buffer) = (img.image, img.extent, self.readback.buffer);
        r.one_shot(|gpu, cmd| {
            let region = vk::BufferImageCopy::default()
                .image_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .image_extent(vk::Extent3D {
                    width: extent.width,
                    height: extent.height,
                    depth: 1,
                });
            // SAFETY: image is in TRANSFER_SRC after the frame; buffer sized.
            unsafe {
                gpu.device.cmd_copy_image_to_buffer(
                    cmd,
                    image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    buffer,
                    &[region],
                )
            };
            let _ = mem::range(0, 1);
        })?;
        let n = (extent.width * extent.height * 4) as usize;
        Ok(self.readback.bytes()[..n].to_vec())
    }

    pub fn destroy(&mut self, r: &Renderer) {
        r.wait_idle();
        for i in &mut self.images {
            i.destroy(r.gpu());
        }
        self.readback.destroy(r.gpu());
    }
}
