//! The audio processing chain fed by the player.
//!
//! `decoded PCM → channel stage (passthrough / ITU downmix / binaural
//! ambisonics) → WSOLA time stretch → resample to device rate → output`.
//!
//! Every output chunk carries the media time of its first sample and the
//! playback speed; [`AudioPipeline::write_to`] turns those into
//! [`OutputClock`](crate::OutputClock) markers, so the audio master clock
//! stays exact through speed changes, resampling and filter latency.

use crate::ambisonics::BinauralRenderer;
use crate::downmix::{DownmixMatrix, DownmixOptions};
use crate::format::ChannelLayout;
use crate::output::AudioOutput;
use crate::resample::Resampler;
use crate::stretch::Wsola;
use fp_core::MediaTime;
use glam::Quat;
use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PipelineConfig {
    pub in_rate: u32,
    pub layout: ChannelLayout,
    pub out_rate: u32,
    pub out_channels: u16,
    /// Render ambisonic tracks binaurally (otherwise a crude stereo fold-down).
    pub binaural: bool,
    pub downmix: DownmixOptions,
}

impl PipelineConfig {
    pub fn new(in_rate: u32, layout: ChannelLayout, out_rate: u32, out_channels: u16) -> Self {
        PipelineConfig {
            in_rate,
            layout,
            out_rate,
            out_channels,
            binaural: true,
            downmix: DownmixOptions::default(),
        }
    }
}

enum ChannelStage {
    Passthrough,
    Downmix(DownmixMatrix),
    Binaural(Box<BinauralRenderer>),
}

struct OutChunk {
    samples: Vec<f32>,
    offset: usize,
    pts: MediaTime,
    speed: f64,
}

pub struct AudioPipeline {
    cfg: PipelineConfig,
    in_channels: usize,
    out_channels: usize,
    stage: ChannelStage,
    stage_latency: u64,
    wsola: Wsola,
    resampler: Resampler,
    base_pts: Option<MediaTime>,
    in_frames: u64,
    wsola_out: u64,
    resampled_out: u64,
    /// `(wsola output frame, input frame, speed)` markers.
    markers: VecDeque<(u64, f64, f64)>,
    pending: VecDeque<OutChunk>,
    s1: Vec<f32>,
    s2: Vec<f32>,
    s3: Vec<f32>,
}

impl AudioPipeline {
    pub fn new(cfg: PipelineConfig) -> Self {
        let in_channels = cfg.layout.channels();
        let out_channels = cfg.out_channels.max(1) as usize;
        let (stage, stage_latency) = match cfg.layout {
            ChannelLayout::Ambisonic { order, norm } if cfg.binaural && out_channels == 2 => {
                let r = BinauralRenderer::new(order as usize, norm, cfg.in_rate);
                let lat = r.latency() as u64;
                (ChannelStage::Binaural(Box::new(r)), lat)
            }
            _ if in_channels == out_channels && !cfg.layout.is_ambisonic() => {
                (ChannelStage::Passthrough, 0)
            }
            layout if out_channels == 2 => (
                ChannelStage::Downmix(DownmixMatrix::itu_stereo(layout, cfg.downmix)),
                0,
            ),
            _ => {
                // Generic: route channel i → i, drop extras.
                let mut coeffs = vec![0.0; out_channels * in_channels];
                for i in 0..out_channels.min(in_channels) {
                    coeffs[i * in_channels + i] = 1.0;
                }
                (
                    ChannelStage::Downmix(DownmixMatrix {
                        inputs: in_channels,
                        outputs: out_channels,
                        coeffs,
                    }),
                    0,
                )
            }
        };
        AudioPipeline {
            cfg,
            in_channels,
            out_channels,
            stage,
            stage_latency,
            wsola: Wsola::with_defaults(cfg.in_rate, out_channels),
            resampler: Resampler::new(cfg.in_rate, cfg.out_rate, out_channels),
            base_pts: None,
            in_frames: 0,
            wsola_out: 0,
            resampled_out: 0,
            markers: VecDeque::new(),
            pending: VecDeque::new(),
            s1: Vec::new(),
            s2: Vec::new(),
            s3: Vec::new(),
        }
    }

    pub fn config(&self) -> &PipelineConfig {
        &self.cfg
    }

    pub fn speed(&self) -> f64 {
        self.wsola.speed()
    }

    /// 0.25–4×, pitch preserved.
    pub fn set_speed(&mut self, speed: f64) {
        self.wsola.set_speed(speed);
    }

