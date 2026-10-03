//! Streaming windowed-sinc sample-rate converter.
//!
//! A Kaiser-windowed sinc low-pass is tabulated at `PHASES` sub-sample
//! offsets; each output sample interpolates linearly between the two nearest
//! phases (arbitrary, even irrational, ratios). The cut-off sits at 0.95 of
//! the lower of the two Nyquist frequencies, the window has β = 8.6
//! (≈ 85 dB side-lobe rejection) and 32 zero crossings per side.
//!
//! Timing is exact: output frame `n` is the band-limited input signal
//! evaluated at input time `n · in_rate / out_rate`, so the caller can map
//! output frames back to media time without a latency correction. The cost is
//! that output lags input by `HALF_TAPS` input frames until [`Resampler::flush`].

use std::f64::consts::PI;

const HALF_TAPS: usize = 32;
const PHASES: usize = 256;
const KAISER_BETA: f64 = 8.6;

pub struct Resampler {
    channels: usize,
    in_rate: u32,
    out_rate: u32,
    /// Input frames advanced per output frame.
    step: f64,
    /// `(PHASES + 1) × 2·HALF_TAPS` filter table.
    table: Vec<f32>,
    /// Pending interleaved input (starts with `HALF_TAPS` zero frames).
    buf: Vec<f32>,
    /// Position (input frames, relative to `buf`) of the next output frame.
    pos: f64,
    passthrough: bool,
}

fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    for k in 1..50 {
        term *= q / (k as f64 * k as f64);
        sum += term;
        if term < 1e-12 * sum {
            break;
        }
    }
    sum
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32, channels: usize) -> Self {
        let in_rate = in_rate.max(1);
        let out_rate = out_rate.max(1);
        let channels = channels.max(1);
        let step = in_rate as f64 / out_rate as f64;
        // Cut-off relative to the input Nyquist.
        let cutoff = 0.95 * (out_rate as f64 / in_rate as f64).min(1.0);
        let taps = 2 * HALF_TAPS;
        let i0b = bessel_i0(KAISER_BETA);
        let mut table = vec![0f32; (PHASES + 1) * taps];
        for p in 0..=PHASES {
            let frac = p as f64 / PHASES as f64;
            let row = &mut table[p * taps..(p + 1) * taps];
            let mut sum = 0.0;
            let mut vals = [0f64; 2 * HALF_TAPS];
            for (k, v) in vals.iter_mut().enumerate() {
                // Distance from the evaluation point to tap k's sample.
                let t = frac + HALF_TAPS as f64 - 1.0 - k as f64;
                let x = t / HALF_TAPS as f64;
                let w = if x.abs() >= 1.0 {
                    0.0
                } else {
                    bessel_i0(KAISER_BETA * (1.0 - x * x).sqrt()) / i0b
                };
                let s = if t.abs() < 1e-12 {
                    1.0
                } else {
                    (PI * cutoff * t).sin() / (PI * cutoff * t)
                };
                *v = cutoff * s * w;
                sum += *v;
            }
            // Normalise each phase to unity DC gain.
            for (r, v) in row.iter_mut().zip(vals) {
                *r = (v / sum) as f32;
            }
        }
        let mut r = Resampler {
            channels,
            in_rate,
            out_rate,
            step,
            table,
            buf: Vec::new(),
            pos: 0.0,
            passthrough: in_rate == out_rate,
        };
        r.reset();
        r
    }

    pub fn in_rate(&self) -> u32 {
        self.in_rate
    }
    pub fn out_rate(&self) -> u32 {
        self.out_rate
    }
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Input frames advanced per output frame.
    pub fn step(&self) -> f64 {
        self.step
    }

    /// Drop all history (seek).
    pub fn reset(&mut self) {
        self.buf.clear();
        self.buf.resize(HALF_TAPS * self.channels, 0.0);
        self.pos = HALF_TAPS as f64;
    }

    /// Convert interleaved `input`, appending to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.passthrough {
            out.extend_from_slice(input);
            return;
        }
        self.buf.extend_from_slice(input);
        self.run(out);
    }

    /// Emit the tail held back for look-ahead.
    pub fn flush(&mut self, out: &mut Vec<f32>) {
        if self.passthrough {
            return;
        }
        self.buf
            .resize(self.buf.len() + HALF_TAPS * self.channels, 0.0);
        self.run(out);
        self.reset();
    }

    fn run(&mut self, out: &mut Vec<f32>) {
        let ch = self.channels;
        let taps = 2 * HALF_TAPS;
        let frames = self.buf.len() / ch;
        loop {
            let i0 = self.pos.floor() as usize;
            if i0 + HALF_TAPS >= frames {
                break;
            }
            let frac = self.pos - i0 as f64;
            let pf = frac * PHASES as f64;
            let p = (pf as usize).min(PHASES - 1);
            let a = (pf - p as f64) as f32;
            let row0 = &self.table[p * taps..(p + 1) * taps];
            let row1 = &self.table[(p + 1) * taps..(p + 2) * taps];
            let start = i0 + 1 - HALF_TAPS;
            for c in 0..ch {
                let mut acc = 0f32;
                for k in 0..taps {
                    let h = row0[k] + a * (row1[k] - row0[k]);
                    acc += h * self.buf[(start + k) * ch + c];
                }
                out.push(acc);
            }
            self.pos += self.step;
        }
        // Discard input no longer needed.
        let keep_from = (self.pos.floor() as usize + 1).saturating_sub(HALF_TAPS);
        if keep_from > 0 {
            self.buf.drain(..keep_from * ch);
            self.pos -= keep_from as f64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f64, rate: u32, secs: f64) -> Vec<f32> {
        (0..(rate as f64 * secs) as usize)
            .map(|i| (2.0 * PI * freq * i as f64 / rate as f64).sin() as f32)
            .collect()
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
    }

    fn run_all(r: &mut Resampler, x: &[f32]) -> Vec<f32> {
        let mut out = Vec::new();
        for chunk in x.chunks(997) {
            r.process(chunk, &mut out);
        }
        r.flush(&mut out);
        out
    }

    #[test]
    fn dc_gain_is_unity() {
        let mut r = Resampler::new(44_100, 48_000, 1);
        let out = run_all(&mut r, &vec![1.0; 44_100]);
        let mid = &out[1000..out.len() - 1000];
        assert!(mid.iter().all(|&s| (s - 1.0).abs() < 1e-3));
    }

    #[test]
    fn length_follows_ratio() {
        let mut r = Resampler::new(44_100, 48_000, 2);
        let out = run_all(&mut r, &vec![0.0; 44_100 * 2]);
        let frames = out.len() / 2;
        assert!((frames as i64 - 48_000).abs() <= 2, "{frames}");
    }

    #[test]
    fn passband_sine_preserved() {
        let mut r = Resampler::new(44_100, 48_000, 1);
        let out = run_all(&mut r, &sine(1000.0, 44_100, 1.0));
        let mid = &out[2000..out.len() - 2000];
        assert!(
            (rms(mid) - std::f64::consts::FRAC_1_SQRT_2).abs() < 0.005,
            "rms {}",
            rms(mid)
        );
        // Compare to the ideal sine at the output rate (timing is exact).
        let ideal = sine(1000.0, 48_000, 1.0);
        let err: f64 = (2000..out.len() - 2000)
            .map(|i| (out[i] - ideal[i]).abs() as f64)
            .fold(0.0, f64::max);
        assert!(err < 2e-3, "max error {err}");
    }

    #[test]
    fn stopband_attenuated_when_downsampling() {
        // 15 kHz is above the 11.025 kHz Nyquist of the output.
        let mut r = Resampler::new(48_000, 22_050, 1);
        let out = run_all(&mut r, &sine(15_000.0, 48_000, 1.0));
        let mid = &out[1000..out.len() - 1000];
        let db = 20.0 * (rms(mid) / std::f64::consts::FRAC_1_SQRT_2).log10();
        assert!(db < -60.0, "alias rejection only {db:.1} dB");
        // While 5 kHz passes.
        let mut r = Resampler::new(48_000, 22_050, 1);
        let out = run_all(&mut r, &sine(5_000.0, 48_000, 1.0));
        let mid = &out[1000..out.len() - 1000];
        assert!((rms(mid) - std::f64::consts::FRAC_1_SQRT_2).abs() < 0.01);
    }

    #[test]
    fn passthrough_same_rate() {
        let mut r = Resampler::new(48_000, 48_000, 2);
        let x: Vec<f32> = (0..100).map(|i| i as f32).collect();
        let mut out = Vec::new();
        r.process(&x, &mut out);
        assert_eq!(out, x);
    }
}
