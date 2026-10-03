//! The per-frame renderer the app drives.
//!
//! ```text
//! renderer.begin_frame()?;                 // waits for this slot's previous use
//! renderer.upload_video_frame(&frame)?;    // when the decoder has a new frame
//! renderer.render_eyes(&eye_request)?;     // YUV→RGB compute (if needed) + both eyes
//! renderer.render_ui(&ui_request)?;        // once per UI layer
//! renderer.end_frame()?;                   // submit; then release XR swapchain images
//! ```
//!
//! Render graph per frame (one command buffer): [CPU plane copies] →
//! YUV→RGB compute into RGBA16F (only when a new frame arrived or the colour
//! parameters changed) → per-eye projection pass into the XR swapchain
//! images → UI passes into UI layer images. Targets are left in
//! `COLOR_ATTACHMENT_OPTIMAL`, as `XR_KHR_vulkan_enable` requires on release.
//!
//! Hot-path allocations are avoided: staging, UI vertex/index buffers and
//! per-target views are reused and only grow; barriers use stack arrays.

use super::pipelines::Pipelines;
use super::textures::{LruBudget, Texture};
use super::util::{align_up, barrier, create_view, Buffer, Garbage, Image, Transition};
use super::video::{self, CpuPlanes, ImportedFrame, Source, VideoState};
use super::{needs_shader_encode, GfxError, GpuContext, Result, UiPush};
use crate::camera::{projection_matrix, EyeView};
use crate::color::{ColorPush, DisplayParams};
use crate::correction::EyePush;
use crate::frame::{CpuFrame, DmaBufFrame, VideoFrame};
use crate::mesh::{build_mesh, Mesh, MeshDensity};
use ash::vk;
use fp_core::draw::{AtlasImage, DrawList, TextureId};
use fp_core::{Corrections, Projection, ViewSettings};
use glam::Mat4;
use std::collections::HashMap;
use std::path::PathBuf;

const FRAMES_IN_FLIGHT: usize = 2;
const MIN_STAGING: u64 = 4 << 20;
const MIN_UI_BUFFER: u64 = 256 << 10;

/// Renderer construction options.
#[derive(Debug, Clone)]
pub struct RendererConfig {
    /// Base directory for [`Projection::CustomMesh`] paths.
    pub mesh_dir: PathBuf,
    pub mesh_density: MeshDensity,
    /// Byte budget of the thumbnail cache.
    pub image_cache_budget: usize,
    /// Max imported decoder buffers kept alive (decoder pools are ~4–16).
    pub dmabuf_cache_size: usize,
    pub near_m: f32,
    pub far_m: f32,
}

impl Default for RendererConfig {
    fn default() -> Self {
        RendererConfig {
            mesh_dir: PathBuf::from("."),
            mesh_density: MeshDensity::default(),
            image_cache_budget: 256 << 20,
            dmabuf_cache_size: 24,
            near_m: 0.05,
            far_m: 1000.0,
        }
    }
}

/// A colour attachment to render into (an XR swapchain image layer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderTarget {
    pub image: vk::Image,
    pub format: vk::Format,
    pub width: u32,
    pub height: u32,
    pub array_layer: u32,
}

/// Inputs for [`Renderer::render_eyes`].
#[derive(Debug, Clone, Copy)]
pub struct EyeRenderRequest<'a> {
    /// Eye poses in the app's reference space (left, right).
    pub views: [EyeView; 2],
    pub targets: [RenderTarget; 2],
    /// Projection, stereo layout and eye swap.
    pub settings: &'a ViewSettings,
    /// Corrections in effect now (already keyframe-interpolated).
    pub corrections: &'a Corrections,
    /// Placement of the mesh in the reference space (recenter yaw, screen
    /// position, head-locked transforms…).
    pub model: Mat4,
    /// Keep immersive spheres centred on the head (ignore head translation).
    pub follow_head: bool,
    pub clear_color: [f32; 4],
    /// Premultiplied multiplier for dimming / fades.
    pub tint: [f32; 4],
}

/// Inputs for [`Renderer::render_ui`].
#[derive(Debug, Clone, Copy)]
pub struct UiRenderRequest<'a> {
    pub draw_list: &'a DrawList,
    /// Font atlas; re-uploaded when its `version` changes.
    pub atlas: Option<&'a AtlasImage>,
    pub target: RenderTarget,
    /// Logical panel size the draw list was laid out in (pixels).
    pub panel_size: [f32; 2],
    pub clear_color: [f32; 4],
}

struct FrameSlot {
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    staging: Buffer,
    staging_used: u64,
    ui_vb: Buffer,
    ui_vb_used: u64,
    ui_ib: Buffer,
    ui_ib_used: u64,
    garbage: Vec<Garbage>,
}

#[derive(Clone, PartialEq)]
struct MeshKey {
    projection: Projection,
    aspect_milli: u32,
}

struct GpuMesh {
    key: MeshKey,
    vb: Buffer,
    ib: Buffer,
    index_count: u32,
}

