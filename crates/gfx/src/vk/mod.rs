//! Vulkan backend: device wrapper, resource helpers, pipelines, video input
//! (DMA-BUF import and CPU upload), texture cache, and the [`Renderer`].
//!
//! Requires Vulkan 1.3 with `dynamicRendering` and `synchronization2`
//! enabled on the device (fp-xr does this). Turnip on Adreno 750 is 1.3
//! conformant. [verify] on the Frame's Mesa build.

mod device;
mod pipelines;
mod renderer;
mod textures;
mod util;
mod video;

pub use device::{DeviceCaps, GpuContext};
pub use renderer::{EyeRenderRequest, RenderTarget, Renderer, RendererConfig, UiRenderRequest};

use bytemuck::{Pod, Zeroable};

/// Push constants for the UI pass (16 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct UiPush {
    /// `2 / panel_size` in pixels.
    pub scale: [f32; 2],
    /// 0 solid, 1 coverage, 2 straight-alpha image, 3 opaque video.
    pub mode: u32,
    /// Bit 0: encode sRGB in the shader.
    pub flags: u32,
}

/// Errors from the Vulkan backend.
#[derive(Debug, thiserror::Error)]
pub enum GfxError {
    #[error("vulkan error: {0}")]
    Vk(#[from] ash::vk::Result),
    #[error("no suitable memory type")]
    NoMemoryType,
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("invalid frame: {0}")]
    InvalidFrame(String),
    #[error("call must happen between begin_frame and end_frame")]
    NotInFrame,
    #[error(transparent)]
    Mesh(#[from] crate::mesh::MeshError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, GfxError>;

/// True for `_SRGB` formats, whose hardware encodes on write.
pub fn is_srgb_format(f: ash::vk::Format) -> bool {
    use ash::vk::Format as F;
    matches!(
        f,
        F::R8G8B8A8_SRGB
            | F::B8G8R8A8_SRGB
            | F::A8B8G8R8_SRGB_PACK32
            | F::R8G8B8_SRGB
            | F::B8G8R8_SRGB
            | F::R8_SRGB
            | F::R8G8_SRGB
    )
}

/// True for formats storing linear values the shader must gamma-encode
/// itself before the compositor treats them as sRGB-encoded (UNORM 8-bit).
pub fn needs_shader_encode(f: ash::vk::Format) -> bool {
    use ash::vk::Format as F;
    matches!(
        f,
        F::R8G8B8A8_UNORM
            | F::B8G8R8A8_UNORM
            | F::A8B8G8R8_UNORM_PACK32
            | F::A2B10G10R10_UNORM_PACK32
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ash::vk::Format;

    #[test]
    fn format_encoding_classes() {
        assert!(is_srgb_format(Format::R8G8B8A8_SRGB));
        assert!(!needs_shader_encode(Format::R8G8B8A8_SRGB));
        assert!(needs_shader_encode(Format::B8G8R8A8_UNORM));
        assert!(!needs_shader_encode(Format::R16G16B16A16_SFLOAT));
        assert_eq!(std::mem::size_of::<UiPush>(), 16);
    }
}
