//! The audio master clock.
//!
//! Every [`AudioOutput`](crate::AudioOutput) owns an [`OutputClock`]. The
//! producer side (the pipeline) drops a *marker* whenever it writes a chunk:
//! "output frame `n` carries media time `pts`, and media time advances at
//! `speed` per output second from there". The consumer side (the device
//! callback or the null output's pacing thread) reports how many frames it
//! has actually played. From those two the clock answers "which media
//! instant is audible right now", interpolating between callbacks with the
//! monotonic clock so the value is smooth enough to pace 144 Hz video.
//!
//! `fp-video`'s `AvClock` uses an `Arc<dyn AudioClock>` as its master.

use fp_core::MediaTime;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// A clock that reports the media time of the audio currently being heard.
pub trait AudioClock: Send + Sync {
    /// Media time of the sample reaching the listener now, or `None` when
    /// the clock is not running (nothing queued yet, paused, or starved).
    fn position(&self) -> Option<MediaTime>;

    /// Playback speed (media seconds per wall second) at the current position.
    fn speed(&self) -> f64 {
        1.0
    }
}

#[derive(Debug, Clone, Copy)]
struct Marker {
    frame: u64,
    pts: MediaTime,
    speed: f64,
}

#[derive(Debug)]
struct ClockState {
    sample_rate: u32,
    frames_played: u64,
    frames_written: u64,
    played_at: Option<Instant>,
    latency: Duration,
    paused: bool,
    markers: VecDeque<Marker>,
}

/// Clock state shared between an output's producer and consumer sides.
#[derive(Debug)]
pub struct OutputClock {
    state: Mutex<ClockState>,
}

impl OutputClock {
    pub fn new(sample_rate: u32) -> Self {
        OutputClock {
            state: Mutex::new(ClockState {
                sample_rate: sample_rate.max(1),
                frames_played: 0,
                frames_written: 0,
                played_at: None,
                latency: Duration::ZERO,
                paused: false,
                markers: VecDeque::new(),
            }),
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.state.lock().sample_rate
    }

    /// Producer: output frame `frame` (absolute index since the output was
    /// opened) carries media time `pts`, advancing at `speed`.
    pub fn mark(&self, frame: u64, pts: MediaTime, speed: f64) {
        let mut s = self.state.lock();
        while s.markers.back().is_some_and(|m| m.frame >= frame) {
            s.markers.pop_back();
        }
        s.markers.push_back(Marker { frame, pts, speed });
        // Keep at most one marker at or before the played position.
        let played = s.frames_played;
        while s.markers.len() >= 2 && s.markers[1].frame <= played {
            s.markers.pop_front();
        }
    }

    /// Producer: `frames` more frames were queued.
    pub fn add_written(&self, frames: u64) {
        self.state.lock().frames_written += frames;
    }

    /// Consumer: `frames` frames of real (non-silence) audio were handed to
    /// the device at `now`.
    pub fn advance(&self, frames: u64, now: Instant) {
        let mut s = self.state.lock();
        s.frames_played = (s.frames_played + frames).min(s.frames_written);
        s.played_at = Some(now);
    }

    /// Discard everything queued but not yet played (flush on seek).
    pub fn discard_queued(&self) {
        let mut s = self.state.lock();
        s.frames_written = s.frames_played;
        s.markers.clear();
        s.played_at = None;
    }

    pub fn set_paused(&self, paused: bool) {
        let mut s = self.state.lock();
        s.paused = paused;
        if paused {
            s.played_at = None;
        }
    }

    /// Device latency between "handed to the device" and "heard".
    pub fn set_latency(&self, latency: Duration) {
        self.state.lock().latency = latency;
    }

    pub fn frames_written(&self) -> u64 {
        self.state.lock().frames_written
    }

    pub fn frames_played(&self) -> u64 {
        self.state.lock().frames_played
    }

    pub fn queued_frames(&self) -> u64 {
        let s = self.state.lock();
        s.frames_written - s.frames_played
    }

    /// [`AudioClock::position`] evaluated at an arbitrary instant.
    pub fn position_at(&self, now: Instant) -> Option<MediaTime> {
        let s = self.state.lock();
        if s.paused || s.markers.is_empty() {
            return None;
        }
        let rate = s.sample_rate as f64;
        let played_at = s.played_at?;
        // Interpolate since the last consumer update, but never past what
        // was actually queued (an underrun freezes the clock).
        let since = now.saturating_duration_since(played_at).as_secs_f64();
        let extra = (since * rate).min((s.frames_written - s.frames_played) as f64);
        let heard = s.frames_played as f64 + extra - s.latency.as_secs_f64() * rate;
        let m = s
            .markers
            .iter()
            .rev()
            .find(|m| (m.frame as f64) <= heard)
            .or(s.markers.front())?;
        let dt = (heard - m.frame as f64) / rate * m.speed;
        Some(m.pts + MediaTime::from_secs_f64(dt))
    }

    fn current_speed(&self) -> f64 {
        let s = self.state.lock();
        let played = s.frames_played;
        s.markers
            .iter()
            .rev()
            .find(|m| m.frame <= played)
            .or(s.markers.front())
            .map_or(1.0, |m| m.speed)
    }
}

impl AudioClock for OutputClock {
    fn position(&self) -> Option<MediaTime> {
        self.position_at(Instant::now())
    }

    fn speed(&self) -> f64 {
        self.current_speed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_map_frames_to_media_time() {
        let c = OutputClock::new(1000);
        assert_eq!(c.position(), None);
        c.mark(0, MediaTime::from_millis(5000), 1.0);
        c.add_written(2000);
        let t0 = Instant::now();
        c.advance(500, t0);
        let p = c.position_at(t0).unwrap();
        assert_eq!(p, MediaTime::from_millis(5500));
        // Interpolation between callbacks.
        let p = c.position_at(t0 + Duration::from_millis(100)).unwrap();
        assert_eq!(p, MediaTime::from_millis(5600));
    }

    #[test]
    fn speed_and_underrun() {
        let c = OutputClock::new(1000);
        c.mark(0, MediaTime::ZERO, 2.0);
        c.add_written(100);
        let t0 = Instant::now();
        c.advance(50, t0);
        assert_eq!(c.position_at(t0).unwrap(), MediaTime::from_millis(100));
        // Only 50 frames remain queued: interpolation stops there.
        assert_eq!(
            c.position_at(t0 + Duration::from_secs(10)).unwrap(),
            MediaTime::from_millis(200)
        );
    }

    #[test]
    fn new_marker_after_seek() {
        let c = OutputClock::new(1000);
        c.mark(0, MediaTime::ZERO, 1.0);
        c.add_written(1000);
        let t0 = Instant::now();
        c.advance(1000, t0);
        c.discard_queued();
        c.mark(1000, MediaTime::from_millis(60_000), 1.0);
        c.add_written(1000);
        c.advance(250, t0);
        assert_eq!(c.position_at(t0).unwrap(), MediaTime::from_millis(60_250));
    }

    #[test]
    fn latency_and_pause() {
        let c = OutputClock::new(1000);
        c.mark(0, MediaTime::ZERO, 1.0);
        c.add_written(1000);
        c.set_latency(Duration::from_millis(100));
        let t0 = Instant::now();
        c.advance(500, t0);
        assert_eq!(c.position_at(t0).unwrap(), MediaTime::from_millis(400));
        c.set_paused(true);
        assert_eq!(c.position_at(t0), None);
    }
}