struct PendingImage {
    id: u64,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

/// The Vulkan renderer. Owns all GPU resources it creates; never the device.
pub struct Renderer {
    gpu: GpuContext,
    config: RendererConfig,
    display: DisplayParams,
    cmd_pool: vk::CommandPool,
    desc_pool: vk::DescriptorPool,
    sampler: vk::Sampler,
    pipelines: Pipelines,
    slots: Vec<FrameSlot>,
    slot: usize,
    in_frame: bool,
    frame_no: u64,
    copy_align: u64,
    video: VideoState,
    imports: HashMap<u64, ImportedFrame>,
    cpu_planes: Option<CpuPlanes>,
    mesh: Option<GpuMesh>,
    white: Option<Texture>,
    atlas: Option<(Texture, u64)>,
    images: LruBudget<Texture>,
    pending_images: Vec<PendingImage>,
    target_views: HashMap<(vk::Image, u32, vk::Format), vk::ImageView>,
    evicted: Vec<Texture>,
}

// SAFETY: all contained Vulkan handles and mapped pointers are plain data
// usable from any thread; the renderer is used by one thread at a time.
unsafe impl Send for Renderer {}

impl Renderer {
    /// Create the renderer on an existing device.
    pub fn new(gpu: GpuContext, config: RendererConfig) -> Result<Renderer> {
        unsafe {
            let d = &gpu.device;
            let cmd_pool = d.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(gpu.queue_family_index)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )?;
            let sizes = [
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::SAMPLED_IMAGE,
                    descriptor_count: 8192,
                },
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::SAMPLER,
                    descriptor_count: 8192,
                },
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::STORAGE_IMAGE,
                    descriptor_count: 256,
                },
            ];
            let desc_pool = d.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .flags(vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET)
                    .max_sets(4096)
                    .pool_sizes(&sizes),
                None,
            )?;
            let sampler = d.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::LINEAR)
                    .min_filter(vk::Filter::LINEAR)
                    .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .max_lod(0.0),
                None,
            )?;
            let pipelines = Pipelines::new(&gpu)?;
            let cmds = d.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(cmd_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(FRAMES_IN_FLIGHT as u32),
            )?;
            let mut slots = Vec::with_capacity(FRAMES_IN_FLIGHT);
            for cmd in cmds {
                let fence = d.create_fence(
                    &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                    None,
                )?;
                slots.push(FrameSlot {
                    cmd,
                    fence,
                    staging: Buffer::new(
                        &gpu,
                        MIN_STAGING,
                        vk::BufferUsageFlags::TRANSFER_SRC,
                        true,
                    )?,
                    staging_used: 0,
                    ui_vb: Buffer::new(
                        &gpu,
                        MIN_UI_BUFFER,
                        vk::BufferUsageFlags::VERTEX_BUFFER,
                        true,
                    )?,
                    ui_vb_used: 0,
                    ui_ib: Buffer::new(
                        &gpu,
                        MIN_UI_BUFFER,
                        vk::BufferUsageFlags::INDEX_BUFFER,
                        true,
                    )?,
                    ui_ib_used: 0,
                    garbage: Vec::new(),
                });
            }
            let copy_align = gpu.limits.optimal_buffer_copy_offset_alignment.max(16);
            let images = LruBudget::new(config.image_cache_budget);
            Ok(Renderer {
                gpu,
                config,
                display: DisplayParams::default(),
                cmd_pool,
                desc_pool,
                sampler,
                pipelines,
                slots,
                slot: FRAMES_IN_FLIGHT - 1,
                in_frame: false,
                frame_no: 0,
                copy_align,
                video: VideoState::new(),
                imports: HashMap::new(),
                cpu_planes: None,
                mesh: None,
                white: None,
                atlas: None,
                images,
                pending_images: Vec::new(),
                target_views: HashMap::new(),
                evicted: Vec::new(),
            })
        }
    }

    pub fn gpu(&self) -> &GpuContext {
        &self.gpu
    }

    pub fn set_display_params(&mut self, p: DisplayParams) {
        self.display = p;
    }

    pub fn set_mesh_density(&mut self, d: MeshDensity) {
        if d != self.config.mesh_density {
            self.config.mesh_density = d;
            if let Some(m) = self.mesh.take() {
                self.slots[self.slot]
                    .garbage
                    .extend([m.vb.into_garbage(), m.ib.into_garbage()]);
            }
        }
    }

    /// Size of the current video frame, if any has been uploaded.
    pub fn video_size(&self) -> Option<(u32, u32)> {
        (self.video.source != Source::None).then_some((self.video.width, self.video.height))
    }

    /// The conversion target holds a converted frame (safe to sample).
    fn video_ready(&self) -> bool {
        self.video.source != Source::None
            && self.video.output.is_some()
            && self.video.last_push.is_some()
    }

    // ---------------------------------------------------------------- frame

    /// Start recording a frame. Blocks until the GPU finished the frame that
    /// last used this slot (normally already done).
    pub fn begin_frame(&mut self) -> Result<()> {
        if self.in_frame {
            return Ok(());
        }
        unsafe {
            self.slot = (self.slot + 1) % FRAMES_IN_FLIGHT;
            self.frame_no += 1;
            let d = &self.gpu.device;
            let s = &mut self.slots[self.slot];
            d.wait_for_fences(&[s.fence], true, u64::MAX)?;
            d.reset_fences(&[s.fence])?;
            for g in s.garbage.drain(..) {
                g.destroy(&self.gpu, self.desc_pool);
            }
            s.staging_used = 0;
            s.ui_vb_used = 0;
            s.ui_ib_used = 0;
            d.reset_command_buffer(s.cmd, vk::CommandBufferResetFlags::empty())?;
            d.begin_command_buffer(
                s.cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            self.in_frame = true;
            if self.white.is_none() {
                let img = self.upload_texture(vk::Format::R8G8B8A8_UNORM, 1, 1, &[255; 4], None)?;
                let set = self.alloc_tex_set(img.view)?;
                self.white = Some(Texture { image: img, set });
            }
            for p in std::mem::take(&mut self.pending_images) {
                self.upload_image(p.id, p.width, p.height, &p.rgba)?;
            }
        }
        Ok(())
    }

    /// Finish and submit the frame. Release XR swapchain images afterwards.
    pub fn end_frame(&mut self) -> Result<()> {
        if !self.in_frame {
            return Err(GfxError::NotInFrame);
        }
        self.in_frame = false;
        unsafe {
            let s = &self.slots[self.slot];
            let d = &self.gpu.device;
            d.end_command_buffer(s.cmd)?;
            let cmds = [s.cmd];
            let submit = vk::SubmitInfo::default().command_buffers(&cmds);
            d.queue_submit(self.gpu.queue, &[submit], s.fence)?;
        }
        Ok(())
    }

    /// Block until the GPU is idle.
    pub fn wait_idle(&self) -> Result<()> {
        unsafe { Ok(self.gpu.device.device_wait_idle()?) }
    }

    fn cmd(&self) -> vk::CommandBuffer {
        self.slots[self.slot].cmd
    }

    fn require_frame(&self) -> Result<()> {
        if self.in_frame {
            Ok(())
        } else {
            Err(GfxError::NotInFrame)
        }
    }

    /// Reserve `len` bytes of this frame's staging buffer.
    unsafe fn stage(&mut self, len: usize) -> Result<(vk::Buffer, u64, *mut u8)> {
        let align = self.copy_align;
        let s = &mut self.slots[self.slot];
        let mut off = align_up(s.staging_used, align);
        if off + len as u64 > s.staging.size {
            let size = (s.staging.size * 2)
                .max(len as u64 + align)
                .max(MIN_STAGING);
            let new = Buffer::new(&self.gpu, size, vk::BufferUsageFlags::TRANSFER_SRC, true)?;
            let old = std::mem::replace(&mut s.staging, new);
            s.garbage.push(old.into_garbage());
            off = 0;
        }
        s.staging_used = off + len as u64;
        Ok((s.staging.buffer, off, s.staging.mapped.add(off as usize)))
    }

    unsafe fn alloc_set(&self, layout: vk::DescriptorSetLayout) -> Result<vk::DescriptorSet> {
        let layouts = [layout];
        let info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(self.desc_pool)
            .set_layouts(&layouts);
        Ok(self.gpu.device.allocate_descriptor_sets(&info)?[0])
    }

    unsafe fn alloc_tex_set(&self, view: vk::ImageView) -> Result<vk::DescriptorSet> {
        let set = self.alloc_set(self.pipelines.tex_dsl)?;
        video::write_tex_set(&self.gpu, set, view, self.sampler);
        Ok(set)
    }

    /// Record an upload of tightly packed `pixels` into a (new or reused)
    /// sampled image, leaving it in `SHADER_READ_ONLY_OPTIMAL`.
    unsafe fn upload_texture(
        &mut self,
        format: vk::Format,
        width: u32,
        height: u32,
        pixels: &[u8],
        reuse: Option<&Image>,
    ) -> Result<Image> {
        let image = match reuse {
            Some(i) => Image { ..*i },
            None => Image::new(
                &self.gpu,
                format,
                width,
                height,
                vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
            )?,
        };
        let (buf, off, ptr) = self.stage(pixels.len())?;
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), ptr, pixels.len());
        let cmd = self.cmd();
        barrier(
            &self.gpu,
            cmd,
            &[Transition::new(
                image.image,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            )
            .src(
                vk::PipelineStageFlags2::FRAGMENT_SHADER,
                vk::AccessFlags2::NONE,
            )
            .dst(
                vk::PipelineStageFlags2::COPY,
                vk::AccessFlags2::TRANSFER_WRITE,
            )],
        );
        let region = vk::BufferImageCopy {
            buffer_offset: off,
            buffer_row_length: 0,
            buffer_image_height: 0,
            image_subresource: vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            },
            image_offset: vk::Offset3D::default(),
            image_extent: vk::Extent3D {
                width,
                height,
                depth: 1,
            },
        };
        self.gpu.device.cmd_copy_buffer_to_image(
            cmd,
            buf,
            image.image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &[region],
        );
        barrier(
            &self.gpu,
            cmd,
            &[Transition::new(
                image.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            )
            .src(
                vk::PipelineStageFlags2::COPY,
                vk::AccessFlags2::TRANSFER_WRITE,
            )
            .dst(
                vk::PipelineStageFlags2::FRAGMENT_SHADER,
                vk::AccessFlags2::SHADER_SAMPLED_READ,
            )],
        );
        Ok(image)
    }

    // ---------------------------------------------------------------- video

    /// Hand the renderer the newest decoded frame. DMA-BUF frames are
    /// imported (once per decoder buffer) and sampled in place; CPU frames
    /// are copied through the staging buffer. Conversion happens in
    /// [`Renderer::render_eyes`].
    pub fn upload_video_frame(&mut self, frame: &VideoFrame<'_>) -> Result<()> {
        self.require_frame()?;
        let (w, h) = frame.size();
        if w == 0 || h == 0 {
            return Err(GfxError::InvalidFrame("empty frame".into()));
        }
        unsafe {
            self.ensure_output(w, h)?;
            match frame {
                VideoFrame::DmaBuf(f) => self.use_dmabuf(f)?,
                VideoFrame::Cpu(f) => self.upload_cpu(f)?,
            }
        }
        self.video.format = frame.format();
        self.video.color = frame.color();
        self.video.width = w;
        self.video.height = h;
        self.video.dirty = true;
        Ok(())
    }

    /// Stop showing the current video (e.g. playback closed): the
    /// projection pass draws nothing until the next upload. GPU resources
    /// and the DMA-BUF import cache are kept for reuse.
    pub fn clear_video(&mut self) {
        self.video.source = Source::None;
        self.video.dirty = false;
    }

    /// Drop the cached import of a decoder buffer (call when the decoder
    /// frees or reallocates it).
    pub fn forget_dmabuf(&mut self, buffer_id: u64) {
        if let Some(f) = self.imports.remove(&buffer_id) {
            f.into_garbage(&mut self.slots[self.slot].garbage);
            if self.video.source == Source::DmaBuf(buffer_id) {
                self.video.source = Source::None;
            }
        }
    }

    /// (Re)create the RGBA16F conversion target. A size change happens once
    /// per opened video, so it simply waits for the GPU and rebuilds.
    unsafe fn ensure_output(&mut self, w: u32, h: u32) -> Result<()> {
        if let Some(o) = &self.video.output {
            if o.width == w && o.height == h {
                return Ok(());
            }
        }
        self.gpu.device.device_wait_idle()?;
        let mut trash = Vec::new();
        if let Some(o) = self.video.output.take() {
            trash.push(o.into_garbage());
        }
        for (_, f) in self.imports.drain() {
            f.into_garbage(&mut trash);
        }
        if let Some(c) = self.cpu_planes.take() {
            trash.push(Garbage::DescriptorSet(c.set));
            let [a, b] = c.planes;
            trash.extend([a.into_garbage(), b.into_garbage()]);
        }
        for g in trash {
            g.destroy(&self.gpu, self.desc_pool);
        }
        let out = Image::new(
            &self.gpu,
            vk::Format::R16G16B16A16_SFLOAT,
            w,
            h,
            vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED,
        )?;
        if self.video.output_set == vk::DescriptorSet::null() {
            self.video.output_set = self.alloc_set(self.pipelines.tex_dsl)?;
        }
        video::write_tex_set(&self.gpu, self.video.output_set, out.view, self.sampler);
        self.video.output = Some(out);
        self.video.source = Source::None;
        self.video.last_push = None;
        Ok(())
    }

    unsafe fn use_dmabuf(&mut self, f: &DmaBufFrame) -> Result<()> {
        if !self.gpu.caps.zero_copy() {
            return Err(GfxError::Unsupported(
                "DMA-BUF import extensions not enabled".into(),
            ));
        }
        if self
            .imports
            .get(&f.buffer_id)
            .is_some_and(|e| !e.matches(f))
        {
            self.forget_dmabuf(f.buffer_id);
        }
        if !self.imports.contains_key(&f.buffer_id) {
            let planes = video::import_frame(&self.gpu, f)?;
            let set = self.alloc_set(self.pipelines.yuv_dsl)?;
            let out = self
                .video
                .output
                .as_ref()
                .map(|o| o.view)
                .unwrap_or_default();
            video::write_yuv_set(
                &self.gpu,
                set,
                planes[0].view,
                planes[1].view,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                self.sampler,
                out,
            );
            self.imports.insert(
                f.buffer_id,
                ImportedFrame {
                    desc: *f,
                    planes,
                    set,
                    transitioned: false,
                    last_used: self.frame_no,
                },
            );
            // Bound the cache: evict the least recently used other buffer.
            while self.imports.len() > self.config.dmabuf_cache_size.max(2) {
                let victim = self
                    .imports
                    .iter()
                    .filter(|(&k, _)| k != f.buffer_id)
                    .min_by_key(|(_, e)| e.last_used)
                    .map(|(&k, _)| k);
                match victim {
                    Some(k) => self.forget_dmabuf(k),
                    None => break,
                }
            }
        }
        let e = self.imports.get_mut(&f.buffer_id).expect("just inserted");
        e.desc = *f;
        e.last_used = self.frame_no;
        self.video.source = Source::DmaBuf(f.buffer_id);
        Ok(())
    }

    unsafe fn upload_cpu(&mut self, f: &CpuFrame<'_>) -> Result<()> {
        f.validate().map_err(GfxError::InvalidFrame)?;
        let formats = video::plane_formats(f.format);
        let reuse = self
            .cpu_planes
            .as_ref()
            .is_some_and(|c| c.format == f.format && c.width == f.width && c.height == f.height);
        if !reuse {
            if let Some(c) = self.cpu_planes.take() {
                let g = &mut self.slots[self.slot].garbage;
                g.push(Garbage::DescriptorSet(c.set));
                let [a, b] = c.planes;
                g.extend([a.into_garbage(), b.into_garbage()]);
            }
            let usage = vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST;
            let (cw, ch) = f.format.plane_extent(1, f.width, f.height);
            let y = Image::new(&self.gpu, formats[0], f.width, f.height, usage)?;
            let c = Image::new(&self.gpu, formats[1], cw, ch, usage)?;
            let set = self.alloc_set(self.pipelines.yuv_dsl)?;
            let out = self
                .video
                .output
                .as_ref()
                .map(|o| o.view)
                .unwrap_or_default();
            video::write_yuv_set(
                &self.gpu,
                set,
                y.view,
                c.view,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                self.sampler,
                out,
            );
            self.cpu_planes = Some(CpuPlanes {
                format: f.format,
                width: f.width,
                height: f.height,
                planes: [y, c],
                set,
            });
        }
        let texel = video::plane_texel_bytes(f.format);
        let mut regions = [vk::BufferImageCopy::default(); 2];
        let mut buffers = [vk::Buffer::null(); 2];
        for p in 0..2 {
            let (w, h) = f.format.plane_extent(p, f.width, f.height);
            let row = f.format.plane_row_bytes(p, f.width);
            let stride = f.strides[p];
            let src = f.planes[p];
            let (row_len, bytes) = if stride.is_multiple_of(texel[p]) {
                ((stride / texel[p]) as u32, stride * (h as usize - 1) + row)
            } else {
                (0, row * h as usize)
            };
            let (buf, off, ptr) = self.stage(bytes)?;
            if row_len != 0 {
                std::ptr::copy_nonoverlapping(src.as_ptr(), ptr, bytes);
            } else {
                for r in 0..h as usize {
                    std::ptr::copy_nonoverlapping(
                        src.as_ptr().add(r * stride),
                        ptr.add(r * row),
                        row,
                    );
                }
            }
            buffers[p] = buf;
            regions[p] = vk::BufferImageCopy {
                buffer_offset: off,
                buffer_row_length: row_len,
                buffer_image_height: 0,
                image_subresource: vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                },
                image_offset: vk::Offset3D::default(),
                image_extent: vk::Extent3D {
                    width: w,
                    height: h,
                    depth: 1,
                },
            };
        }
        let planes = &self.cpu_planes.as_ref().expect("created above").planes;
        let imgs = [planes[0].image, planes[1].image];
        let cmd = self.cmd();
        let to_dst = |i: vk::Image| {
            Transition::new(
                i,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            )
            .src(
                vk::PipelineStageFlags2::COMPUTE_SHADER,
                vk::AccessFlags2::NONE,
            )
            .dst(
                vk::PipelineStageFlags2::COPY,
                vk::AccessFlags2::TRANSFER_WRITE,
            )
        };
        barrier(&self.gpu, cmd, &[to_dst(imgs[0]), to_dst(imgs[1])]);
        for p in 0..2 {
            self.gpu.device.cmd_copy_buffer_to_image(
                cmd,
                buffers[p],
                imgs[p],
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &regions[p..p + 1],
            );
        }
        let to_read = |i: vk::Image| {
            Transition::new(
                i,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            )
            .src(
                vk::PipelineStageFlags2::COPY,
                vk::AccessFlags2::TRANSFER_WRITE,
            )
            .dst(
                vk::PipelineStageFlags2::COMPUTE_SHADER,
                vk::AccessFlags2::SHADER_SAMPLED_READ,
            )
        };
        barrier(&self.gpu, cmd, &[to_read(imgs[0]), to_read(imgs[1])]);
        self.video.source = Source::Cpu;
        Ok(())
    }

    /// Record the YUV → RGBA16F compute pass.
    unsafe fn convert(&mut self, push: &ColorPush) {
        let Some(out) = &self.video.output else {
            return;
        };
        let (out_img, w, h) = (out.image, out.width, out.height);
        let ours = self.gpu.queue_family_index;
        let foreign = if self.gpu.caps.queue_family_foreign {
            vk::QUEUE_FAMILY_FOREIGN_EXT
        } else {
            vk::QUEUE_FAMILY_EXTERNAL
        };
        let (set, dmabuf) = match self.video.source {
            Source::None => return,
            Source::Cpu => match &self.cpu_planes {
                Some(c) => (c.set, None),
                None => return,
            },
            Source::DmaBuf(id) => match self.imports.get_mut(&id) {
                Some(e) => {
                    let old = if e.transitioned {
                        vk::ImageLayout::GENERAL
                    } else {
                        vk::ImageLayout::PREINITIALIZED
                    };
                    e.transitioned = true;
                    (e.set, Some(([e.planes[0].image, e.planes[1].image], old)))
                }
                None => return,
            },
        };
        let cmd = self.cmd();
        let d = &self.gpu.device;
        let out_to_general = Transition::new(
            out_img,
            vk::ImageLayout::UNDEFINED,
            vk::ImageLayout::GENERAL,
        )
        .src(
            vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::NONE,
        )
        .dst(
            vk::PipelineStageFlags2::COMPUTE_SHADER,
            vk::AccessFlags2::SHADER_STORAGE_WRITE,
        );
        match dmabuf {
            // Acquire the decoder's buffer from the foreign queue family.
            // [verify] Turnip honours FOREIGN acquire + PREINITIALIZED/GENERAL
            // without discarding decoder output (no implicit-sync wait here:
            // the decoder must have finished the frame before handing it over).
            Some((planes, old)) => {
                let acquire = |i| {
                    Transition::new(i, old, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                        .src(vk::PipelineStageFlags2::NONE, vk::AccessFlags2::NONE)
                        .dst(
                            vk::PipelineStageFlags2::COMPUTE_SHADER,
                            vk::AccessFlags2::SHADER_SAMPLED_READ,
                        )
                        .queues(foreign, ours)
                };
                barrier(
                    &self.gpu,
                    cmd,
                    &[out_to_general, acquire(planes[0]), acquire(planes[1])],
                );
            }
            None => barrier(&self.gpu, cmd, &[out_to_general]),
        }
        d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.pipelines.yuv);
        d.cmd_bind_descriptor_sets(
            cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.pipelines.yuv_layout,
            0,
            &[set],
            &[],
        );
        d.cmd_push_constants(
            cmd,
            self.pipelines.yuv_layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            bytemuck::bytes_of(push),
        );
        d.cmd_dispatch(cmd, w.div_ceil(8), h.div_ceil(8), 1);
        let out_to_read = Transition::new(
            out_img,
            vk::ImageLayout::GENERAL,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        )
        .src(
            vk::PipelineStageFlags2::COMPUTE_SHADER,
            vk::AccessFlags2::SHADER_STORAGE_WRITE,
        )
        .dst(
            vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
        );
        match dmabuf {
            Some((planes, _)) => {
                let release = |i| {
                    Transition::new(
                        i,
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                        vk::ImageLayout::GENERAL,
                    )
                    .src(
                        vk::PipelineStageFlags2::COMPUTE_SHADER,
                        vk::AccessFlags2::NONE,
                    )
                    .dst(vk::PipelineStageFlags2::NONE, vk::AccessFlags2::NONE)
                    .queues(ours, foreign)
                };
                barrier(
                    &self.gpu,
                    cmd,
                    &[out_to_read, release(planes[0]), release(planes[1])],
                );
            }
            None => barrier(&self.gpu, cmd, &[out_to_read]),
        }
    }

    /// Run the conversion if a new frame arrived or the colour parameters
    /// changed. Called by `render_eyes`; exposed for UI-only frames that show
    /// the video as a texture.
    pub fn convert_video(&mut self, corrections: &Corrections) -> Result<()> {
        self.require_frame()?;
        if self.video.source == Source::None {
            return Ok(());
        }
        let push = ColorPush::new(
            &self.video.color,
            self.video.format.storage(),
            &self.display,
            corrections,
        );
        if self.video.dirty || self.video.last_push != Some(push) {
            unsafe { self.convert(&push) };
            self.video.dirty = false;
            self.video.last_push = Some(push);
        }
        Ok(())
    }

    // ---------------------------------------------------------------- eyes

    /// Eye-image aspect for the current video and stereo layout.
    fn eye_aspect(&self, settings: &ViewSettings) -> f32 {
        if self.video.width == 0 {
            return 16.0 / 9.0;
        }
        let r = settings.stereo.eye_rect(0, false);
        (self.video.width as f32 * (r[2] - r[0])) / (self.video.height as f32 * (r[3] - r[1]))
    }

    unsafe fn ensure_mesh(&mut self, projection: &Projection, aspect: f32) -> Result<()> {
        let key = MeshKey {
            projection: projection.clone(),
            aspect_milli: (aspect * 1000.0).round() as u32,
        };
        if self.mesh.as_ref().is_some_and(|m| m.key == key) {
            return Ok(());
        }
        let dir = self.config.mesh_dir.clone();
        let mesh = match build_mesh(projection, aspect, self.config.mesh_density, |p| {
            std::fs::read_to_string(dir.join(p))
        }) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("projection mesh failed ({e}); falling back to 180° equirect");
                build_mesh(
                    &Projection::EQUIRECT_180,
                    aspect,
                    self.config.mesh_density,
                    |_| unreachable!(),
                )?
            }
        };
        self.set_mesh(key, &mesh)
    }

    unsafe fn set_mesh(&mut self, key: MeshKey, mesh: &Mesh) -> Result<()> {
        let vbytes: &[u8] = bytemuck::cast_slice(&mesh.vertices);
        let ibytes: &[u8] = bytemuck::cast_slice(&mesh.indices);
        let vb = Buffer::new(
            &self.gpu,
            vbytes.len() as u64,
            vk::BufferUsageFlags::VERTEX_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
            false,
        )?;
        let ib = Buffer::new(
            &self.gpu,
            ibytes.len() as u64,
            vk::BufferUsageFlags::INDEX_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
            false,
        )?;
        let cmd = self.cmd();
        for (dst, bytes) in [(vb.buffer, vbytes), (ib.buffer, ibytes)] {
            let (src, off, ptr) = self.stage(bytes.len())?;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
            let region = vk::BufferCopy {
                src_offset: off,
                dst_offset: 0,
                size: bytes.len() as u64,
            };
            self.gpu.device.cmd_copy_buffer(cmd, src, dst, &[region]);
        }
        let mb = [vk::MemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COPY)
            .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::VERTEX_INPUT)
            .dst_access_mask(
                vk::AccessFlags2::VERTEX_ATTRIBUTE_READ | vk::AccessFlags2::INDEX_READ,
            )];
        self.gpu
            .device
            .cmd_pipeline_barrier2(cmd, &vk::DependencyInfo::default().memory_barriers(&mb));
        if let Some(old) = self.mesh.take() {
            self.slots[self.slot]
                .garbage
                .extend([old.vb.into_garbage(), old.ib.into_garbage()]);
        }
        self.mesh = Some(GpuMesh {
            key,
            vb,
            ib,
            index_count: mesh.indices.len() as u32,
        });
        Ok(())
    }

    /// Replace the projection mesh with a caller-built one (e.g. a custom
    /// mesh loaded off the render thread). It stays until the projection or
    /// eye aspect changes.
    pub fn set_projection_mesh(
        &mut self,
        projection: &Projection,
        eye_aspect: f32,
        mesh: &Mesh,
    ) -> Result<()> {
        self.require_frame()?;
        let key = MeshKey {
            projection: projection.clone(),
            aspect_milli: (eye_aspect * 1000.0).round() as u32,
        };
        unsafe { self.set_mesh(key, mesh) }
    }

    unsafe fn target_view(&mut self, t: &RenderTarget) -> Result<vk::ImageView> {
        let key = (t.image, t.array_layer, t.format);
        if let Some(&v) = self.target_views.get(&key) {
            return Ok(v);
        }
        let v = create_view(&self.gpu, t.image, t.format, t.array_layer)?;
        self.target_views.insert(key, v);
        Ok(v)
    }

    /// Destroy cached views of XR swapchain images (call before destroying
    /// or recreating swapchains). Waits for the GPU.
    pub fn forget_targets(&mut self) -> Result<()> {
        unsafe {
            self.gpu.device.device_wait_idle()?;
            for (_, v) in self.target_views.drain() {
                self.gpu.device.destroy_image_view(v, None);
            }
        }
        Ok(())
    }

    unsafe fn begin_target(&mut self, t: &RenderTarget, clear: [f32; 4]) -> Result<()> {
        let view = self.target_view(t)?;
        let cmd = self.cmd();
        barrier(
            &self.gpu,
            cmd,
            &[Transition::new(
                t.image,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            )
            .layer(t.array_layer)
            .src(
                vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                vk::AccessFlags2::NONE,
            )
            .dst(
                vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
                vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            )],
        );
        let att = [vk::RenderingAttachmentInfo::default()
            .image_view(view)
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .clear_value(vk::ClearValue {
                color: vk::ClearColorValue { float32: clear },
            })];
        let area = vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent: vk::Extent2D {
                width: t.width,
                height: t.height,
            },
        };
        let info = vk::RenderingInfo::default()
            .render_area(area)
            .layer_count(1)
            .color_attachments(&att);
        let d = &self.gpu.device;
        d.cmd_begin_rendering(cmd, &info);
        let vp = vk::Viewport {
            x: 0.0,
            y: 0.0,
            width: t.width as f32,
            height: t.height as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        };
        d.cmd_set_viewport(cmd, 0, &[vp]);
        d.cmd_set_scissor(cmd, 0, &[area]);
        Ok(())
    }

    /// Convert the video if needed and draw both eyes.
    pub fn render_eyes(&mut self, req: &EyeRenderRequest<'_>) -> Result<()> {
        self.require_frame()?;
        let aspect = self.eye_aspect(req.settings);
        unsafe {
            self.ensure_mesh(&req.settings.projection, aspect)?;
            self.convert_video(req.corrections)?;
            let immersive = req.settings.projection.is_immersive();
            for eye in 0..2 {
                let t = req.targets[eye];
                let pipeline = self.pipelines.projection(&self.gpu, t.format)?;
                self.begin_target(&t, req.clear_color)?;
                let cmd = self.cmd();
                let d = &self.gpu.device;
                if let (Some(mesh), true) = (&self.mesh, self.video_ready()) {
                    let v = &req.views[eye];
                    let view_proj = projection_matrix(v.fov, self.config.near_m, self.config.far_m)
                        * v.view_matrix(immersive && req.follow_head);
                    let push = EyePush::new(
                        view_proj,
                        req.model,
                        req.settings.stereo,
                        req.settings.swap_eyes,
                        eye,
                        req.corrections,
                        needs_shader_encode(t.format),
                        req.tint,
                    );
                    d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, pipeline);
                    d.cmd_bind_descriptor_sets(
                        cmd,
                        vk::PipelineBindPoint::GRAPHICS,
                        self.pipelines.proj_layout,
                        0,
                        &[self.video.output_set],
                        &[],
                    );
                    d.cmd_bind_vertex_buffers(cmd, 0, &[mesh.vb.buffer], &[0]);
                    d.cmd_bind_index_buffer(cmd, mesh.ib.buffer, 0, vk::IndexType::UINT32);
                    d.cmd_push_constants(
                        cmd,
                        self.pipelines.proj_layout,
                        vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                        0,
                        bytemuck::bytes_of(&push),
                    );
                    d.cmd_draw_indexed(cmd, mesh.index_count, 1, 0, 0, 0);
                }
                d.cmd_end_rendering(cmd);
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------------- UI

    /// Upload (or replace) a thumbnail / image for `TextureId::Image(id)`.
    /// `rgba` is tightly packed sRGB RGBA8 with straight alpha. Outside a
    /// frame the upload is queued for the next `begin_frame`.
    pub fn upload_image(&mut self, id: u64, width: u32, height: u32, rgba: &[u8]) -> Result<()> {
        if width == 0 || height == 0 || rgba.len() < (width * height * 4) as usize {
            return Err(GfxError::InvalidFrame(format!(
                "image {id}: {} bytes for {width}x{height}",
                rgba.len()
            )));
        }
        if !self.in_frame {
            self.pending_images.push(PendingImage {
                id,
                width,
                height,
                rgba: rgba.to_vec(),
            });
            return Ok(());
        }
        unsafe {
            let bytes = &rgba[..(width * height * 4) as usize];
            let img = self.upload_texture(vk::Format::R8G8B8A8_SRGB, width, height, bytes, None)?;
            let set = self.alloc_tex_set(img.view)?;
            let mut evicted = std::mem::take(&mut self.evicted);
            self.images.insert(
                id,
                Texture { image: img, set },
                bytes.len(),
                self.frame_no,
                &mut evicted,
            );
            let g = &mut self.slots[self.slot].garbage;
            for t in evicted.drain(..) {
                t.into_garbage(g);
            }
            self.evicted = evicted;
        }
        Ok(())
    }

    /// Bytes held by the thumbnail cache.
    pub fn image_cache_bytes(&self) -> usize {
        self.images.used_bytes()
    }

    pub fn has_image(&self, id: u64) -> bool {
        self.images.contains(id) || self.pending_images.iter().any(|p| p.id == id)
    }

    pub fn remove_image(&mut self, id: u64) {
        self.pending_images.retain(|p| p.id != id);
        if let Some(t) = self.images.remove(id) {
            t.into_garbage(&mut self.slots[self.slot].garbage);
        }
    }

    unsafe fn sync_atlas(&mut self, atlas: &AtlasImage) -> Result<()> {
        if atlas.width == 0
            || atlas.height == 0
            || atlas.pixels.len() < (atlas.width * atlas.height) as usize
        {
            return Ok(());
        }
        if self
            .atlas
            .as_ref()
            .is_some_and(|(_, v)| *v == atlas.version)
        {
            return Ok(());
        }
        let pixels = &atlas.pixels[..(atlas.width * atlas.height) as usize];
        let same_size = self
            .atlas
            .as_ref()
            .is_some_and(|(t, _)| t.image.width == atlas.width && t.image.height == atlas.height);
        if same_size {
            let (tex, _) = self.atlas.take().expect("checked");
            let img = self.upload_texture(
                vk::Format::R8_UNORM,
                atlas.width,
                atlas.height,
                pixels,
                Some(&tex.image),
            )?;
            self.atlas = Some((
                Texture {
                    image: img,
                    set: tex.set,
                },
                atlas.version,
            ));
        } else {
            if let Some((t, _)) = self.atlas.take() {
                t.into_garbage(&mut self.slots[self.slot].garbage);
            }
            let img = self.upload_texture(
                vk::Format::R8_UNORM,
                atlas.width,
                atlas.height,
                pixels,
                None,
            )?;
            let set = self.alloc_tex_set(img.view)?;
            self.atlas = Some((Texture { image: img, set }, atlas.version));
        }
        Ok(())
    }

    /// Write `bytes` into this frame's UI vertex or index buffer, growing it
    /// if needed. Returns `(buffer, offset)`.
    unsafe fn ui_write(&mut self, index: bool, bytes: &[u8]) -> Result<(vk::Buffer, u64)> {
        let s = &mut self.slots[self.slot];
        let (buf, used, usage) = if index {
            (
                &mut s.ui_ib,
                &mut s.ui_ib_used,
                vk::BufferUsageFlags::INDEX_BUFFER,
            )
        } else {
            (
                &mut s.ui_vb,
                &mut s.ui_vb_used,
                vk::BufferUsageFlags::VERTEX_BUFFER,
            )
        };
        let mut off = align_up(*used, 16);
        if off + bytes.len() as u64 > buf.size {
            let size = (buf.size * 2)
                .max(bytes.len() as u64 * 2)
                .max(MIN_UI_BUFFER);
            let new = Buffer::new(&self.gpu, size, usage, true)?;
            s.garbage.push(std::mem::replace(buf, new).into_garbage());
            off = 0;
        }
        buf.write(off as usize, bytes);
        *used = off + bytes.len() as u64;
        Ok((buf.buffer, off))
    }

    /// Rasterize a draw list into a UI layer image (premultiplied alpha;
    /// submit the layer without `UNPREMULTIPLIED_ALPHA`).
    pub fn render_ui(&mut self, req: &UiRenderRequest<'_>) -> Result<()> {
        self.require_frame()?;
        let dl = req.draw_list;
        let t = req.target;
        unsafe {
            if let Some(a) = req.atlas {
                self.sync_atlas(a)?;
            }
            let pipeline = self.pipelines.ui(&self.gpu, t.format)?;
            let draw = !dl.cmds.is_empty() && !dl.indices.is_empty() && !dl.vertices.is_empty();
            let buffers = if draw {
                // SAFETY: `Vertex` is `repr(C)` with eight f32 fields and no padding.
                let vbytes = std::slice::from_raw_parts(
                    dl.vertices.as_ptr() as *const u8,
                    std::mem::size_of_val(dl.vertices.as_slice()),
                );
                let (vb, voff) = self.ui_write(false, vbytes)?;
                let (ib, ioff) = self.ui_write(true, bytemuck::cast_slice(&dl.indices))?;
                Some((vb, voff, ib, ioff))
            } else {
                None
            };
            self.begin_target(&t, req.clear_color)?;
            let cmd = self.cmd();
            if let Some((vb, voff, ib, ioff)) = buffers {
                let d = &self.gpu.device;
                let panel = [req.panel_size[0].max(1.0), req.panel_size[1].max(1.0)];
                let sx = t.width as f32 / panel[0];
                let sy = t.height as f32 / panel[1];
                let flags = needs_shader_encode(t.format) as u32;
                d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, pipeline);
                d.cmd_bind_vertex_buffers(cmd, 0, &[vb], &[voff]);
                d.cmd_bind_index_buffer(cmd, ib, ioff, vk::IndexType::UINT32);
                let stages = vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT;
                let index_total = dl.indices.len() as u32;
                for c in &dl.cmds {
                    let (set, mode) = match c.texture {
                        TextureId::White => (self.white.as_ref().map(|w| w.set), 0),
                        TextureId::FontAtlas => (self.atlas.as_ref().map(|(a, _)| a.set), 1),
                        TextureId::Image(id) => {
                            (self.images.touch(id, self.frame_no).map(|i| i.set), 2)
                        }
                        TextureId::Video => {
                            (self.video_ready().then_some(self.video.output_set), 3)
                        }
                    };
                    let Some(set) = set else { continue };
                    if c.index_count == 0
                        || c.first_index.saturating_add(c.index_count) > index_total
                    {
                        continue;
                    }
                    let Some(scissor) = clip_to_scissor(c.clip, sx, sy, t.width, t.height) else {
                        continue;
                    };
                    d.cmd_set_scissor(cmd, 0, &[scissor]);
                    d.cmd_bind_descriptor_sets(
                        cmd,
                        vk::PipelineBindPoint::GRAPHICS,
                        self.pipelines.ui_layout,
                        0,
                        &[set],
                        &[],
                    );
                    let push = UiPush {
                        scale: [2.0 / panel[0], 2.0 / panel[1]],
                        mode,
                        flags,
                    };
                    d.cmd_push_constants(
                        cmd,
                        self.pipelines.ui_layout,
                        stages,
                        0,
                        bytemuck::bytes_of(&push),
                    );
                    d.cmd_draw_indexed(cmd, c.index_count, 1, c.first_index, 0, 0);
                }
            }
            self.gpu.device.cmd_end_rendering(cmd);
        }
        Ok(())
    }
}

