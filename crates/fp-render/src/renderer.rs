//! Frame recording: video upload, egui panels, and the two eye passes.
//!
//! Two frame slots are in flight. Per-slot resources (command buffer,
//! uniform buffer, video planes, vertex buffers) are reused only after the
//! slot's fence signals. Images freed during a frame go to that slot's
//! garbage list and are destroyed when the slot comes round again, by which
//! time no submitted frame can still read them.

use crate::gpu::Gpu;
use crate::mem::{self, Buffer, Image};
use crate::params::{self, SceneParams, VideoParams};
use crate::pipeline::{self, PipelineDesc};
use crate::video::VideoPlanes;
use crate::{Result, VkContext};
use ash::vk;
use egui::epaint::{ClippedPrimitive, Primitive, TextureId, textures::TexturesDelta};
use fp_media::VideoFrame;
use glam::{Mat4, Vec2, Vec3};
use gpu_allocator::MemoryLocation;
use std::collections::HashMap;
use std::sync::Arc;

const SLOTS: usize = 2;
/// Linear-light background format of eye targets.
pub const PANEL_FORMATS: [vk::Format; 2] = [vk::Format::R8G8B8A8_UNORM, vk::Format::R8G8B8A8_SRGB];

/// Camera for one eye.
#[derive(Clone, Copy, Debug)]
pub struct EyeView {
    /// World → eye.
    pub view: Mat4,
    /// Eye → Vulkan clip space (see [`crate::math::projection`]).
    pub proj: Mat4,
}

/// Where an eye is rendered: an OpenXR swapchain image or an offscreen image.
#[derive(Clone, Copy, Debug)]
pub struct EyeTarget {
    pub image: vk::Image,
    /// sRGB colour view.
    pub view: vk::ImageView,
    pub extent: vk::Extent2D,
    /// Layout to leave the image in (COLOR_ATTACHMENT_OPTIMAL for OpenXR).
    pub final_layout: vk::ImageLayout,
}

pub type PanelId = usize;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum QuadTexture {
    /// Solid colour (tinted by the quad colour).
    White,
    Panel(PanelId),
}

/// A textured quad in world space.
#[derive(Clone, Copy, Debug)]
pub struct QuadDraw {
    pub texture: QuadTexture,
    /// Top-left, top-right, bottom-right, bottom-left.
    pub corners: [Vec3; 4],
    /// Texture coordinates of the top-left and bottom-right corners.
    pub uv: [Vec2; 2],
    /// Linear, premultiplied tint.
    pub color: [f32; 4],
}

impl QuadDraw {
    /// A quad centred on `transform`'s origin, `width` x `height` metres, in
    /// its local XY plane facing +Z.
    pub fn panel(
        texture: QuadTexture,
        transform: Mat4,
        width: f32,
        height: f32,
        opacity: f32,
    ) -> QuadDraw {
        let (w, h) = (width / 2.0, height / 2.0);
        let p = |x: f32, y: f32| transform.transform_point3(Vec3::new(x, y, 0.0));
        QuadDraw {
            texture,
            corners: [p(-w, h), p(w, h), p(w, -h), p(-w, -h)],
            uv: [Vec2::ZERO, Vec2::ONE],
            color: [opacity; 4],
        }
    }

