//! Audio: resampling, time-stretch, ambisonic rendering and device output.

pub mod ambisonic;
pub mod output;
pub mod resample;
pub mod stretch;

/// Output sample rate for every device.
pub const RATE: u32 = 48_000;
