//! OpenXR for FramePlayer.
//!
//! - [`loader`]: finds the active runtime manifest and negotiates with the
//!   runtime directly (no Khronos loader to ship).
//! - [`context`]: instance, system and the Vulkan device the runtime wants
//!   (`XR_KHR_vulkan_enable2`), exposed as an [`fp_render::Creator`].
//! - [`session`]: session lifecycle, per-eye swapchains, the frame loop.
//! - [`input`]: controller actions with bindings for the Steam Frame
//!   controller and common fallbacks.

pub mod context;
pub mod input;
pub mod loader;
pub mod session;

pub use context::XrContext;
pub use input::{Hand, InputState};
pub use session::{FrameCtx, SessionEvent, XrSession};

/// OpenXR errors with the failing call named.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}: XR_{1:?}")]
    Xr(&'static str, openxr::sys::Result),
    #[error("{0}")]
    Runtime(String),
    #[error(transparent)]
    Render(#[from] fp_render::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) trait XrContextExt<T> {
    fn ctx(self, what: &'static str) -> Result<T>;
}

impl<T> XrContextExt<T> for std::result::Result<T, openxr::sys::Result> {
    fn ctx(self, what: &'static str) -> Result<T> {
        self.map_err(|e| Error::Xr(what, e))
    }
}

/// OpenXR pose to glam position and orientation.
pub fn pose(p: openxr::Posef) -> (glam::Vec3, glam::Quat) {
    (
        glam::Vec3::new(p.position.x, p.position.y, p.position.z),
        glam::Quat::from_xyzw(
            p.orientation.x,
            p.orientation.y,
            p.orientation.z,
            p.orientation.w,
        ),
    )
}

/// glam position and orientation to an OpenXR pose.
pub fn to_pose(position: glam::Vec3, orientation: glam::Quat) -> openxr::Posef {
    openxr::Posef {
        orientation: openxr::Quaternionf {
            x: orientation.x,
            y: orientation.y,
            z: orientation.z,
            w: orientation.w,
        },
        position: openxr::Vector3f {
            x: position.x,
            y: position.y,
            z: position.z,
        },
    }
}
