//! Pitch-preserving time stretch (WSOLA) for 0.25×–4× playback.
//!
//! Waveform-Similarity Overlap-Add: Hann-windowed frames of `frame` samples
//! are overlap-added at a fixed synthesis hop of `frame / 2` (the periodic
//! Hann window sums to exactly one at 50 % overlap). The analysis hop is
//! `speed × synthesis hop`; each frame's analysis position is nudged within
//! ±`tolerance` to the offset whose waveform best matches the natural
//! continuation of the previous frame (normalised cross-correlation on a
//! mono mix), which keeps periodic signals phase-coherent.
//!
//! At exactly 1× the stage is bypassed. [`Wsola::input_pos_of_next_output`]
//! maps output frames back to input frames for the audio clock.

pub const MIN_SPEED: f64 = 0.25;
pub const MAX_SPEED: f64 = 4.0;

pub struct Wsola {
    channels: usize,
    frame: usize,
    hop: usize,
    tolerance: usize,
    speed: f64,
    window: Vec<f32>,
    /// Interleaved input; `in_buf[0]` is absolute input frame `in_base`.
    in_buf: Vec<f32>,
    in_base: u64,
    /// Absolute input frame of the next nominal analysis position.
    analysis: f64,
    /// Absolute input frame where the previous chosen frame started.
    prev_start: Option<u64>,
    /// Overlap accumulator (interleaved, `frame` frames).
    acc: Vec<f32>,
    bypass_pos: u64,
    mono: Vec<f32>,
    reference: Vec<f32>,
}

impl Wsola {
    /// `frame_ms` ≈ 40 ms and `tolerance_ms` ≈ 10 ms work well for speech and music.
    pub fn new(sample_rate: u32, channels: usize, frame_ms: f64, tolerance_ms: f64) -> Self {
        let frame = (((sample_rate as f64 * frame_ms / 1000.0) as usize) & !1).max(64);
        let hop = frame / 2;
        let tolerance = ((sample_rate as f64 * tolerance_ms / 1000.0) as usize).max(1);
        // Periodic Hann: w[n] + w[n + N/2] = 1.
        let window = (0..frame)
            .map(|n| {
                (0.5 - 0.5 * (2.0 * std::f64::consts::PI * n as f64 / frame as f64).cos()) as f32
            })
            .collect();
        let channels = channels.max(1);
        Wsola {
            channels,
            frame,
            hop,
            tolerance,
            speed: 1.0,
            window,
            in_buf: Vec::new(),
            in_base: 0,
            analysis: 0.0,
            prev_start: None,
            acc: vec![0.0; frame * channels],
            bypass_pos: 0,
            mono: Vec::new(),
            reference: Vec::new(),
        }
    }

    pub fn with_defaults(sample_rate: u32, channels: usize) -> Self {
        Self::new(sample_rate, channels, 40.0, 10.0)
    }

    pub fn speed(&self) -> f64 {
        self.speed
    }

    /// Change speed (clamped to 0.25–4×); takes effect from the next frame.
    pub fn set_speed(&mut self, speed: f64) {
        let s = speed.clamp(MIN_SPEED, MAX_SPEED);
        if (s == 1.0) != (self.speed == 1.0) {
            self.reset_keep_position();
        }
        self.speed = s;
    }

    fn total_input(&self) -> u64 {
        self.in_base + (self.in_buf.len() / self.channels) as u64
    }

    fn reset_keep_position(&mut self) {
        // Restart the stretch state at the current read position without
        // losing buffered input.
        let pos = if self.speed == 1.0 {
            self.bypass_pos
        } else {
            self.analysis as u64
        };
        let drop_frames = pos
            .saturating_sub(self.in_base)
            .min((self.in_buf.len() / self.channels) as u64);
        self.in_buf.drain(..drop_frames as usize * self.channels);
        self.in_base += drop_frames;
        self.analysis = self.in_base as f64;
        self.bypass_pos = self.in_base;
        self.prev_start = None;
        self.acc.fill(0.0);
    }