    /// Head orientation (OpenXR pose quaternion) for binaural ambisonics.
    pub fn set_head_orientation(&mut self, head: Quat) {
        if let ChannelStage::Binaural(b) = &mut self.stage {
            b.set_head_orientation(head);
        }
    }

    /// Drop all buffered state (seek, track change).
    pub fn reset(&mut self) {
        if let ChannelStage::Binaural(b) = &mut self.stage {
            b.reset();
        }
        self.wsola.reset();
        self.resampler.reset();
        self.base_pts = None;
        self.in_frames = 0;
        self.wsola_out = 0;
        self.resampled_out = 0;
        self.markers.clear();
        self.pending.clear();
    }

    /// Frames (at the output rate) processed but not yet written.
    pub fn pending_frames(&self) -> usize {
        self.pending
            .iter()
            .map(|c| (c.samples.len() - c.offset) / self.out_channels)
            .sum()
    }

    /// Media time of the next sample the pipeline expects (end of pushed input).
    pub fn input_end(&self) -> Option<MediaTime> {
        self.base_pts
            .map(|b| b + MediaTime::from_secs_f64(self.in_frames as f64 / self.cfg.in_rate as f64))
    }

    /// Push decoded interleaved PCM (layout per config) starting at `pts`.
    pub fn push(&mut self, pcm: &[f32], pts: MediaTime) {
        let frames = pcm.len() / self.in_channels.max(1);
        if frames == 0 {
            return;
        }
        if let Some(expected) = self.input_end() {
            if (pts.0 - expected.0).abs() > 200_000 {
                tracing::debug!("audio discontinuity: expected {expected}, got {pts}; rebasing");
                let keep = std::mem::take(&mut self.pending);
                self.reset();
                self.pending = keep;
            }
        }
        if self.base_pts.is_none() {
            self.base_pts = Some(pts);
        }
        self.in_frames += frames as u64;
        let pcm = &pcm[..frames * self.in_channels];

        // Channel stage.
        self.s1.clear();
        match &mut self.stage {
            ChannelStage::Passthrough => self.s1.extend_from_slice(pcm),
            ChannelStage::Downmix(m) => m.apply(pcm, &mut self.s1),
            ChannelStage::Binaural(b) => b.process(pcm, self.in_channels, &mut self.s1),
        }
        // Time stretch.
        let speed = self.wsola.speed();
        self.markers
            .push_back((self.wsola_out, self.wsola.input_pos_of_next_output(), speed));
        self.s2.clear();
        self.wsola.process(&self.s1, &mut self.s2);
        self.wsola_out += (self.s2.len() / self.out_channels) as u64;
        // Resample.
        self.s3.clear();
        self.resampler.process(&self.s2, &mut self.s3);
        let produced = self.s3.len() / self.out_channels;
        if produced == 0 {
            return;
        }
        let (chunk_pts, chunk_speed) = self.media_time_of_output(self.resampled_out);
        self.resampled_out += produced as u64;
        // Prune markers no longer needed.
        let horizon = self.resampled_out as f64 * self.resampler.step();
        while self.markers.len() > 1 && (self.markers[1].0 as f64) <= horizon {
            self.markers.pop_front();
        }
        self.pending.push_back(OutChunk {
            samples: std::mem::take(&mut self.s3),
            offset: 0,
            pts: chunk_pts,
            speed: chunk_speed,
        });
    }

    fn media_time_of_output(&self, resampled_index: u64) -> (MediaTime, f64) {
        let m = resampled_index as f64 * self.resampler.step();
        let mk = self
            .markers
            .iter()
            .rev()
            .find(|mk| (mk.0 as f64) <= m)
            .or(self.markers.front());
        let (input_pos, speed) = match mk {
            Some(&(m0, in0, sp)) => (in0 + (m - m0 as f64) * sp, sp),
            None => (m, 1.0),
        };
        let input_pos = input_pos - self.stage_latency as f64;
        let base = self.base_pts.unwrap_or(MediaTime::ZERO);
        // Speed in the clock is media seconds per *output* second.
        (
            base + MediaTime::from_secs_f64(input_pos / self.cfg.in_rate as f64),
            speed,
        )
    }

