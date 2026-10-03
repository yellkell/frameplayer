//! The A/V clock that paces video presentation.
//!
//! Audio is the master: when an [`AudioClock`] is attached and advancing,
//! the media time "now" is whatever sample is audible. Otherwise (no audio
//! track, audio starved or finished, during seeks) an internal monotonic
//! clock takes over, continuously re-anchored to the audio clock so the
//! hand-over is seamless.
//!
//! [`AvClock::video_time_at`] answers "which media time should be on screen
//! at this (predicted display) instant", including the user's A/V offset.

use fp_audio::AudioClock;
use fp_core::MediaTime;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const MIN_SPEED: f64 = 0.25;
pub const MAX_SPEED: f64 = 4.0;

/// If the audio clock has not moved for this long while playing, treat it
/// as stalled (underrun / audio track ended) and fall back to the internal clock.
const AUDIO_STALL: Duration = Duration::from_millis(150);

struct Inner {
    base_media: MediaTime,
    base_instant: Instant,
    speed: f64,
    paused: bool,
    av_offset: MediaTime,
    audio: Option<Arc<dyn AudioClock>>,
    last_audio: Option<(MediaTime, Instant)>,
}

pub struct AvClock {
    inner: Mutex<Inner>,
}

impl Default for AvClock {
    fn default() -> Self {
        Self::new()
    }
}

impl AvClock {
    pub fn new() -> Self {
        AvClock {
            inner: Mutex::new(Inner {
                base_media: MediaTime::ZERO,
                base_instant: Instant::now(),
                speed: 1.0,
                paused: true,
                av_offset: MediaTime::ZERO,
                audio: None,
                last_audio: None,
            }),
        }
    }

    /// Attach (or detach) the audio master clock.
    pub fn set_audio_master(&self, audio: Option<Arc<dyn AudioClock>>) {
        let mut s = self.inner.lock();
        s.audio = audio;
        s.last_audio = None;
    }

    fn internal_at(s: &Inner, at: Instant) -> MediaTime {
        if s.paused {
            return s.base_media;
        }
        let dt = if at >= s.base_instant {
            at.duration_since(s.base_instant).as_secs_f64()
        } else {
            -(s.base_instant.duration_since(at).as_secs_f64())
        };
        s.base_media + MediaTime::from_secs_f64(dt * s.speed)
    }

    /// Audible media time at `at` (no A/V offset), updating the anchor.
    fn master_at(&self, at: Instant) -> MediaTime {
        let mut s = self.inner.lock();
        if s.paused {
            return s.base_media;
        }
        let now = Instant::now();
        if let Some(p) = s.audio.as_ref().and_then(|a| a.position()) {
            let moved = match s.last_audio {
                Some((lp, t)) => p != lp || now.duration_since(t) < AUDIO_STALL,
                None => true,
            };
            if s.last_audio.is_none_or(|(lp, _)| lp != p) {
                s.last_audio = Some((p, now));
            }
            if moved {
                // Re-anchor the internal clock on the audio clock.
                s.base_media = p;
                s.base_instant = now;
            }
        }
        Self::internal_at(&s, at)
    }

    /// Current audible playback position (no A/V offset).
    pub fn position(&self) -> MediaTime {
        self.master_at(Instant::now())
    }

    /// Media time to present at `display_at`, including the A/V offset.
    pub fn video_time_at(&self, display_at: Instant) -> MediaTime {
        let t = self.master_at(display_at);
        t + self.inner.lock().av_offset
    }

    /// Jump to `t` (seek / frame step).
    pub fn set_position(&self, t: MediaTime) {
        let mut s = self.inner.lock();
        s.base_media = t;
        s.base_instant = Instant::now();
        s.last_audio = None;
    }

    pub fn set_paused(&self, paused: bool) {
        let mut s = self.inner.lock();
        if paused == s.paused {
            return;
        }
        let now = Instant::now();
        s.base_media = Self::internal_at(&s, now);
        s.base_instant = now;
        s.paused = paused;
        s.last_audio = None;
    }

    pub fn is_paused(&self) -> bool {
        self.inner.lock().paused
    }

    /// Playback speed, clamped to 0.25–4×.
    pub fn set_speed(&self, speed: f64) {
        let mut s = self.inner.lock();
        let now = Instant::now();
        s.base_media = Self::internal_at(&s, now);
        s.base_instant = now;
        s.speed = speed.clamp(MIN_SPEED, MAX_SPEED);
    }

    pub fn speed(&self) -> f64 {
        self.inner.lock().speed
    }

    /// Positive offset shows video earlier relative to audio (compensates
    /// late audio such as Bluetooth latency); negative delays video.
    pub fn set_av_offset(&self, offset: MediaTime) {
        self.inner.lock().av_offset = offset;
    }

    pub fn av_offset(&self) -> MediaTime {
        self.inner.lock().av_offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedAudio(Mutex<Option<MediaTime>>);
    impl AudioClock for FixedAudio {
        fn position(&self) -> Option<MediaTime> {
            *self.0.lock()
        }
    }

    #[test]
    fn internal_clock_runs_pauses_and_scales() {
        let c = AvClock::new();
        c.set_position(MediaTime::from_secs_f64(10.0));
        assert_eq!(
            c.position(),
            MediaTime::from_secs_f64(10.0),
            "starts paused"
        );
        c.set_paused(false);
        let t0 = Instant::now();
        let p = c.video_time_at(t0 + Duration::from_secs(1));
        assert!((p.as_secs_f64() - 11.0).abs() < 0.01, "{p}");
        c.set_speed(2.0);
        let p0 = c.position();
        let p = c.video_time_at(Instant::now() + Duration::from_millis(500));
        assert!(((p - p0).as_secs_f64() - 1.0).abs() < 0.01);
        c.set_speed(10.0);
        assert_eq!(c.speed(), MAX_SPEED);
        c.set_paused(true);
        let a = c.position();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(c.position(), a);
    }

    #[test]
    fn audio_master_and_offset() {
        let c = AvClock::new();
        let audio = Arc::new(FixedAudio(Mutex::new(Some(MediaTime::from_secs_f64(5.0)))));
        c.set_audio_master(Some(audio.clone()));
        c.set_paused(false);
        let now = Instant::now();
        let p = c.video_time_at(now);
        assert!((p.as_secs_f64() - 5.0).abs() < 0.01, "{p}");
        // Extrapolates to a future display time.
        let p = c.video_time_at(now + Duration::from_millis(100));
        assert!((p.as_secs_f64() - 5.1).abs() < 0.01, "{p}");
        c.set_av_offset(MediaTime::from_millis(-40));
        let p = c.video_time_at(Instant::now());
        assert!((p.as_secs_f64() - 4.96).abs() < 0.01, "{p}");
        assert!((c.position().as_secs_f64() - 5.0).abs() < 0.01);
    }

    #[test]
    fn stalled_audio_falls_back_to_internal() {
        let c = AvClock::new();
        let audio = Arc::new(FixedAudio(Mutex::new(Some(MediaTime::from_secs_f64(1.0)))));
        c.set_audio_master(Some(audio.clone()));
        c.set_paused(false);
        let _ = c.position();
        std::thread::sleep(AUDIO_STALL + Duration::from_millis(100));
        // Audio frozen at 1.0 s but wall time moved on.
        let p = c.position().as_secs_f64();
        assert!(
            p > 1.05,
            "clock should keep running past stalled audio, got {p}"
        );
        // No audio at all.
        *audio.0.lock() = None;
        let p2 = c.position().as_secs_f64();
        assert!(p2 >= p);
    }
}