    /// A flat ribbon from `a` to `b`, `width` wide, facing `eye`.
    pub fn line(a: Vec3, b: Vec3, width: f32, eye: Vec3, color: [f32; 4]) -> QuadDraw {
        let dir = (b - a).normalize_or_zero();
        let to_eye = (eye - (a + b) * 0.5).normalize_or_zero();
        let side = dir.cross(to_eye).normalize_or_zero() * (width / 2.0);
        QuadDraw {
            texture: QuadTexture::White,
            corners: [a - side, a + side, b + side, b - side],
            uv: [Vec2::ZERO, Vec2::ONE],
            color,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct QuadVertex {
    pos: [f32; 3],
    uv: [f32; 2],
    color: [f32; 4],
}

struct Tex {
    image: Image,
    set: vk::DescriptorSet,
}

struct Panel {
    image: Image,
    set: vk::DescriptorSet,
    textures: HashMap<TextureId, Tex>,
    painted: bool,
    /// Bumped by every paint, so copies of the picture know when to update.
    version: u64,
}

struct Slot {
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    uniform: Buffer,
    scene_set: vk::DescriptorSet,
    scene_set_dirty: bool,
    /// The video image set (index, generation) the scene set points at.
    video_bound: Option<(usize, u64)>,
    has_video: bool,
    quad_vb: Buffer,
    egui_vb: Buffer,
    egui_ib: Buffer,
    egui_vb_used: u64,
    egui_ib_used: u64,
    staging: Buffer,
    staging_used: u64,
    garbage: Vec<Image>,
    garbage_sets: Vec<vk::DescriptorSet>,
    garbage_buffers: Vec<Buffer>,
}

pub struct Renderer {
    gpu: Arc<Gpu>,
    color_format: vk::Format,
    pool: vk::CommandPool,
    dpool: vk::DescriptorPool,
    sampler: vk::Sampler,
    scene_layout: vk::DescriptorSetLayout,
    scene_pl: vk::PipelineLayout,
    scene_pipe: vk::Pipeline,
    tex_layout: vk::DescriptorSetLayout,
    quad_pl: vk::PipelineLayout,
    quad_pipe: vk::Pipeline,
    egui_pl: vk::PipelineLayout,
    egui_pipe: vk::Pipeline,
    white: Tex,
    dummy: Image,
    panels: Vec<Option<Panel>>,
    slots: Vec<Slot>,
    /// Two video image sets shared by the slots (see `set_video`), which
    /// is newest, and how often each was recreated.
    videos: [Option<VideoPlanes>; 2],
    video_cur: usize,
    video_gen: [u64; 2],
    current: usize,
    recording: bool,
}

impl Renderer {
    /// `color_format` is the sRGB format of the eye targets.
    pub fn new(gpu: Arc<Gpu>, color_format: vk::Format) -> Result<Renderer> {
        let d = &gpu.device;
        // SAFETY: plain object creation on a valid device.
        unsafe {
            let pool = d
                .create_command_pool(
                    &vk::CommandPoolCreateInfo::default()
                        .queue_family_index(gpu.queue_family)
                        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                    None,
                )
                .ctx("command pool")?;
            let sizes = [
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::UNIFORM_BUFFER,
                    descriptor_count: 16,
                },
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::SAMPLER,
                    descriptor_count: 1024,
                },
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::SAMPLED_IMAGE,
                    descriptor_count: 2048,
                },
            ];
            let dpool = d
                .create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .flags(vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET)
                        .max_sets(1024)
                        .pool_sizes(&sizes),
                    None,
                )
                .ctx("descriptor pool")?;
            let sampler = d
                .create_sampler(
                    &vk::SamplerCreateInfo::default()
                        .mag_filter(vk::Filter::LINEAR)
                        .min_filter(vk::Filter::LINEAR)
                        .mipmap_mode(vk::SamplerMipmapMode::LINEAR)
                        .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                        .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                        .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                        .max_lod(vk::LOD_CLAMP_NONE),
                    None,
                )
                .ctx("sampler")?;

            let scene_layout = pipeline::set_layout(
                &gpu,
                &[
                    (0, vk::DescriptorType::UNIFORM_BUFFER),
                    (1, vk::DescriptorType::SAMPLER),
                    (2, vk::DescriptorType::SAMPLED_IMAGE),
                    (3, vk::DescriptorType::SAMPLED_IMAGE),
                    (4, vk::DescriptorType::SAMPLED_IMAGE),
                ],
            )?;
            let scene_pl = pipeline::layout(&gpu, scene_layout, 4)?;
            let scene_pipe = pipeline::create(
                &gpu,
                &PipelineDesc {
                    spv: crate::SCENE_SPV,
                    layout: scene_pl,
                    color_format,
                    vertex_stride: 0,
                    attributes: &[],
                    blend: false,
                },
            )?;
            let tex_layout = pipeline::set_layout(
                &gpu,
                &[
                    (0, vk::DescriptorType::SAMPLER),
                    (1, vk::DescriptorType::SAMPLED_IMAGE),
                ],
            )?;
            let quad_pl = pipeline::layout(&gpu, tex_layout, 64)?;
            let quad_attrs = [
                vk::VertexInputAttributeDescription {
                    location: 0,
                    binding: 0,
                    format: vk::Format::R32G32B32_SFLOAT,
                    offset: 0,
                },
                vk::VertexInputAttributeDescription {
                    location: 1,
                    binding: 0,
                    format: vk::Format::R32G32_SFLOAT,
                    offset: 12,
                },
                vk::VertexInputAttributeDescription {
                    location: 2,
                    binding: 0,
                    format: vk::Format::R32G32B32A32_SFLOAT,
                    offset: 20,
                },
            ];
            let quad_pipe = pipeline::create(
                &gpu,
                &PipelineDesc {
                    spv: crate::QUAD_SPV,
                    layout: quad_pl,
                    color_format,
                    vertex_stride: std::mem::size_of::<QuadVertex>() as u32,
                    attributes: &quad_attrs,
                    blend: true,
                },
            )?;
            let egui_pl = pipeline::layout(&gpu, tex_layout, 8)?;
            let egui_attrs = [
                vk::VertexInputAttributeDescription {
                    location: 0,
                    binding: 0,
                    format: vk::Format::R32G32_SFLOAT,
                    offset: 0,
                },
                vk::VertexInputAttributeDescription {
                    location: 1,
                    binding: 0,
                    format: vk::Format::R32G32_SFLOAT,
                    offset: 8,
                },
                vk::VertexInputAttributeDescription {
                    location: 2,
                    binding: 0,
                    format: vk::Format::R8G8B8A8_UNORM,
                    offset: 16,
                },
            ];
            let egui_pipe = pipeline::create(
                &gpu,
                &PipelineDesc {
                    spv: crate::EGUI_SPV,
                    layout: egui_pl,
                    color_format: PANEL_FORMATS[0],
                    vertex_stride: 20,
                    attributes: &egui_attrs,
                    blend: true,
                },
            )?;

            let mut r = Renderer {
                gpu: gpu.clone(),
                color_format,
                pool,
                dpool,
                sampler,
                scene_layout,
                scene_pl,
                scene_pipe,
                tex_layout,
                quad_pl,
                quad_pipe,
                egui_pl,
                egui_pipe,
                white: Tex {
                    image: Image::new(&gpu, 1, 1, &PANEL_FORMATS, usage_sampled(), 1, "white")?,
                    set: vk::DescriptorSet::null(),
                },
                dummy: Image::new(
                    &gpu,
                    1,
                    1,
                    &[vk::Format::R8G8B8A8_UNORM],
                    usage_sampled(),
                    1,
                    "dummy",
                )?,
                panels: Vec::new(),
                slots: Vec::new(),
                videos: [None, None],
                video_cur: 0,
                video_gen: [0, 0],
                current: 0,
                recording: false,
            };
            r.white.set = r.tex_set(r.white.image.views[1])?;
            // Initialise the white and dummy images.
            let (white, dummy) = (r.white.image.image, r.dummy.image);
            r.one_shot(|gpu, cmd| {
                for (img, value) in [(white, 1.0f32), (dummy, 0.0)] {
                    mem::barrier(
                        gpu,
                        cmd,
                        img,
                        mem::range(0, 1),
                        vk::ImageLayout::UNDEFINED,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        mem::TOP,
                        mem::TRANSFER_WRITE,
                    );
                    gpu.device.cmd_clear_color_image(
                        cmd,
                        img,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &vk::ClearColorValue {
                            float32: [value; 4],
                        },
                        &[mem::range(0, 1)],
                    );
                    mem::barrier(
                        gpu,
                        cmd,
                        img,
                        mem::range(0, 1),
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                        mem::TRANSFER_WRITE,
                        mem::SHADER_READ,
                    );
                }
            })?;
            for _ in 0..SLOTS {
                let slot = r.new_slot()?;
                r.slots.push(slot);
            }
            Ok(r)
        }
    }

    pub fn gpu(&self) -> &Arc<Gpu> {
        &self.gpu
    }

    pub fn color_format(&self) -> vk::Format {
        self.color_format
    }

    fn new_slot(&self) -> Result<Slot> {
        let d = &self.gpu.device;
        // SAFETY: allocation from our pool; fence created signalled so the
        // first wait returns immediately.
        unsafe {
            let cmd = d
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(self.pool)
                        .command_buffer_count(1),
                )
                .ctx("command buffer")?[0];
            let fence = d
                .create_fence(
                    &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                    None,
                )
                .ctx("fence")?;
            let layouts = [self.scene_layout];
            let scene_set = d
                .allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(self.dpool)
                        .set_layouts(&layouts),
                )
                .ctx("scene descriptor set")?[0];
            let uniform = Buffer::new(
                &self.gpu,
                std::mem::size_of::<SceneParams>() as u64,
                vk::BufferUsageFlags::UNIFORM_BUFFER,
                MemoryLocation::CpuToGpu,
                "scene params",
            )?;
            let cpu = |size, usage, name| {
                Buffer::new(&self.gpu, size, usage, MemoryLocation::CpuToGpu, name)
            };
            Ok(Slot {
                cmd,
                fence,
                uniform,
                scene_set,
                scene_set_dirty: true,
                video_bound: None,
                has_video: false,
                quad_vb: cpu(64 << 10, vk::BufferUsageFlags::VERTEX_BUFFER, "quads")?,
                egui_vb: cpu(
                    1 << 20,
                    vk::BufferUsageFlags::VERTEX_BUFFER,
                    "egui vertices",
                )?,
                egui_ib: cpu(1 << 20, vk::BufferUsageFlags::INDEX_BUFFER, "egui indices")?,
                egui_vb_used: 0,
                egui_ib_used: 0,
                staging: cpu(4 << 20, vk::BufferUsageFlags::TRANSFER_SRC, "egui staging")?,
                staging_used: 0,
                garbage: Vec::new(),
                garbage_sets: Vec::new(),
                garbage_buffers: Vec::new(),
            })
        }
    }

    fn tex_set(&self, view: vk::ImageView) -> Result<vk::DescriptorSet> {
        let layouts = [self.tex_layout];
        // SAFETY: allocation from our pool and writes of valid handles.
        unsafe {
            let set = self
                .gpu
                .device
                .allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(self.dpool)
                        .set_layouts(&layouts),
                )
                .ctx("texture descriptor set")?[0];
            let samplers = [vk::DescriptorImageInfo::default().sampler(self.sampler)];
            let images = [vk::DescriptorImageInfo::default()
                .image_view(view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let writes = [
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::SAMPLER)
                    .image_info(&samplers),
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(1)
                    .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                    .image_info(&images),
            ];
            self.gpu.device.update_descriptor_sets(&writes, &[]);
            Ok(set)
        }
    }

    /// Records and runs commands immediately, waiting for completion.
    pub fn one_shot(&self, f: impl FnOnce(&Gpu, vk::CommandBuffer)) -> Result<()> {
        let d = &self.gpu.device;
        // SAFETY: temporary command buffer and fence, freed before returning.
        unsafe {
            let cmd = d
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(self.pool)
                        .command_buffer_count(1),
                )
                .ctx("one-shot command buffer")?[0];
            d.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .ctx("begin")?;
            f(&self.gpu, cmd);
            d.end_command_buffer(cmd).ctx("end")?;
            let fence = d
                .create_fence(&vk::FenceCreateInfo::default(), None)
                .ctx("fence")?;
            let cmds = [cmd];
            d.queue_submit(
                self.gpu.queue,
                &[vk::SubmitInfo::default().command_buffers(&cmds)],
                fence,
            )
            .ctx("submit")?;
            let r = d.wait_for_fences(&[fence], true, u64::MAX).ctx("wait");
            d.destroy_fence(fence, None);
            d.free_command_buffers(self.pool, &cmds);
            r
        }
    }

    /// Creates an off-screen UI panel of `width` x `height` pixels.
    pub fn create_panel(&mut self, width: u32, height: u32) -> Result<PanelId> {
        let mips = 32 - width.max(height).leading_zeros();
        let image = Image::new(
            &self.gpu,
            width,
            height,
            &PANEL_FORMATS,
            vk::ImageUsageFlags::COLOR_ATTACHMENT
                | vk::ImageUsageFlags::SAMPLED
                | vk::ImageUsageFlags::TRANSFER_SRC
                | vk::ImageUsageFlags::TRANSFER_DST,
            mips,
            "ui panel",
        )?;
        let set = self.tex_set(image.views[1])?;
        let img = image.image;
        self.one_shot(|gpu, cmd| unsafe {
            mem::barrier(
                gpu,
                cmd,
                img,
                mem::range(0, mips),
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                mem::TOP,
                mem::TRANSFER_WRITE,
            );
            gpu.device.cmd_clear_color_image(
                cmd,
                img,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &vk::ClearColorValue { float32: [0.0; 4] },
                &[mem::range(0, mips)],
            );
            mem::barrier(
                gpu,
                cmd,
                img,
                mem::range(0, mips),
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                mem::TRANSFER_WRITE,
                mem::SHADER_READ,
            );
        })?;
        let panel = Panel {
            image,
            set,
            textures: HashMap::new(),
            painted: false,
            version: 0,
        };
        match self.panels.iter().position(|p| p.is_none()) {
            Some(i) => {
                self.panels[i] = Some(panel);
                Ok(i)
            }
            None => {
                self.panels.push(Some(panel));
                Ok(self.panels.len() - 1)
            }
        }
    }

    pub fn panel_size(&self, id: PanelId) -> Option<(u32, u32)> {
        self.panels
            .get(id)?
            .as_ref()
            .map(|p| (p.image.extent.width, p.image.extent.height))
    }

    /// Destroys a panel (deferred until no frame uses it).
    pub fn destroy_panel(&mut self, id: PanelId) {
        if let Some(Some(p)) = self.panels.get_mut(id).map(Option::take) {
            let slot = &mut self.slots[self.current];
            slot.garbage.push(p.image);
            slot.garbage_sets.push(p.set);
            for (_, t) in p.textures {
                slot.garbage.push(t.image);
                slot.garbage_sets.push(t.set);
            }
        }
    }

    /// Waits for the next slot and starts recording.
    pub fn begin_frame(&mut self) -> Result<()> {
        self.current = (self.current + 1) % SLOTS;
        let d = &self.gpu.device;
        let slot = &mut self.slots[self.current];
        // SAFETY: the slot's previous submission is complete after the wait,
        // so its resources and garbage can be reused/destroyed.
        unsafe {
            d.wait_for_fences(&[slot.fence], true, u64::MAX)
                .ctx("wait for frame")?;
            for mut img in slot.garbage.drain(..) {
                img.destroy(&self.gpu);
            }
            if !slot.garbage_sets.is_empty() {
                let _ = d.free_descriptor_sets(self.dpool, &slot.garbage_sets);
                slot.garbage_sets.clear();
            }
            for mut b in slot.garbage_buffers.drain(..) {
                b.destroy(&self.gpu);
            }
            d.reset_command_buffer(slot.cmd, vk::CommandBufferResetFlags::empty())
                .ctx("reset")?;
            d.begin_command_buffer(
                slot.cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .ctx("begin frame")?;
        }
        slot.egui_vb_used = 0;
        slot.egui_ib_used = 0;
        slot.staging_used = 0;
        self.recording = true;
        Ok(())
    }

    /// Uploads the frame to show (or clears the video when `None`).
    /// Shows `frame` this frame. Each video frame is uploaded once, into
    /// whichever of the two shared image sets the frame in flight isn't
    /// reading: an 8K frame is 50 MB, and copying it for every frame slot
    /// cost the render thread its 90 Hz deadline.
    pub fn set_video(&mut self, frame: Option<&Arc<VideoFrame>>) -> Result<()> {
        let Some(f) = frame else {
            let slot = &mut self.slots[self.current];
            if slot.has_video {
                slot.scene_set_dirty = true;
            }
            slot.has_video = false;
            return Ok(());
        };
        let resident = self.videos[self.video_cur]
            .as_ref()
            .is_some_and(|v| v.key == Some(f.id));
        if !resident {
            // The other set was last read by the frame before the one in
            // flight, which begin_frame has waited for.
            let target = 1 - self.video_cur;
            let slot = &mut self.slots[self.current];
            let recreated = VideoPlanes::upload(
                &mut self.videos[target],
                &self.gpu,
                slot.cmd,
                f,
                &mut slot.garbage,
            )?;
            if recreated {
                self.video_gen[target] += 1;
            }
            self.video_cur = target;
        }
        let bound = (self.video_cur, self.video_gen[self.video_cur]);
        let slot = &mut self.slots[self.current];
        if slot.video_bound != Some(bound) || !slot.has_video {
            slot.scene_set_dirty = true;
            slot.video_bound = Some(bound);
        }
        slot.has_video = true;
        Ok(())
    }

    /// The video image set this slot shows, if any.
    fn slot_video(&self, slot: usize) -> Option<&VideoPlanes> {
        let s = &self.slots[slot];
        if !s.has_video {
            return None;
        }
        self.videos[s.video_bound?.0].as_ref()
    }

    fn staging_alloc(&mut self, bytes: &[u8]) -> Result<(vk::Buffer, u64)> {
        let slot = &mut self.slots[self.current];
        let need = slot.staging_used + bytes.len() as u64 + 256;
        if need > slot.staging.size {
            let bigger = Buffer::new(
                &self.gpu,
                need.max(slot.staging.size * 2),
                vk::BufferUsageFlags::TRANSFER_SRC,
                MemoryLocation::CpuToGpu,
                "egui staging",
            )?;
            let old = std::mem::replace(&mut slot.staging, bigger);
            slot.garbage_buffers.push(old);
            // Earlier copies this frame reference the old buffer; it stays
            // alive until the slot comes round again.
            slot.staging_used = 0;
        }
        let off = (slot.staging_used + 15) & !15;
        slot.staging.bytes()[off as usize..off as usize + bytes.len()].copy_from_slice(bytes);
        slot.staging_used = off + bytes.len() as u64;
        Ok((slot.staging.buffer, off))
    }

    fn upload_egui_texture(
        &mut self,
        panel: PanelId,
        id: TextureId,
        delta: &egui::epaint::ImageDelta,
    ) -> Result<()> {
        let egui::ImageData::Color(img) = &delta.image;
        let [w, h] = img.size;
        let bytes: &[u8] = bytemuck::cast_slice(&img.pixels);
        let (buf, off) = self.staging_alloc(bytes)?;
        let cmd = self.slots[self.current].cmd;
        let need_new = delta.pos.is_none();
        if need_new {
            let image = Image::new(
                &self.gpu,
                w as u32,
                h as u32,
                &[vk::Format::R8G8B8A8_UNORM],
                usage_sampled(),
                1,
                "egui texture",
            )?;
            let set = self.tex_set(image.views[0])?;
            let Some(Some(p)) = self.panels.get_mut(panel) else {
                return Ok(());
            };
            if let Some(old) = p.textures.insert(id, Tex { image, set }) {
                let slot = &mut self.slots[self.current];
                slot.garbage.push(old.image);
                slot.garbage_sets.push(old.set);
            }
        }
        let Some(Some(p)) = self.panels.get(panel) else {
            return Ok(());
        };
        let Some(tex) = p.textures.get(&id) else {
            return Ok(());
        };
        let [x, y] = delta.pos.unwrap_or([0, 0]);
        let old_layout = if need_new {
            vk::ImageLayout::UNDEFINED
        } else {
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
        };
        let gpu = &self.gpu;
        mem::barrier(
            gpu,
            cmd,
            tex.image.image,
            mem::range(0, 1),
            old_layout,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            mem::SHADER_READ,
            mem::TRANSFER_WRITE,
        );
        let region = vk::BufferImageCopy::default()
            .buffer_offset(off)
            .image_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .image_offset(vk::Offset3D {
                x: x as i32,
                y: y as i32,
                z: 0,
            })
            .image_extent(vk::Extent3D {
                width: w as u32,
                height: h as u32,
                depth: 1,
            });
        // SAFETY: recording; region lies inside the image.
        unsafe {
            gpu.device.cmd_copy_buffer_to_image(
                cmd,
                buf,
                tex.image.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[region],
            )
        };
        mem::barrier(
            gpu,
            cmd,
            tex.image.image,
            mem::range(0, 1),
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            mem::TRANSFER_WRITE,
            mem::SHADER_READ,
        );
        Ok(())
    }

    /// Paints egui output into a panel. `pixels_per_point` maps egui points
    /// to panel pixels.
    pub fn paint_panel(
        &mut self,
        panel: PanelId,
        primitives: &[ClippedPrimitive],
        delta: &TexturesDelta,
        pixels_per_point: f32,
    ) -> Result<()> {
        for (id, d) in &delta.set {
            self.upload_egui_texture(panel, *id, d)?;
        }
        let Some(Some(p)) = self.panels.get(panel) else {
            return Ok(());
        };
        let (pw, ph) = (p.image.extent.width, p.image.extent.height);
        let (img, view, mips) = (p.image.image, p.image.level0, p.image.mips);

        // Gather geometry into the slot's buffers.
        let mut draws: Vec<(vk::DescriptorSet, vk::Rect2D, u32, u32, i32)> = Vec::new();
        {
            let slot = &mut self.slots[self.current];
            let mut vbytes = Vec::new();
            let mut ibytes = Vec::new();
            let (mut vcount, mut icount) = (
                (slot.egui_vb_used / 20) as i32,
                (slot.egui_ib_used / 4) as u32,
            );
            for cp in primitives {
                let Primitive::Mesh(mesh) = &cp.primitive else {
                    continue;
                };
                if mesh.indices.is_empty() {
                    continue;
                }
                let set = match p.textures.get(&mesh.texture_id) {
                    Some(t) => t.set,
                    None => self.white.set,
                };
                let r = cp.clip_rect;
                let x0 = (r.min.x * pixels_per_point).round().clamp(0.0, pw as f32) as i32;
                let y0 = (r.min.y * pixels_per_point).round().clamp(0.0, ph as f32) as i32;
                let x1 = (r.max.x * pixels_per_point).round().clamp(0.0, pw as f32) as i32;
                let y1 = (r.max.y * pixels_per_point).round().clamp(0.0, ph as f32) as i32;
                if x1 <= x0 || y1 <= y0 {
                    continue;
                }
                let scissor = vk::Rect2D {
                    offset: vk::Offset2D { x: x0, y: y0 },
                    extent: vk::Extent2D {
                        width: (x1 - x0) as u32,
                        height: (y1 - y0) as u32,
                    },
                };
                vbytes.extend_from_slice(bytemuck::cast_slice(&mesh.vertices));
                ibytes.extend_from_slice(bytemuck::cast_slice(&mesh.indices));
                draws.push((set, scissor, icount, mesh.indices.len() as u32, vcount));
                vcount += mesh.vertices.len() as i32;
                icount += mesh.indices.len() as u32;
            }
            for (buf, used, bytes, usage, name) in [
                (
                    &mut slot.egui_vb,
                    &mut slot.egui_vb_used,
                    &vbytes,
                    vk::BufferUsageFlags::VERTEX_BUFFER,
                    "egui vertices",
                ),
                (
                    &mut slot.egui_ib,
                    &mut slot.egui_ib_used,
                    &ibytes,
                    vk::BufferUsageFlags::INDEX_BUFFER,
                    "egui indices",
                ),
            ] {
                let need = *used + bytes.len() as u64;
                if need > buf.size {
                    // Grow; geometry already written this frame is copied over.
                    let mut bigger = Buffer::new(
                        &self.gpu,
                        (need * 2).max(buf.size * 2),
                        usage,
                        MemoryLocation::CpuToGpu,
                        name,
                    )?;
                    let keep = *used as usize;
                    bigger.bytes()[..keep].copy_from_slice(&buf.bytes()[..keep]);
                    let old = std::mem::replace(buf, bigger);
                    slot.garbage_buffers.push(old);
                }
                buf.bytes()[*used as usize..need as usize].copy_from_slice(bytes);
                *used = need;
            }
        }
        let slot = &self.slots[self.current];
        let (cmd, gpu) = (slot.cmd, &self.gpu);
        let d = &gpu.device;
        mem::barrier(
            gpu,
            cmd,
            img,
            mem::range(0, 1),
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            mem::SHADER_READ,
            mem::COLOR_WRITE,
        );
        let attachment = [vk::RenderingAttachmentInfo::default()
            .image_view(view)
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .clear_value(vk::ClearValue {
                color: vk::ClearColorValue { float32: [0.0; 4] },
            })];
        let area = vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent: vk::Extent2D {
                width: pw,
                height: ph,
            },
        };
        // SAFETY: recording into the slot's command buffer with valid handles.
        unsafe {
            d.cmd_begin_rendering(
                cmd,
                &vk::RenderingInfo::default()
                    .render_area(area)
                    .layer_count(1)
                    .color_attachments(&attachment),
            );
            d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, self.egui_pipe);
            d.cmd_set_viewport(
                cmd,
                0,
                &[vk::Viewport {
                    x: 0.0,
                    y: 0.0,
                    width: pw as f32,
                    height: ph as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                }],
            );
            let size = [pw as f32 / pixels_per_point, ph as f32 / pixels_per_point];
            d.cmd_push_constants(
                cmd,
                self.egui_pl,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                bytemuck::cast_slice(&size),
            );
            d.cmd_bind_vertex_buffers(cmd, 0, &[slot.egui_vb.buffer], &[0]);
            d.cmd_bind_index_buffer(cmd, slot.egui_ib.buffer, 0, vk::IndexType::UINT32);
            for (set, scissor, first, count, base) in draws {
                d.cmd_set_scissor(cmd, 0, &[scissor]);
                d.cmd_bind_descriptor_sets(
                    cmd,
                    vk::PipelineBindPoint::GRAPHICS,
                    self.egui_pl,
                    0,
                    &[set],
                    &[],
                );
                d.cmd_draw_indexed(cmd, count, 1, first, base, 0);
            }
            d.cmd_end_rendering(cmd);
        }
        generate_mips(gpu, cmd, img, pw, ph, mips);
        // Free textures egui no longer needs (after this frame).
        if let Some(Some(p)) = self.panels.get_mut(panel) {
            p.painted = true;
            p.version += 1;
            let slot = &mut self.slots[self.current];
            for id in &delta.free {
                if let Some(t) = p.textures.remove(id) {
                    slot.garbage.push(t.image);
                    slot.garbage_sets.push(t.set);
                }
            }
        }
        Ok(())
    }

    fn update_scene_set(&mut self) {
        if !self.slots[self.current].scene_set_dirty {
            return;
        }
        let dummy = self.dummy.views[0];
        let views: Vec<vk::ImageView> = match self.slot_video(self.current) {
            Some(v) => {
                let mut vs: Vec<vk::ImageView> = v.images.iter().map(|i| i.views[0]).collect();
                vs.resize(3, dummy);
                vs
            }
            None => vec![dummy; 3],
        };
        let slot = &mut self.slots[self.current];
        let ubo = [vk::DescriptorBufferInfo::default()
            .buffer(slot.uniform.buffer)
            .range(vk::WHOLE_SIZE)];
        let samplers = [vk::DescriptorImageInfo::default().sampler(self.sampler)];
        let images: Vec<[vk::DescriptorImageInfo; 1]> = views
            .iter()
            .map(|&v| {
                [vk::DescriptorImageInfo::default()
                    .image_view(v)
                    .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)]
            })
            .collect();
        let mut writes = vec![
            vk::WriteDescriptorSet::default()
                .dst_set(slot.scene_set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                .buffer_info(&ubo),
            vk::WriteDescriptorSet::default()
                .dst_set(slot.scene_set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .image_info(&samplers),
        ];
        for (i, info) in images.iter().enumerate() {
            writes.push(
                vk::WriteDescriptorSet::default()
                    .dst_set(slot.scene_set)
                    .dst_binding(2 + i as u32)
                    .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                    .image_info(info),
            );
        }
        // SAFETY: the set is not in use by any pending submission (the slot's
        // fence was waited on in begin_frame).
        unsafe { self.gpu.device.update_descriptor_sets(&writes, &[]) };
        slot.scene_set_dirty = false;
    }

    /// Records both eye passes.
    pub fn draw(
        &mut self,
        targets: &[EyeTarget; 2],
        eyes: &[EyeView; 2],
        video: &VideoParams,
        quads: &[QuadDraw],
    ) -> Result<()> {
        self.update_scene_set();
        let inv = [
            (eyes[0].proj * eyes[0].view).inverse(),
            (eyes[1].proj * eyes[1].view).inverse(),
        ];
        let slot_idx = self.current;
        let frame_desc = self.slot_video(slot_idx).map(|v| v.desc);
        let p = params::build(video, frame_desc.as_ref(), inv);
        // Quad vertices: two triangles per quad.
        let mut verts: Vec<QuadVertex> = Vec::with_capacity(quads.len() * 6);
        let mut sets = Vec::with_capacity(quads.len());
        for q in quads {
            let set = match q.texture {
                QuadTexture::White => self.white.set,
                QuadTexture::Panel(id) => match self.panels.get(id).and_then(Option::as_ref) {
                    Some(p) => p.set,
                    None => continue,
                },
            };
            let [a, b] = q.uv;
            let uvs = [
                Vec2::new(a.x, a.y),
                Vec2::new(b.x, a.y),
                Vec2::new(b.x, b.y),
                Vec2::new(a.x, b.y),
            ];
            for i in [0usize, 1, 2, 0, 2, 3] {
                verts.push(QuadVertex {
                    pos: q.corners[i].to_array(),
                    uv: uvs[i].to_array(),
                    color: q.color,
                });
            }
            sets.push(set);
        }
        {
            let slot = &mut self.slots[slot_idx];
            slot.uniform.bytes()[..std::mem::size_of::<SceneParams>()]
                .copy_from_slice(bytemuck::bytes_of(&p));
            let bytes: &[u8] = bytemuck::cast_slice(&verts);
            if bytes.len() as u64 > slot.quad_vb.size {
                let bigger = Buffer::new(
                    &self.gpu,
                    bytes.len() as u64 * 2,
                    vk::BufferUsageFlags::VERTEX_BUFFER,
                    MemoryLocation::CpuToGpu,
                    "quads",
                )?;
                let old = std::mem::replace(&mut slot.quad_vb, bigger);
                slot.garbage_buffers.push(old);
            }
            slot.quad_vb.bytes()[..bytes.len()].copy_from_slice(bytes);
        }
        let slot = &self.slots[slot_idx];
        let (gpu, cmd) = (&self.gpu, slot.cmd);
        let d = &gpu.device;
        let bg = video.background;
        for (eye, t) in targets.iter().enumerate() {
            mem::barrier(
                gpu,
                cmd,
                t.image,
                mem::range(0, 1),
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                mem::TOP,
                mem::COLOR_WRITE,
            );
            let attachment = [vk::RenderingAttachmentInfo::default()
                .image_view(t.view)
                .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .clear_value(vk::ClearValue {
                    color: vk::ClearColorValue { float32: bg },
                })];
            let area = vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: t.extent,
            };
            // SAFETY: recording with valid pipelines, sets and buffers.
            unsafe {
                d.cmd_begin_rendering(
                    cmd,
                    &vk::RenderingInfo::default()
                        .render_area(area)
                        .layer_count(1)
                        .color_attachments(&attachment),
                );
                d.cmd_set_viewport(
                    cmd,
                    0,
                    &[vk::Viewport {
                        x: 0.0,
                        y: 0.0,
                        width: t.extent.width as f32,
                        height: t.extent.height as f32,
                        min_depth: 0.0,
                        max_depth: 1.0,
                    }],
                );
                d.cmd_set_scissor(cmd, 0, &[area]);
                d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, self.scene_pipe);
                d.cmd_bind_descriptor_sets(
                    cmd,
                    vk::PipelineBindPoint::GRAPHICS,
                    self.scene_pl,
                    0,
                    &[slot.scene_set],
                    &[],
                );
                d.cmd_push_constants(
                    cmd,
                    self.scene_pl,
                    vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                    0,
                    &(eye as u32).to_le_bytes(),
                );
                d.cmd_draw(cmd, 3, 1, 0, 0);
                if !verts.is_empty() {
                    d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, self.quad_pipe);
                    let vp = eyes[eye].proj * eyes[eye].view;
                    d.cmd_push_constants(
                        cmd,
                        self.quad_pl,
                        vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                        0,
                        bytemuck::bytes_of(&vp),
                    );
                    d.cmd_bind_vertex_buffers(cmd, 0, &[slot.quad_vb.buffer], &[0]);
                    for (i, set) in sets.iter().enumerate() {
                        d.cmd_bind_descriptor_sets(
                            cmd,
                            vk::PipelineBindPoint::GRAPHICS,
                            self.quad_pl,
                            0,
                            &[*set],
                            &[],
                        );
                        d.cmd_draw(cmd, 6, 1, (i * 6) as u32, 0);
                    }
                }
                d.cmd_end_rendering(cmd);
            }
            if t.final_layout != vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL {
                let dst = if t.final_layout == vk::ImageLayout::TRANSFER_SRC_OPTIMAL {
                    mem::TRANSFER_READ
                } else {
                    mem::SHADER_READ
                };
                mem::barrier(
                    gpu,
                    cmd,
                    t.image,
                    mem::range(0, 1),
                    vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                    t.final_layout,
                    mem::COLOR_WRITE,
                    dst,
                );
            }
        }
        Ok(())
    }

    /// Ends recording and submits the frame.
    pub fn end_frame(&mut self) -> Result<()> {
        let slot = &self.slots[self.current];
        let d = &self.gpu.device;
        // SAFETY: the command buffer is recording; fence is unsignalled after reset.
        unsafe {
            d.end_command_buffer(slot.cmd).ctx("end frame")?;
            d.reset_fences(&[slot.fence]).ctx("reset fence")?;
            let cmds = [slot.cmd];
            d.queue_submit(
                self.gpu.queue,
                &[vk::SubmitInfo::default().command_buffers(&cmds)],
                slot.fence,
            )
            .ctx("submit frame")?;
        }
        self.recording = false;
        Ok(())
    }

    /// How many times a panel has been painted (0: never, or no such panel).
    pub fn panel_version(&self, panel: PanelId) -> u64 {
        match self.panels.get(panel) {
            Some(Some(p)) if p.painted => p.version,
            _ => 0,
        }
    }

    /// Copies a panel's picture into `dst`, an image of the same size and a
    /// compatible RGBA8 format (an OpenXR quad layer's swapchain image, left
    /// in COLOR_ATTACHMENT_OPTIMAL as OpenXR expects). Recorded into this
    /// frame, after the panel's paint; call between `begin_frame` and
    /// `end_frame`.
    pub fn copy_panel_to(&mut self, panel: PanelId, dst: vk::Image) -> Result<()> {
        let Some(Some(p)) = self.panels.get(panel) else {
            return Err(crate::Error::Unsupported("no such panel".into()));
        };
        if !self.recording {
            return Err(crate::Error::Unsupported("copy outside a frame".into()));
        }
        let (src, extent) = (p.image.image, p.image.extent);
        let cmd = self.slots[self.current].cmd;
        let gpu = &self.gpu;
        // The whole image is overwritten: its old contents can go.
        mem::barrier(
            gpu,
            cmd,
            dst,
            mem::range(0, 1),
            vk::ImageLayout::UNDEFINED,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            mem::TOP,
            mem::TRANSFER_WRITE,
        );
        mem::barrier(
            gpu,
            cmd,
            src,
            mem::range(0, 1),
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            mem::SHADER_READ,
            mem::TRANSFER_READ,
        );
        let layers = vk::ImageSubresourceLayers {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: 0,
            base_array_layer: 0,
            layer_count: 1,
        };
        let region = vk::ImageCopy::default()
            .src_subresource(layers)
            .dst_subresource(layers)
            .extent(vk::Extent3D {
                width: extent.width,
                height: extent.height,
                depth: 1,
            });
        // SAFETY: recording; both images are extent-sized and in the
        // transfer layouts set above.
        unsafe {
            gpu.device.cmd_copy_image(
                cmd,
                src,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                dst,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[region],
            )
        };
        mem::barrier(
            gpu,
            cmd,
            src,
            mem::range(0, 1),
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            mem::TRANSFER_READ,
            mem::SHADER_READ,
        );
        mem::barrier(
            gpu,
            cmd,
            dst,
            mem::range(0, 1),
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            mem::TRANSFER_WRITE,
            mem::COLOR_WRITE,
        );
        Ok(())
    }

    /// Fills `dst` (`width`×`height`, RGBA8, e.g. an OpenXR swapchain image)
    /// with `rgba` and leaves it in COLOR_ATTACHMENT_OPTIMAL. Waits for the
    /// GPU; for small, static pictures.
    pub fn upload_rgba(&self, dst: vk::Image, width: u32, height: u32, rgba: &[u8]) -> Result<()> {
        let mut staging = Buffer::new(
            &self.gpu,
            rgba.len() as u64,
            vk::BufferUsageFlags::TRANSFER_SRC,
            MemoryLocation::CpuToGpu,
            "upload",
        )?;
        staging.bytes()[..rgba.len()].copy_from_slice(rgba);
        let buf = staging.buffer;
        let r = self.one_shot(|gpu, cmd| {
            mem::barrier(
                gpu,
                cmd,
                dst,
                mem::range(0, 1),
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                mem::TOP,
                mem::TRANSFER_WRITE,
            );
            let region = vk::BufferImageCopy::default()
                .image_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .image_extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                });
            // SAFETY: recording; the buffer holds width*height texels.
            unsafe {
                gpu.device.cmd_copy_buffer_to_image(
                    cmd,
                    buf,
                    dst,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[region],
                )
            };
            mem::barrier(
                gpu,
                cmd,
                dst,
                mem::range(0, 1),
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                mem::TRANSFER_WRITE,
                mem::COLOR_WRITE,
            );
        });
        staging.destroy(&self.gpu);
        r
    }

    /// Blocks until all submitted frames are done.
    pub fn wait_idle(&self) {
        self.gpu.wait_idle();
    }

    /// Reads a painted panel back as `(width, height, RGBA8)`: sRGB-encoded,
    /// premultiplied alpha, as egui painted it. For screenshots of the UI
    /// (the desktop preview); waits for the GPU.
    pub fn read_panel(&self, panel: PanelId) -> Result<(u32, u32, Vec<u8>)> {
        let Some(Some(p)) = self.panels.get(panel) else {
            return Err(crate::Error::Unsupported("no such panel".into()));
        };
        let (image, extent) = (p.image.image, p.image.extent);
        let mut readback = Buffer::new(
            &self.gpu,
            extent.width as u64 * extent.height as u64 * 4,
            vk::BufferUsageFlags::TRANSFER_DST,
            MemoryLocation::GpuToCpu,
            "panel readback",
        )?;
        let buffer = readback.buffer;
        self.wait_idle();
        self.one_shot(|gpu, cmd| {
            mem::barrier(
                gpu,
                cmd,
                image,
                mem::range(0, 1),
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                mem::SHADER_READ,
                mem::TRANSFER_READ,
            );
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
            // SAFETY: level 0 is in TRANSFER_SRC; the buffer holds it.
            unsafe {
                gpu.device.cmd_copy_image_to_buffer(
                    cmd,
                    image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    buffer,
                    &[region],
                )
            };
            mem::barrier(
                gpu,
                cmd,
                image,
                mem::range(0, 1),
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                mem::TRANSFER_READ,
                mem::SHADER_READ,
            );
        })?;
        let n = (extent.width * extent.height * 4) as usize;
        let px = readback.bytes()[..n].to_vec();
        readback.destroy(&self.gpu);
        Ok((extent.width, extent.height, px))
    }
}

