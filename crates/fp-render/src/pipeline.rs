//! Graphics pipeline construction (dynamic rendering, no render passes).

use crate::gpu::Gpu;
use crate::{Result, VkContext};
use ash::vk;

pub(crate) struct PipelineDesc<'a> {
    pub spv: &'a [u8],
    pub layout: vk::PipelineLayout,
    pub color_format: vk::Format,
    pub vertex_stride: u32,
    pub attributes: &'a [vk::VertexInputAttributeDescription],
    /// Premultiplied-alpha blending.
    pub blend: bool,
}

pub(crate) fn create(gpu: &Gpu, d: &PipelineDesc) -> Result<vk::Pipeline> {
    let module = gpu.shader(d.spv)?;
    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(module)
            .name(c"vs_main"),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(module)
            .name(c"fs_main"),
    ];
    let bindings = [vk::VertexInputBindingDescription::default()
        .binding(0)
        .stride(d.vertex_stride)
        .input_rate(vk::VertexInputRate::VERTEX)];
    let vertex_input = if d.vertex_stride > 0 {
        vk::PipelineVertexInputStateCreateInfo::default()
            .vertex_binding_descriptions(&bindings)
            .vertex_attribute_descriptions(d.attributes)
    } else {
        vk::PipelineVertexInputStateCreateInfo::default()
    };
    let ia = vk::PipelineInputAssemblyStateCreateInfo::default()
        .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
    let vp = vk::PipelineViewportStateCreateInfo::default()
        .viewport_count(1)
        .scissor_count(1);
    let rs = vk::PipelineRasterizationStateCreateInfo::default()
        .polygon_mode(vk::PolygonMode::FILL)
        .cull_mode(vk::CullModeFlags::NONE)
        .line_width(1.0);
    let ms = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);
    let att = [if d.blend {
        vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(true)
            .src_color_blend_factor(vk::BlendFactor::ONE)
            .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(vk::BlendFactor::ONE)
            .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .alpha_blend_op(vk::BlendOp::ADD)
            .color_write_mask(vk::ColorComponentFlags::RGBA)
    } else {
        vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA)
    }];
    let cb = vk::PipelineColorBlendStateCreateInfo::default().attachments(&att);
    let dyn_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dyn_states);
    let formats = [d.color_format];
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
        .dynamic_state(&dynamic)
        .layout(d.layout)
        .push_next(&mut rendering);
    // SAFETY: all referenced state lives until the call returns.
    let result = unsafe {
        gpu.device
            .create_graphics_pipelines(vk::PipelineCache::null(), &[info], None)
    };
    // SAFETY: the module is no longer needed once the pipeline exists.
    unsafe { gpu.device.destroy_shader_module(module, None) };
    result
        .map_err(|(_, e)| e)
        .ctx("create pipeline")
        .map(|p| p[0])
}

pub(crate) fn set_layout(
    gpu: &Gpu,
    bindings: &[(u32, vk::DescriptorType)],
) -> Result<vk::DescriptorSetLayout> {
    let b: Vec<_> = bindings
        .iter()
        .map(|&(i, t)| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(i)
                .descriptor_type(t)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT | vk::ShaderStageFlags::VERTEX)
        })
        .collect();
    // SAFETY: valid create info.
    unsafe {
        gpu.device.create_descriptor_set_layout(
            &vk::DescriptorSetLayoutCreateInfo::default().bindings(&b),
            None,
        )
    }
    .ctx("descriptor set layout")
}

pub(crate) fn layout(
    gpu: &Gpu,
    set: vk::DescriptorSetLayout,
    push_bytes: u32,
) -> Result<vk::PipelineLayout> {
    let sets = [set];
    let ranges = [vk::PushConstantRange::default()
        .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
        .offset(0)
        .size(push_bytes)];
    // SAFETY: valid create info.
    unsafe {
        gpu.device.create_pipeline_layout(
            &vk::PipelineLayoutCreateInfo::default()
                .set_layouts(&sets)
                .push_constant_ranges(&ranges),
            None,
        )
    }
    .ctx("pipeline layout")
}
