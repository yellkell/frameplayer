//! fp-ui: FramePlayer's immediate-mode VR UI toolkit.
//!
//! Pure Rust and renderer-agnostic: every frame a [`Ui`] turns
//! [`FrameInput`] (laser / gaze / hand pointers, thumbstick navigation,
//! text events) into an [`fp_core::draw::DrawList`] in panel pixels plus an
//! R8 font atlas ([`fp_core::draw::AtlasImage`]) that `fp-gfx` rasterizes
//! into an OpenXR quad or cylinder layer.
//!
//! Layers of the crate:
//! - [`geom`], [`theme`], [`text`], [`painter`], [`icons`]: drawing primitives.
//! - [`input`]: pointer model, ray-vs-panel intersection, gaze dimming.
//! - [`layout`]: stacking, grids, scroll physics, virtualization math.
//! - [`ui`] + [`widgets`]: the context and widgets.
//! - [`screens`]: Library, Player controls, Picture adjust and Settings,
//!   driven by plain view-model structs and returning [`UiAction`]s.

pub mod geom;
pub mod icons;
pub mod input;
pub mod layout;
pub mod painter;
pub mod screens;
pub mod text;
pub mod theme;
pub mod ui;
pub mod widgets;

pub use geom::{Rect, Vec2};
pub use icons::Icon;
pub use input::{
    ray_cylinder, ray_quad, CylinderPanel, FrameInput, GazeDimmer, Hand, NavDir, NavInput,
    PanelHit, PointerInput, PointerSource, PokeDetector, QuadPanel, Ray, StickNavigator, TextEvent,
};
pub use layout::{ScrollState, FILL};
pub use painter::Layer;
pub use screens::UiAction;
pub use text::{Align, Fonts, TextParams, DEFAULT_FONT};
pub use theme::{Color, Theme};
pub use ui::{Feedback, Id, Response, Sense, Ui, UiOutput};
pub use widgets::toast::ToastKind;