fn usage_sampled() -> vk::ImageUsageFlags {
    vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST
}

/// Blits level 0 down the mip chain and leaves every level shader-readable.
fn generate_mips(gpu: &Gpu, cmd: vk::CommandBuffer, image: vk::Image, w: u32, h: u32, mips: u32) {
    let d = &gpu.device;
    mem::barrier(
        gpu,
        cmd,
        image,
        mem::range(0, 1),
        vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        mem::COLOR_WRITE,
        mem::TRANSFER_READ,
    );
    let (mut mw, mut mh) = (w as i32, h as i32);
    for level in 1..mips {
        let (nw, nh) = ((mw / 2).max(1), (mh / 2).max(1));
        mem::barrier(
            gpu,
            cmd,
            image,
            mem::range(level, 1),
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            mem::SHADER_READ,
            mem::TRANSFER_WRITE,
        );
        let sub = |l| vk::ImageSubresourceLayers {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: l,
            base_array_layer: 0,
            layer_count: 1,
        };
        let blit = vk::ImageBlit {
            src_subresource: sub(level - 1),
            src_offsets: [vk::Offset3D::default(), vk::Offset3D { x: mw, y: mh, z: 1 }],
            dst_subresource: sub(level),
            dst_offsets: [vk::Offset3D::default(), vk::Offset3D { x: nw, y: nh, z: 1 }],
        };
        // SAFETY: recording; levels are in the right layouts.
        unsafe {
            d.cmd_blit_image(
                cmd,
                image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[blit],
                vk::Filter::LINEAR,
            )
        };
        mem::barrier(
            gpu,
            cmd,
            image,
            mem::range(level, 1),
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            mem::TRANSFER_WRITE,
            mem::TRANSFER_READ,
        );
        mw = nw;
        mh = nh;
    }
    mem::barrier(
        gpu,
        cmd,
        image,
        mem::range(0, mips),
        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        mem::TRANSFER_READ,
        mem::SHADER_READ,
    );
}

