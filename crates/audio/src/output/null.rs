//! An output that discards audio at real-time pace.
//!
//! A background thread consumes the ring at exactly the nominal sample rate,
//! so the [`OutputClock`] advances as if a device were playing. Used by
//! tests, headless runs, and as the master-clock fallback when no device is
//! available.

use super::{AudioOutput, OutputConfig, SampleRing};
use crate::clock::OutputClock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub struct NullOutput {
    ring: Arc<SampleRing>,
    sample_rate: u32,
    channels: u16,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl NullOutput {
    pub fn new(cfg: OutputConfig) -> Self {
        let ring = SampleRing::new(cfg.sample_rate, cfg.channels, cfg.buffer);
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let ring = ring.clone();
            let stop = stop.clone();
            let rate = cfg.sample_rate as f64;
            let channels = cfg.channels.max(1) as usize;
            std::thread::Builder::new()
                .name("fp-audio-null".into())
                .spawn(move || {
                    let mut scratch = Vec::new();
                    let mut last = Instant::now();
                    let mut carry = 0.0f64;
                    while !stop.load(Ordering::Acquire) {
                        std::thread::sleep(Duration::from_millis(2));
                        let now = Instant::now();
                        let want = now.duration_since(last).as_secs_f64() * rate + carry;
                        last = now;
                        let frames = want.floor() as usize;
                        carry = want - frames as f64;
                        if frames == 0 {
                            continue;
                        }
                        scratch.resize(frames * channels, 0.0);
                        ring.pull(&mut scratch, now);
                    }
                })
                .expect("spawn null audio thread")
        };
        NullOutput {
            ring,
            sample_rate: cfg.sample_rate,
            channels: cfg.channels,
            stop,
            thread: Some(thread),
        }
    }
}

impl AudioOutput for NullOutput {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn channels(&self) -> u16 {
        self.channels
    }
    fn write(&mut self, interleaved: &[f32]) -> usize {
        self.ring.push(interleaved)
    }
    fn free_frames(&self) -> usize {
        self.ring.free_frames()
    }
    fn set_paused(&mut self, paused: bool) {
        self.ring.set_paused(paused);
    }
    fn is_paused(&self) -> bool {
        self.ring.is_paused()
    }
    fn flush(&mut self) {
        self.ring.flush();
    }
    fn clock(&self) -> Arc<OutputClock> {
        self.ring.clock().clone()
    }
    fn backend_name(&self) -> &str {
        "null"
    }
}

impl Drop for NullOutput {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::AudioClock;
    use fp_core::MediaTime;

    #[test]
    fn consumes_in_real_time() {
        let mut out = NullOutput::new(OutputConfig {
            sample_rate: 48_000,
            channels: 2,
            buffer: Duration::from_secs(2),
        });
        let clock = out.clock();
        clock.mark(out.frames_written(), MediaTime::from_secs_f64(10.0), 1.0);
        let n = out.write(&vec![0.0; 48_000 * 2]);
        assert_eq!(n, 48_000);
        let t0 = std::time::Instant::now();
        std::thread::sleep(Duration::from_millis(200));
        let p = clock.position().expect("running").as_secs_f64() - 10.0;
        let el = t0.elapsed().as_secs_f64();
        assert!(p > el * 0.7 && p < el + 0.05, "position +{p}s after {el}s");
        out.set_paused(true);
        assert!(clock.position().is_none());
    }
}
