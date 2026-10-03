//! Shared vocabulary for every FramePlayer crate.
//!
//! Nothing in here touches hardware, I/O, or async runtimes; it is plain data
//! plus the pure functions that interpret it, so every other crate can depend
//! on it without pulling anything heavy in.

pub mod detect;
pub mod draw;
pub mod media;
pub mod projection;
pub mod time;

pub use media::{
    AudioTrackInfo, Chapter, Codec, ColorTransfer, MediaInfo, SubtitleTrackInfo, VideoTrackInfo,
};
pub use projection::{Corrections, FisheyeLens, Projection, StereoMode, ViewSettings};
pub use time::MediaTime;
