//! Playback engine: A/V clock, decode-ahead frame queue, seek engine and
//! the [`Player`] handle (command channel in, status/events/frames out).

pub mod clock;
pub mod player;
pub mod queue;

pub use clock::AvClock;
pub use player::{
    DefaultBackend, MediaBackend, OpenRequest, PlaybackState, Player, PlayerCommand, PlayerConfig,
    PlayerEvent, PlayerStatus, SeekMode, VideoOutput,
};
pub use queue::{FrameQueue, QueueStats, VideoFrame};
