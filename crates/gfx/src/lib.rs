//! fp-gfx: FramePlayer's Vulkan renderer.
//!
//! * [`mesh`] — projection meshes in pure Rust (equirect, fisheye, EAC,
//!   flat/curved screen, OBJ custom meshes).
//! * [`correction`] / [`color`] — CPU reference implementations of the
//!   shader math (per-eye UV corrections, YUV→RGB, PQ/HLG, tone mapping),
//!   plus the push-constant blocks that feed the shaders.
//! * [`shaders`] — WGSL compiled to SPIR-V at build time by naga.
//! * [`vk`] — the [`Renderer`]: DMA-BUF/CPU video input, YUV→RGBA16F
//!   compute, per-eye projection pass, UI draw-list pass, thumbnail cache.
//! * [`camera`] — eye views and Vulkan projection matrices from OpenXR FOVs.
//!
//! The crate is independent of fp-xr: the app passes raw ash handles
//! ([`GpuContext::new`]) and per-frame eye poses/FOVs ([`EyeView`]).

pub mod camera;
pub mod color;
pub mod correction;
pub mod frame;
pub mod mesh;
pub mod shaders;
pub mod vk;

pub use camera::{projection_matrix, EyeView, Fov};
pub use color::{ColorInfo, ColorRange, DisplayParams, Primaries, ToneMapOp, YuvMatrix};
pub use frame::{CpuFrame, DmaBufFrame, DmaBufPlane, PixelFormat, VideoFrame};
pub use mesh::{build_mesh, Mesh, MeshDensity, MeshVertex};
pub use vk::{
    DeviceCaps, EyeRenderRequest, GfxError, GpuContext, RenderTarget, Renderer, RendererConfig,
    UiRenderRequest,
};

/// Re-export so callers can name Vulkan types without their own ash dependency.
pub use ash;