    /// Write as much pending audio as the output accepts, placing clock
    /// markers. Returns frames written.
    pub fn write_to(&mut self, out: &mut dyn AudioOutput) -> usize {
        let ch = self.out_channels;
        let clock = out.clock();
        let mut total = 0;
        while let Some(c) = self.pending.front_mut() {
            if c.offset == 0 {
                clock.mark(out.frames_written(), c.pts, c.speed);
            }
            let n = out.write(&c.samples[c.offset..]);
            c.offset += n * ch;
            total += n;
            if c.offset >= c.samples.len() {
                self.pending.pop_front();
            } else {
                break;
            }
        }
        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::AudioClock;
    use crate::output::{NullOutput, OutputConfig};
    use std::time::{Duration, Instant};

    #[test]
    fn surround_downmix_and_resample_lengths() {
        let mut p = AudioPipeline::new(PipelineConfig::new(
            44_100,
            ChannelLayout::Surround51,
            48_000,
            2,
        ));
        let pcm = vec![0.1f32; 44_100 * 6];
        p.push(&pcm, MediaTime::ZERO);
        let frames = p.pending_frames();
        assert!((47_900..=48_000).contains(&frames), "{frames}");
    }

    #[test]
    fn chunk_pts_track_input_time() {
        let mut p = AudioPipeline::new(PipelineConfig::new(
            48_000,
            ChannelLayout::Stereo,
            48_000,
            2,
        ));
        p.push(&vec![0.0; 4800 * 2], MediaTime::from_secs_f64(3.0));
        p.push(&vec![0.0; 4800 * 2], MediaTime::from_secs_f64(3.1));
        let pts: Vec<_> = p.pending.iter().map(|c| c.pts).collect();
        assert_eq!(
            pts,
            vec![MediaTime::from_secs_f64(3.0), MediaTime::from_secs_f64(3.1)]
        );
    }

    #[test]
    fn stretched_chunks_advance_by_speed() {
        let mut p = AudioPipeline::new(PipelineConfig::new(
            48_000,
            ChannelLayout::Stereo,
            44_100,
            2,
        ));
        p.set_speed(2.0);
        for i in 0..20 {
            p.push(
                &vec![0.0; 4800 * 2],
                MediaTime::from_secs_f64(i as f64 * 0.1),
            );
        }
        // Output frames ≈ input/2 resampled to 44.1k; the last chunk's pts ≈
        // 2 × its output time.
        let first = p.pending.front().unwrap().pts.as_secs_f64();
        let mut out_frames = 0usize;
        let n = p.pending.len();
        for (i, c) in p.pending.iter().enumerate() {
            if i == n - 1 {
                let expect = first + out_frames as f64 / 44_100.0 * 2.0;
                assert!(
                    (c.pts.as_secs_f64() - expect).abs() < 0.02,
                    "{} vs {expect}",
                    c.pts.as_secs_f64()
                );
                assert_eq!(c.speed, 2.0);
            }
            out_frames += c.samples.len() / 2;
        }
        let secs_out = out_frames as f64 / 44_100.0;
        assert!(
            (secs_out - 1.0).abs() < 0.05,
            "2 s of input at 2× → ~1 s, got {secs_out}"
        );
    }

    #[test]
    fn clock_follows_written_audio() {
        let mut out = NullOutput::new(OutputConfig {
            sample_rate: 48_000,
            channels: 2,
            buffer: Duration::from_secs(1),
        });
        let mut p = AudioPipeline::new(PipelineConfig::new(48_000, ChannelLayout::Mono, 48_000, 2));
        p.push(&vec![0.0; 48_000], MediaTime::from_secs_f64(42.0));
        assert_eq!(p.write_to(&mut out), 48_000);
        let t0 = Instant::now();
        std::thread::sleep(Duration::from_millis(100));
        let pos = out.clock().position().unwrap().as_secs_f64();
        let el = t0.elapsed().as_secs_f64();
        assert!(pos >= 42.05 && pos <= 42.0 + el + 0.02, "{pos}");
    }

    #[test]
    fn binaural_ambisonic_pipeline_runs() {
        let layout = ChannelLayout::Ambisonic {
            order: 1,
            norm: crate::format::AmbisonicNorm::AmbiX,
        };
        let mut p = AudioPipeline::new(PipelineConfig::new(48_000, layout, 48_000, 2));
        p.push(&vec![0.1; 4800 * 4], MediaTime::ZERO);
        assert_eq!(p.pending_frames(), 4800);
        // Latency of the convolver is compensated in the pts.
        assert!(p.pending.front().unwrap().pts < MediaTime::ZERO);
    }
}