    /// Forget everything (seek). Input frame numbering restarts at 0.
    pub fn reset(&mut self) {
        self.in_buf.clear();
        self.in_base = 0;
        self.analysis = 0.0;
        self.prev_start = None;
        self.acc.fill(0.0);
        self.bypass_pos = 0;
    }

    /// Input frame (counted since the last reset) corresponding to the next
    /// output frame [`Wsola::process`] will emit.
    pub fn input_pos_of_next_output(&self) -> f64 {
        if self.speed == 1.0 {
            return self.bypass_pos as f64;
        }
        // Output emitted after placing frame k starts at frame k's nominal
        // analysis position, i.e. the value `analysis` advances to.
        self.analysis
    }

    /// Stretch interleaved `input`, appending to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let ch = self.channels;
        if self.speed == 1.0 {
            if !self.in_buf.is_empty() {
                // Input buffered while stretching; play it out first.
                self.bypass_pos += (self.in_buf.len() / ch) as u64;
                out.append(&mut self.in_buf);
            }
            out.extend_from_slice(input);
            self.bypass_pos += (input.len() / ch) as u64;
            self.in_base = self.bypass_pos;
            self.analysis = self.bypass_pos as f64;
            return;
        }
        self.in_buf.extend_from_slice(input);
        let ha = self.hop as f64 * self.speed;
        loop {
            let nominal = self.analysis.round() as i64;
            let lo = (nominal - self.tolerance as i64).max(self.in_base as i64) as u64;
            let hi = nominal as u64 + self.tolerance as u64;
            // Need the full search range plus the natural continuation.
            let need_end = (hi + self.frame as u64).max(
                self.prev_start
                    .map_or(0, |p| p + (self.hop + self.frame) as u64),
            );
            if need_end > self.total_input() {
                break;
            }
            let start = match self.prev_start {
                None => nominal.max(self.in_base as i64) as u64,
                Some(prev) => self.best_offset(prev + self.hop as u64, lo, hi),
            };
            // Overlap-add the chosen frame.
            let off = (start - self.in_base) as usize * ch;
            for n in 0..self.frame {
                let w = self.window[n];
                for c in 0..ch {
                    self.acc[n * ch + c] += w * self.in_buf[off + n * ch + c];
                }
            }
            // The first hop of the accumulator is now complete.
            out.extend_from_slice(&self.acc[..self.hop * ch]);
            self.acc.copy_within(self.hop * ch.., 0);
            let tail = self.acc.len() - self.hop * ch;
            self.acc[tail..].fill(0.0);
            self.prev_start = Some(start);
            self.analysis += ha;
            // Trim input that can no longer be referenced.
            let min_needed = (self.analysis as i64 - self.tolerance as i64).max(0) as u64;
            let min_needed = min_needed.min(start + self.hop as u64);
            if min_needed > self.in_base {
                let d = (min_needed - self.in_base) as usize;
                self.in_buf.drain(..d * ch);
                self.in_base = min_needed;
            }
        }
    }

    /// Search `[lo, hi]` for the start whose frame best matches the frame
    /// starting at `target` (normalised cross-correlation of mono mixes).
    /// Coarse-to-fine: a decimated search over the whole range, then a
    /// full-resolution refinement around the best coarse candidate.
    fn best_offset(&mut self, target: u64, lo: u64, hi: u64) -> u64 {
        let ch = self.channels;
        // Correlate the overlapping half of the frame.
        let len = self.hop;
        let mono_at = |buf: &[f32], base: u64, frame_idx: u64| -> f32 {
            let o = (frame_idx - base) as usize * ch;
            buf[o..o + ch].iter().sum::<f32>()
        };
        let span = (hi - lo) as usize + len;
        self.mono.clear();
        self.mono
            .extend((0..span as u64).map(|n| mono_at(&self.in_buf, self.in_base, lo + n)));
        self.reference.clear();
        self.reference
            .extend((0..len as u64).map(|n| mono_at(&self.in_buf, self.in_base, target + n)));
        let score = |mono: &[f32], reference: &[f32], off: usize, stride: usize| -> f32 {
            let mut dot = 0f32;
            let mut energy = 1e-9f32;
            let seg = &mono[off..off + len];
            for i in (0..len).step_by(stride) {
                let s = seg[i];
                dot += s * reference[i];
                energy += s * s;
            }
            dot / energy.sqrt()
        };
        let range = (hi - lo) as usize;
        const COARSE: usize = 4;
        let mut best = (f32::MIN, 0usize);
        for off in (0..=range).step_by(COARSE) {
            let sc = score(&self.mono, &self.reference, off, COARSE);
            if sc > best.0 {
                best = (sc, off);
            }
        }
        let (from, to) = (
            best.1.saturating_sub(COARSE - 1),
            (best.1 + COARSE - 1).min(range),
        );
        let mut fine = (f32::MIN, best.1);
        for off in from..=to {
            let sc = score(&self.mono, &self.reference, off, 1);
            if sc > fine.0 {
                fine = (sc, off);
            }
        }
        lo + fine.1 as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    fn sine(freq: f64, rate: u32, frames: usize, ch: usize) -> Vec<f32> {
        let mut v = Vec::with_capacity(frames * ch);
        for i in 0..frames {
            let s = (2.0 * PI * freq * i as f64 / rate as f64).sin() as f32 * 0.5;
            for _ in 0..ch {
                v.push(s);
            }
        }
        v
    }

    fn stretch(speed: f64, input: &[f32], ch: usize) -> Vec<f32> {
        let mut w = Wsola::with_defaults(48_000, ch);
        w.set_speed(speed);
        let mut out = Vec::new();
        for c in input.chunks(1024 * ch) {
            w.process(c, &mut out);
        }
        out
    }

    fn zero_crossings(x: &[f32], ch: usize) -> usize {
        let m: Vec<f32> = x.chunks(ch).map(|f| f[0]).collect();
        m.windows(2)
            .filter(|w| (w[0] <= 0.0) != (w[1] <= 0.0))
            .count()
    }

    #[test]
    fn duration_scales_with_speed() {
        let input = sine(440.0, 48_000, 96_000, 2);
        for speed in [0.25, 0.5, 1.5, 2.0, 4.0] {
            let out = stretch(speed, &input, 2);
            let expected = 96_000.0 / speed;
            let got = (out.len() / 2) as f64;
            // Within one frame + tolerance of the ideal length.
            assert!(
                (got - expected).abs() < 0.06 * 48_000.0 / speed.min(1.0),
                "speed {speed}: {got} vs {expected}"
            );
        }
    }

    #[test]
    fn pitch_is_preserved() {
        let input = sine(440.0, 48_000, 96_000, 1);
        for speed in [0.5, 2.0] {
            let out = stretch(speed, &input, 1);
            let skip = 4800;
            let body = &out[skip..out.len() - skip];
            let secs = body.len() as f64 / 48_000.0;
            let freq = zero_crossings(body, 1) as f64 / 2.0 / secs;
            assert!(
                (freq - 440.0).abs() < 440.0 * 0.03,
                "speed {speed}: measured {freq} Hz"
            );
        }
    }

    #[test]
    fn steady_amplitude() {
        // Phase-aligned overlap-add of a sine should not modulate its level.
        let input = sine(500.0, 48_000, 96_000, 1);
        let out = stretch(1.7, &input, 1);
        let body = &out[4800..out.len() - 4800];
        for block in body.chunks(2400) {
            let peak = block.iter().fold(0f32, |m, &s| m.max(s.abs()));
            assert!((0.42..0.58).contains(&peak), "peak {peak}");
        }
    }

    #[test]
    fn unity_speed_is_bit_exact() {
        let input = sine(440.0, 48_000, 4800, 2);
        assert_eq!(stretch(1.0, &input, 2), input);
    }

    #[test]
    fn output_maps_back_to_input_position() {
        let input = sine(440.0, 48_000, 96_000, 1);
        let mut w = Wsola::with_defaults(48_000, 1);
        w.set_speed(2.0);
        let mut out = Vec::new();
        w.process(&input[..48_000], &mut out);
        let pos = w.input_pos_of_next_output();
        let emitted = out.len() as f64;
        // Emitted output covers ~2× as much input.
        assert!(
            (pos - emitted * 2.0).abs() < 2000.0,
            "pos {pos}, emitted {emitted}"
        );
    }
}
