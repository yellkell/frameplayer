//! fp-xr: FramePlayer's OpenXR layer for the Steam Frame (SteamVR runtime).
//!
//! * [`XrContext`] — loader, instance (XR_KHR_vulkan_enable2 plus optional
//!   eye gaze, hand tracking, cylinder/depth layers, refresh rate), HMD system.
//! * [`VulkanContext`] — Vulkan instance/device created *through* the runtime,
//!   with DMA-BUF import extensions added when present; raw ash handles for fp-gfx.
//! * [`XrSession`] — lifecycle-driven event polling, frame loop
//!   (wait/begin/end with predicted display time), views, reference spaces
//!   with recentering, swapchains, quad/cylinder/projection layers,
//!   refresh-rate control.
//! * [`input`] — action set + suggested bindings (Frame, Index, simple),
//!   eye gaze, hand joints with pinch detection, per-frame [`InputState`].
//! * [`math`], [`pinch`], [`lifecycle`], [`select`], [`bindings`] — pure,
//!   unit-tested building blocks.
//!
//! Typical loop (render thread):
//! ```text
//! let ctx = XrContext::new(XrConfig::default())?;
//! let vk = VulkanContext::new(&ctx)?;
//! let mut session = XrSession::new(&ctx, &vk)?;
//! let mut stereo = session.create_stereo_swapchain()?;
//! loop {
//!     session.poll_events(&mut events)?;           // begins/ends the session
//!     if !session.lifecycle().should_run_frame_loop() { sleep; continue }
//!     let t = session.wait_frame()?;
//!     session.begin_frame()?;
//!     let input = session.sync_input(t.predicted_display_time)?;
//!     let views = session.locate_views(t.predicted_display_time)?;
//!     // acquire swapchain images, render with fp-gfx, release
//!     session.end_frame(&t, &[Layer::Projection { .. }, Layer::Quad { .. }])?;
//! }
//! ```
//! Drop order: renderer → `XrSession` → `VulkanContext` → `XrContext`.

pub mod bindings;
pub mod extensions;
pub mod input;
pub mod lifecycle;
pub mod math;
pub mod pinch;
pub mod select;
mod session;
mod vulkan;

pub use extensions::{ExtensionReport, ExtensionWishes};
pub use input::{Button, ControllerState, DPad, HandState, InputState, InputSystem, JointPose};
pub use lifecycle::{Lifecycle, LifecycleAction, SessionState};
pub use math::{Pose, Ray};
pub use pinch::{PinchConfig, PinchDetector, PinchState};
pub use select::{choose_refresh_rate, ColorFormatPreference, DeviceExtensions};
pub use session::{
    FrameTiming, Layer, View, XrConfig, XrContext, XrEvent, XrSession, XrSwapchain, MAX_LAYERS,
};
pub use vulkan::VulkanContext;

/// Re-exports so the app can name OpenXR/Vulkan types without extra deps.
pub use ash;
pub use openxr;

/// Errors from the XR layer.
#[derive(Debug, thiserror::Error)]
pub enum XrError {
    #[error("loading OpenXR runtime: {0}")]
    Load(String),
    #[error("OpenXR: {0}")]
    Xr(#[from] openxr::sys::Result),
    #[error("Vulkan: {0}")]
    Vk(#[from] ash::vk::Result),
    #[error("Vulkan setup: {0}")]
    Vulkan(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
}