/// Panel-pixel clip rect → integer scissor in target pixels (None if empty).
pub(crate) fn clip_to_scissor(
    clip: [f32; 4],
    sx: f32,
    sy: f32,
    w: u32,
    h: u32,
) -> Option<vk::Rect2D> {
    let x0 = (clip[0] * sx).floor().clamp(0.0, w as f32);
    let y0 = (clip[1] * sy).floor().clamp(0.0, h as f32);
    let x1 = ((clip[0] + clip[2]) * sx).ceil().clamp(0.0, w as f32);
    let y1 = ((clip[1] + clip[3]) * sy).ceil().clamp(0.0, h as f32);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(vk::Rect2D {
        offset: vk::Offset2D {
            x: x0 as i32,
            y: y0 as i32,
        },
        extent: vk::Extent2D {
            width: (x1 - x0) as u32,
            height: (y1 - y0) as u32,
        },
    })
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            let gpu = &self.gpu;
            let d = &gpu.device;
            let _ = d.device_wait_idle();
            let mut trash: Vec<Garbage> = Vec::new();
            for s in &mut self.slots {
                trash.append(&mut s.garbage);
            }
            for (_, f) in self.imports.drain() {
                f.into_garbage(&mut trash);
            }
            if let Some(c) = self.cpu_planes.take() {
                let [a, b] = c.planes;
                trash.extend([a.into_garbage(), b.into_garbage()]);
            }
            if let Some(o) = self.video.output.take() {
                trash.push(o.into_garbage());
            }
            if let Some(m) = self.mesh.take() {
                trash.extend([m.vb.into_garbage(), m.ib.into_garbage()]);
            }
            if let Some(w) = self.white.take() {
                trash.push(w.image.into_garbage());
            }
            if let Some((a, _)) = self.atlas.take() {
                trash.push(a.image.into_garbage());
            }
            for t in self.images.drain() {
                trash.push(t.image.into_garbage());
            }
            for (_, v) in self.target_views.drain() {
                trash.push(Garbage::View(v));
            }
            for s in self.slots.drain(..) {
                trash.extend([
                    s.staging.into_garbage(),
                    s.ui_vb.into_garbage(),
                    s.ui_ib.into_garbage(),
                ]);
                d.destroy_fence(s.fence, None);
            }
            for g in trash {
                // Descriptor sets die with the pool below.
                if !matches!(g, Garbage::DescriptorSet(_)) {
                    g.destroy(gpu, self.desc_pool);
                }
            }
            d.destroy_descriptor_pool(self.desc_pool, None);
            d.destroy_command_pool(self.cmd_pool, None);
            d.destroy_sampler(self.sampler, None);
            self.pipelines.destroy(gpu);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scissor_scaling_and_clamping() {
        let r = clip_to_scissor([10.0, 20.0, 100.0, 50.0], 2.0, 2.0, 1000, 1000).unwrap();
        assert_eq!(
            (r.offset.x, r.offset.y, r.extent.width, r.extent.height),
            (20, 40, 200, 100)
        );
        let r = clip_to_scissor([-10.0, -10.0, 5000.0, 5000.0], 1.0, 1.0, 512, 256).unwrap();
        assert_eq!(
            (r.offset.x, r.offset.y, r.extent.width, r.extent.height),
            (0, 0, 512, 256)
        );
        assert!(clip_to_scissor([600.0, 0.0, 10.0, 10.0], 1.0, 1.0, 512, 256).is_none());
        assert!(clip_to_scissor([0.0, 0.0, 0.0, 10.0], 1.0, 1.0, 512, 256).is_none());
    }

    #[test]
    fn ui_vertex_layout_matches_pipeline() {
        assert_eq!(std::mem::size_of::<fp_core::draw::Vertex>(), 32);
        assert_eq!(std::mem::offset_of!(fp_core::draw::Vertex, uv), 8);
        assert_eq!(std::mem::offset_of!(fp_core::draw::Vertex, color), 16);
        assert_eq!(std::mem::size_of::<crate::mesh::MeshVertex>(), 20);
    }
}