impl Drop for Renderer {
    fn drop(&mut self) {
        self.gpu.wait_idle();
        let gpu = self.gpu.clone();
        let d = &gpu.device;
        // SAFETY: the device is idle; everything below was created by us.
        unsafe {
            for v in &mut self.videos {
                if let Some(mut v) = v.take() {
                    v.destroy(&gpu);
                }
            }
            for mut s in self.slots.drain(..) {
                for mut i in s.garbage.drain(..) {
                    i.destroy(&gpu);
                }
                for mut b in s.garbage_buffers.drain(..) {
                    b.destroy(&gpu);
                }
                for mut b in [s.uniform, s.quad_vb, s.egui_vb, s.egui_ib, s.staging] {
                    b.destroy(&gpu);
                }
                d.destroy_fence(s.fence, None);
            }
            for p in self.panels.drain(..).flatten() {
                let mut img = p.image;
                img.destroy(&gpu);
                for (_, mut t) in p.textures {
                    t.image.destroy(&gpu);
                }
            }
            self.white.image.destroy(&gpu);
            self.dummy.destroy(&gpu);
            for p in [self.scene_pipe, self.quad_pipe, self.egui_pipe] {
                d.destroy_pipeline(p, None);
            }
            for l in [self.scene_pl, self.quad_pl, self.egui_pl] {
                d.destroy_pipeline_layout(l, None);
            }
            d.destroy_descriptor_set_layout(self.scene_layout, None);
            d.destroy_descriptor_set_layout(self.tex_layout, None);
            d.destroy_sampler(self.sampler, None);
            d.destroy_descriptor_pool(self.dpool, None);
            d.destroy_command_pool(self.pool, None);
        }
    }
}
