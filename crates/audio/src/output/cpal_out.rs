//! cpal-backed device output (features `cpal`, `alsa`, `pipewire`).
//!
//! The cpal `Stream` is not `Send` on every host, so it lives on a small
//! dedicated thread that builds it, starts it and parks until the output is
//! dropped. The data callback pulls from the shared [`SampleRing`] and feeds
//! the device's reported playback latency into the clock.
//!
//! [verify] On SteamOS (Frame) the default cpal host is ALSA, which reaches
//! PipeWire through the `pipewire-alsa` PCM plugin. Confirm that the
//! `default` PCM exists for a non-root gaming-mode app and that the reported
//! playback timestamps are sane; otherwise fall back to a fixed latency.

use super::{AudioOutput, OutputConfig, SampleRing};
use crate::clock::OutputClock;
use crate::{AudioError, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub struct CpalOutput {
    ring: Arc<SampleRing>,
    sample_rate: u32,
    channels: u16,
    name: String,
    stop_tx: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl CpalOutput {
    pub fn open(cfg: OutputConfig) -> Result<Self> {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(Arc<SampleRing>, u32, u16, String)>>();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("fp-audio-cpal".into())
            .spawn(move || {
                let built = build_stream(cfg);
                match built {
                    Ok((stream, ring, rate, ch, name)) => {
                        let _ = ready_tx.send(Ok((ring, rate, ch, name)));
                        // Park until the output is dropped.
                        let _ = stop_rx.recv();
                        drop(stream);
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
            })
            .map_err(|e| AudioError::Backend(e.to_string()))?;
        let (ring, sample_rate, channels, name) = ready_rx
            .recv()
            .map_err(|_| AudioError::Backend("cpal thread exited".into()))??;
        Ok(CpalOutput {
            ring,
            sample_rate,
            channels,
            name,
            stop_tx: Some(stop_tx),
            thread: Some(thread),
        })
    }
}

fn build_stream(cfg: OutputConfig) -> Result<(cpal::Stream, Arc<SampleRing>, u32, u16, String)> {
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or(AudioError::NoDevice)?;
    let name = format!(
        "cpal:{}:{}",
        host.id().name(),
        device.name().unwrap_or_else(|_| "default".into())
    );
    // Prefer the requested rate; fall back to the device default.
    let default = device
        .default_output_config()
        .map_err(|e| AudioError::Backend(e.to_string()))?;
    let supports_requested = device
        .supported_output_configs()
        .map(|mut it| {
            it.any(|c| {
                c.channels() == cfg.channels
                    && c.min_sample_rate().0 <= cfg.sample_rate
                    && c.max_sample_rate().0 >= cfg.sample_rate
                    && c.sample_format() == cpal::SampleFormat::F32
            })
        })
        .unwrap_or(false);
    let (rate, channels) = if supports_requested {
        (cfg.sample_rate, cfg.channels)
    } else {
        (default.sample_rate().0, default.channels())
    };
    let stream_cfg = cpal::StreamConfig {
        channels,
        sample_rate: cpal::SampleRate(rate),
        buffer_size: cpal::BufferSize::Default,
    };
    let ring = SampleRing::new(rate, channels, cfg.buffer);
    let cb_ring = ring.clone();
    let stream = device
        .build_output_stream(
            &stream_cfg,
            move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
                let ts = info.timestamp();
                if let Some(lat) = ts.playback.duration_since(&ts.callback) {
                    cb_ring
                        .clock()
                        .set_latency(lat.min(Duration::from_millis(500)));
                }
                cb_ring.pull(data, Instant::now());
            },
            |err| tracing::warn!("audio stream error: {err}"),
            None,
        )
        .map_err(|e| AudioError::Backend(e.to_string()))?;
    stream
        .play()
        .map_err(|e| AudioError::Backend(e.to_string()))?;
    tracing::info!("audio output {name} at {rate} Hz, {channels} ch");
    Ok((stream, ring, rate, channels, name))
}

impl AudioOutput for CpalOutput {
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
        &self.name
    }
}

impl Drop for CpalOutput {
    fn drop(&mut self) {
        drop(self.stop_tx.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
