//! Descriptor layouts, pipeline layouts and pipelines. Graphics pipelines use
//! dynamic rendering and are created lazily per colour-attachment format
//! (XR swapchain formats are only known after session creation).

use super::{GpuContext, Result, UiPush};
use crate::color::ColorPush;
use crate::correction::EyePush;
use crate::mesh::MeshVertex;
use crate::shaders;
use ash::vk;
use std::collections::HashMap;
use std::mem::size_of;

pub(crate) struct Pipelines {
    /// Compute: luma, chroma, sampler, storage output.
    pub yuv_dsl: vk::DescriptorSetLayout,
    /// Graphics: one sampled texture + sampler.
    pub tex_dsl: vk::DescriptorSetLayout,
    pub yuv_layout: vk::PipelineLayout,
    pub proj_layout: vk::PipelineLayout,
    pub ui_layout: vk::PipelineLayout,
    pub yuv: vk::Pipeline,
    proj_module: vk::ShaderModule,
    ui_module: vk::ShaderModule,
    proj: HashMap<vk::Format, vk::Pipeline>,
    ui: HashMap<vk::Format, vk::Pipeline>,
}

unsafe fn module(gpu: &GpuContext, spv: &[u8]) -> Result<vk::ShaderModule> {
    let words = shaders::words(spv);
    Ok(gpu
        .device
        .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)?)
}

unsafe fn dsl(
    gpu: &GpuContext,
    types: &[vk::DescriptorType],
    stages: vk::ShaderStageFlags,
) -> Result<vk::DescriptorSetLayout> {
    let bindings: Vec<_> = types
        .iter()
        .enumerate()
        .map(|(i, &t)| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(i as u32)
                .descriptor_type(t)
                .descriptor_count(1)
                .stage_flags(stages)
        })
        .collect();
    Ok(gpu.device.create_descriptor_set_layout(
        &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
        None,
    )?)
}

unsafe fn layout(
    gpu: &GpuContext,
    set: vk::DescriptorSetLayout,
    stages: vk::ShaderStageFlags,
    push: usize,
) -> Result<vk::PipelineLayout> {
    let ranges = [vk::PushConstantRange {
        stage_flags: stages,
        offset: 0,
        size: push as u32,
    }];
    let sets = [set];
    let info = vk::PipelineLayoutCreateInfo::default()
        .set_layouts(&sets)
        .push_constant_ranges(&ranges);
    Ok(gpu.device.create_pipeline_layout(&info, None)?)
}

impl Pipelines {
    pub unsafe fn new(gpu: &GpuContext) -> Result<Pipelines> {
        use vk::DescriptorType as T;
        let d = &gpu.device;
        let yuv_dsl = dsl(
            gpu,
            &[
                T::SAMPLED_IMAGE,
                T::SAMPLED_IMAGE,
                T::SAMPLER,
                T::STORAGE_IMAGE,
            ],
            vk::ShaderStageFlags::COMPUTE,
        )?;
        let gfx_stages = vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT;
        let tex_dsl = dsl(
            gpu,
            &[T::SAMPLED_IMAGE, T::SAMPLER],
            vk::ShaderStageFlags::FRAGMENT,
        )?;
        let yuv_layout = layout(
            gpu,
            yuv_dsl,
            vk::ShaderStageFlags::COMPUTE,
            size_of::<ColorPush>(),
        )?;
        let proj_layout = layout(gpu, tex_dsl, gfx_stages, size_of::<EyePush>())?;
        let ui_layout = layout(gpu, tex_dsl, gfx_stages, size_of::<UiPush>())?;

        let yuv_module = module(gpu, shaders::YUV_SPV)?;
        let stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(yuv_module)
            .name(shaders::ENTRY_COMPUTE);
        let info = vk::ComputePipelineCreateInfo::default()
            .stage(stage)
            .layout(yuv_layout);
        let yuv = d
            .create_compute_pipelines(vk::PipelineCache::null(), &[info], None)
            .map_err(|(_, e)| e)?[0];
        d.destroy_shader_module(yuv_module, None);

        Ok(Pipelines {
            yuv_dsl,
            tex_dsl,
            yuv_layout,
            proj_layout,
            ui_layout,
            yuv,
            proj_module: module(gpu, shaders::PROJECTION_SPV)?,
            ui_module: module(gpu, shaders::UI_SPV)?,
            proj: HashMap::new(),
            ui: HashMap::new(),
        })
    }

