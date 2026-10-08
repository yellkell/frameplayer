//! Types shared by every FramePlayer crate.
//!
//! - [`format`]: how a video maps onto the sphere or screen ([`Projection`],
//!   [`StereoLayout`]) and detection from file names and metadata.
//! - [`view`]: per-video picture corrections ([`ViewSettings`]).
//! - [`source`]: the contract between media sources and the player
//!   ([`ByteSource`], [`Entry`]).
//! - [`playback`]: playback status and commands shared with haptics and the
//!   remote-control API.
//! - [`dirs`]: where FramePlayer keeps config, data and cache.

pub mod dirs;
pub mod format;
pub mod playback;
pub mod source;
pub mod view;

pub use format::{DetectedFormat, Projection, StereoLayout, VideoFormat};
pub use playback::{PlaybackStatus, PlayerCommand};
pub use source::{ByteSource, Entry, EntryKind};
pub use view::ViewSettings;
