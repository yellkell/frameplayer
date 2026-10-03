//! FramePlayer audio.
//!
//! This crate owns everything between decoded PCM and the speakers:
//!
//! * [`output`]: the [`AudioOutput`] trait, a real-time [`NullOutput`] (tests,
//!   headless runs, and the master-clock fallback when no device exists) and,
//!   behind the `cpal` / `alsa` / `pipewire` features, a cpal device backend.
//! * [`clock`]: the [`AudioClock`] trait and the [`OutputClock`] every output
//!   drives. **Dependency direction:** the clock trait lives here and
//!   `fp-video` depends on `fp-audio`; its `AvClock` consumes an
//!   `Arc<dyn AudioClock>` as the master clock. `fp-audio` never depends on
//!   `fp-video`.
//! * DSP: ITU downmix ([`downmix`]), windowed-sinc resampling ([`resample`]),
//!   pitch-preserving WSOLA time stretch ([`stretch`]), uniformly partitioned
//!   FFT convolution ([`convolve`]).
//! * [`ambisonics`]: AmbiX/FuMa handling, head-tracked rotation up to 3rd
//!   order, and binaural rendering through a procedurally generated
//!   spherical-head HRIR set.
//! * [`pipeline`]: the chain the player feeds (channel processing → time
//!   stretch → resample → output), with media-time markers so the clock
//!   reports exactly which media sample is audible.

pub mod ambisonics;
pub mod clock;
pub mod convolve;
pub mod downmix;
pub mod error;
pub mod format;
pub mod output;
pub mod pipeline;
pub mod resample;
pub mod stretch;

pub use clock::{AudioClock, OutputClock};
pub use error::{AudioError, Result};
pub use format::{AmbisonicNorm, ChannelLayout};
pub use output::{open_default_output, AudioOutput, NullOutput, OutputConfig};
pub use pipeline::{AudioPipeline, PipelineConfig};