    /// Projection pipeline for a colour format (no blending: the video layer
    /// is opaque except where masked, and masked texels are written as 0).
    pub unsafe fn projection(
        &mut self,
        gpu: &GpuContext,
        format: vk::Format,
    ) -> Result<vk::Pipeline> {
        if let Some(&p) = self.proj.get(&format) {
            return Ok(p);
        }
        let attrs = [
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
        ];
        let p = graphics(
            gpu,
            self.proj_module,
            self.proj_layout,
            format,
            size_of::<MeshVertex>() as u32,
            &attrs,
            false,
        )?;
        self.proj.insert(format, p);
        Ok(p)
    }

    /// UI pipeline for a colour format (premultiplied-alpha blending).
    pub unsafe fn ui(&mut self, gpu: &GpuContext, format: vk::Format) -> Result<vk::Pipeline> {
        if let Some(&p) = self.ui.get(&format) {
            return Ok(p);
        }
        let attrs = [
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
                format: vk::Format::R32G32B32A32_SFLOAT,
                offset: 16,
            },
        ];
        let stride = size_of::<fp_core::draw::Vertex>() as u32;
        let p = graphics(
            gpu,
            self.ui_module,
            self.ui_layout,
            format,
            stride,
            &attrs,
            true,
        )?;
        self.ui.insert(format, p);
        Ok(p)
    }

    pub unsafe fn destroy(&mut self, gpu: &GpuContext) {
        let d = &gpu.device;
        for (_, p) in self.proj.drain().chain(self.ui.drain()) {
            d.destroy_pipeline(p, None);
        }
        d.destroy_pipeline(self.yuv, None);
        d.destroy_shader_module(self.proj_module, None);
        d.destroy_shader_module(self.ui_module, None);
        d.destroy_pipeline_layout(self.yuv_layout, None);
        d.destroy_pipeline_layout(self.proj_layout, None);
        d.destroy_pipeline_layout(self.ui_layout, None);
        d.destroy_descriptor_set_layout(self.yuv_dsl, None);
        d.destroy_descriptor_set_layout(self.tex_dsl, None);
    }
}

unsafe fn graphics(
    gpu: &GpuContext,
    module: vk::ShaderModule,
    layout: vk::PipelineLayout,
    format: vk::Format,
    stride: u32,
    attrs: &[vk::VertexInputAttributeDescription],
    blend: bool,
) -> Result<vk::Pipeline> {
    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(module)
            .name(shaders::ENTRY_VERTEX),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(module)
            .name(shaders::ENTRY_FRAGMENT),
    ];
    let bindings = [vk::VertexInputBindingDescription {
        binding: 0,
        stride,
        input_rate: vk::VertexInputRate::VERTEX,
    }];
    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
        .vertex_binding_descriptions(&bindings)
        .vertex_attribute_descriptions(attrs);
    let ia = vk::PipelineInputAssemblyStateCreateInfo::default()
        .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
    let vp = vk::PipelineViewportStateCreateInfo::default()
        .viewport_count(1)
        .scissor_count(1);
    let rs = vk::PipelineRasterizationStateCreateInfo::default()
        .polygon_mode(vk::PolygonMode::FILL)
        .cull_mode(vk::CullModeFlags::NONE)
        .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
        .line_width(1.0);
    let ms = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);
    let att = [vk::PipelineColorBlendAttachmentState {
        blend_enable: blend as u32,
        src_color_blend_factor: vk::BlendFactor::ONE,
        dst_color_blend_factor: vk::BlendFactor::ONE_MINUS_SRC_ALPHA,
        color_blend_op: vk::BlendOp::ADD,
        src_alpha_blend_factor: vk::BlendFactor::ONE,
        dst_alpha_blend_factor: vk::BlendFactor::ONE_MINUS_SRC_ALPHA,
        alpha_blend_op: vk::BlendOp::ADD,
        color_write_mask: vk::ColorComponentFlags::RGBA,
    }];
    let cb = vk::PipelineColorBlendStateCreateInfo::default().attachments(&att);
    let dyn_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dy = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dyn_states);
    let formats = [format];
    let mut rendering =
        vk::PipelineRenderingCreateInfo::default().color_attachment_formats(&formats);
    let info = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&ia)
        .viewport_state(&vp)
        .rasterization_state(&rs)
        .multisample_state(&ms)
        .color_blend_state(&cb)
        .dynamic_state(&dy)
        .layout(layout)
        .push_next(&mut rendering);
    Ok(gpu
        .device
        .create_graphics_pipelines(vk::PipelineCache::null(), &[info], None)
        .map_err(|(_, e)| e)?[0])
}
