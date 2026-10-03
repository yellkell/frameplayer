//! Audio output backends.
//!
//! All outputs share the same push model: the player writes interleaved
//! `f32` frames into a bounded ring ([`SampleRing`]) without blocking, and a
//! consumer (device callback or pacing thread) pulls from it in real time,
//! advancing the output's [`OutputClock`]. Underruns play silence and freeze
//! the clock; pausing stops consumption.

mod null;
pub use null::NullOutput;

#[cfg(feature = "cpal")]
mod cpal_out;
#[cfg(feature = "cpal")]
pub use cpal_out::CpalOutput;

use crate::clock::OutputClock;
use crate::Result;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Requested output parameters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputConfig {
    pub sample_rate: u32,
    pub channels: u16,
    /// Ring capacity; bounds how far ahead the player can write.
    pub buffer: Duration,
}

impl Default for OutputConfig {
    fn default() -> Self {
        OutputConfig {
            sample_rate: 48_000,
            channels: 2,
            buffer: Duration::from_millis(250),
        }
    }
}

/// A sink for interleaved `f32` PCM that drives an [`OutputClock`].
pub trait AudioOutput: Send {
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> u16;
    /// Queue interleaved samples without blocking. Returns the number of
    /// *frames* accepted (fewer than offered when the ring is full).
    fn write(&mut self, interleaved: &[f32]) -> usize;
    /// Frames that can currently be written without being refused.
    fn free_frames(&self) -> usize;
    fn set_paused(&mut self, paused: bool);
    fn is_paused(&self) -> bool;
    /// Drop everything queued (seek / track switch).
    fn flush(&mut self);
    /// The clock this output drives.
    fn clock(&self) -> Arc<OutputClock>;
    /// Absolute index of the next frame `write` will queue; used for clock markers.
    fn frames_written(&self) -> u64 {
        self.clock().frames_written()
    }
    /// Human-readable backend name for diagnostics.
    fn backend_name(&self) -> &str;
}

/// Bounded interleaved sample ring shared by producer and consumer.
#[derive(Debug)]
pub struct SampleRing {
    buf: Mutex<VecDeque<f32>>,
    channels: usize,
    capacity_frames: usize,
    paused: AtomicBool,
    clock: Arc<OutputClock>,
}

impl SampleRing {
    pub fn new(sample_rate: u32, channels: u16, capacity: Duration) -> Arc<Self> {
        let channels = channels.max(1) as usize;
        let capacity_frames = ((capacity.as_secs_f64() * sample_rate as f64) as usize).max(256);
        Arc::new(SampleRing {
            buf: Mutex::new(VecDeque::with_capacity(capacity_frames * channels)),
            channels,
            capacity_frames,
            paused: AtomicBool::new(false),
            clock: Arc::new(OutputClock::new(sample_rate)),
        })
    }

    pub fn clock(&self) -> &Arc<OutputClock> {
        &self.clock
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn free_frames(&self) -> usize {
        self.capacity_frames - self.buf.lock().len() / self.channels
    }

    /// Producer side. Returns frames accepted.
    pub fn push(&self, interleaved: &[f32]) -> usize {
        let mut b = self.buf.lock();
        let free = self.capacity_frames - b.len() / self.channels;
        let frames = (interleaved.len() / self.channels).min(free);
        b.extend(&interleaved[..frames * self.channels]);
        drop(b);
        self.clock.add_written(frames as u64);
        frames
    }

    /// Consumer side: fill `out` (interleaved, `channels` wide) with queued
    /// audio, padding with silence. Returns the number of real frames.
    pub fn pull(&self, out: &mut [f32], now: Instant) -> usize {
        if self.paused.load(Ordering::Acquire) {
            out.fill(0.0);
            return 0;
        }
        let mut b = self.buf.lock();
        let n = (b.len() / self.channels).min(out.len() / self.channels) * self.channels;
        for (o, s) in out.iter_mut().zip(b.drain(..n)) {
            *o = s;
        }
        drop(b);
        out[n..].fill(0.0);
        let frames = n / self.channels;
        if frames > 0 {
            self.clock.advance(frames as u64, now);
        }
        frames
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Release);
        self.clock.set_paused(paused);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    pub fn flush(&self) {
        self.buf.lock().clear();
        self.clock.discard_queued();
    }
}

/// Open the best available output: the cpal device when compiled in and a
/// device exists, otherwise a [`NullOutput`] so playback (and the master
/// clock) still work.
pub fn open_default_output(cfg: OutputConfig) -> Result<Box<dyn AudioOutput>> {
    #[cfg(feature = "cpal")]
    {
        match CpalOutput::open(cfg) {
            Ok(o) => return Ok(Box::new(o)),
            Err(e) => tracing::warn!("audio device unavailable ({e}); using null output"),
        }
    }
    Ok(Box::new(NullOutput::new(cfg)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::AudioClock;
    use fp_core::MediaTime;

    #[test]
    fn ring_push_pull_and_clock() {
        let r = SampleRing::new(1000, 2, Duration::from_millis(500));
        r.clock().mark(0, MediaTime::ZERO, 1.0);
        let data = vec![0.5f32; 2 * 1000];
        assert_eq!(r.push(&data), 500, "capacity is 500 frames");
        assert_eq!(r.free_frames(), 0);
        let mut out = vec![0.0f32; 2 * 100];
        let now = Instant::now();
        assert_eq!(r.pull(&mut out, now), 100);
        assert!(out.iter().all(|&s| s == 0.5));
        assert_eq!(
            r.clock().position_at(now),
            Some(MediaTime::from_millis(100))
        );
        r.set_paused(true);
        assert_eq!(r.pull(&mut out, now), 0);
        assert!(out.iter().all(|&s| s == 0.0));
        assert_eq!(r.clock().position(), None);
        r.set_paused(false);
        r.flush();
        assert_eq!(r.free_frames(), 500);
    }
}
