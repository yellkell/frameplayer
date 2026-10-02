//! Pitch-preserving time-stretch (WSOLA) for playback at other speeds.
//!
//! Each step emits `hop` output frames by cross-fading the tail of the
//! previous segment with the input segment, near the nominal analysis
//! position, that best matches it (normalised cross-correlation within
//! ±`search` frames). The analysis position advances `hop * speed` per step.

pub struct Stretch {
    channels: usize,
    speed: f64,
    hop: usize,
    search: usize,
    /// Buffered input, interleaved; `input[0]` is input frame `base`.
    input: Vec<f32>,
    base: u64,
    /// Nominal analysis position in input frames.
    analysis: f64,
    /// Start of the previous chosen segment's second half (input frames).
    prev_tail: Option<u64>,
    window: Vec<f32>,
}

impl Stretch {
    pub fn new(channels: u32, rate: u32) -> Stretch {
        let hop = (rate as usize * 20) / 1000; // 20 ms
        let window = (0..hop)
            .map(|i| 0.5 - 0.5 * (std::f32::consts::PI * (i as f32 + 0.5) / hop as f32).cos())
            .collect();
        Stretch {
            channels: channels.max(1) as usize,
            speed: 1.0,
            hop,
            search: (rate as usize * 8) / 1000,
            input: Vec::new(),
            base: 0,
            analysis: 0.0,
            prev_tail: None,
            window,
        }
    }

    pub fn speed(&self) -> f64 {
        self.speed
    }

    pub fn set_speed(&mut self, speed: f64) {
        self.speed = speed.clamp(0.25, 4.0);
    }

    /// Forgets buffered audio (after a seek).
    pub fn reset(&mut self) {
        self.input.clear();
        self.base = 0;
        self.analysis = 0.0;
        self.prev_tail = None;
    }

    fn frames(&self) -> u64 {
        (self.input.len() / self.channels) as u64
    }

    fn mono(&self, frame: u64) -> f32 {
        let i = ((frame - self.base) as usize) * self.channels;
        self.input[i..i + self.channels].iter().sum::<f32>()
    }

    /// Feeds input; returns stretched output (may be empty while buffering).
    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        if (self.speed - 1.0).abs() < 1e-3 && self.prev_tail.is_none() && self.input.is_empty() {
            return input.to_vec();
        }
        self.input.extend_from_slice(input);
        let mut out = Vec::new();
        let (hop, search, ch) = (self.hop as u64, self.search as u64, self.channels);
        loop {
            let nominal = self.analysis.round() as u64 + self.base.max(search);
            let lo = nominal.saturating_sub(search).max(self.base);
            if lo + 2 * search + 2 * hop > self.base + self.frames() {
                break; // need more input
            }
            // Choose the segment start best matching the previous tail.
            let start = match self.prev_tail {
                None => nominal,
                Some(tail) => {
                    let mut best = (f32::MIN, nominal);
                    let step = 2;
                    let mut cand = lo;
                    while cand <= nominal + search {
                        let (mut xy, mut yy) = (0.0f32, 1e-9f32);
                        let mut k = 0;
                        while k < hop {
                            let a = self.mono(tail + k);
                            let b = self.mono(cand + k);
                            xy += a * b;
                            yy += b * b;
                            k += 4;
                        }
                        let score = xy / yy.sqrt();
                        if score > best.0 {
                            best = (score, cand);
                        }
                        cand += step;
                    }
                    best.1
                }
            };
            // Cross-fade previous tail (fading out) with the new segment.
            for k in 0..hop as usize {
                let w = self.window[k];
                for c in 0..ch {
                    let new = self.input[((start - self.base) as usize + k) * ch + c];
                    let old = match self.prev_tail {
                        Some(t) => self.input[((t - self.base) as usize + k) * ch + c],
                        None => new,
                    };
                    out.push(old * (1.0 - w) + new * w);
                }
            }
            self.prev_tail = Some(start + hop);
            self.analysis += hop as f64 * self.speed;
            // Drop input nobody will read again.
            let keep_from = (self.prev_tail.unwrap_or(0))
                .min((self.analysis.round() as u64 + self.base).saturating_sub(search));
            if keep_from > self.base + 4 * hop {
                let drop = (keep_from - self.base) as usize;
                self.input.drain(..drop * ch);
                self.analysis -= drop as f64;
                self.base += drop as u64;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(freq: f32, secs: f32) -> Vec<f32> {
        (0..(48000.0 * secs) as usize)
            .flat_map(|i| {
                let v = (2.0 * std::f32::consts::PI * freq * i as f32 / 48000.0).sin() * 0.5;
                [v, v]
            })
            .collect()
    }

    /// Dominant frequency by zero-crossing count.
    fn freq(stereo: &[f32]) -> f32 {
        let mono: Vec<f32> = stereo.chunks(2).map(|c| c[0]).collect();
        let crossings = mono
            .windows(2)
            .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
            .count();
        crossings as f32 / (mono.len() as f32 / 48000.0)
    }

    #[test]
    fn length_scales_and_pitch_is_kept() {
        for speed in [0.5, 1.5, 2.0] {
            let mut s = Stretch::new(2, 48000);
            s.set_speed(speed);
            let input = tone(440.0, 2.0);
            let mut out = Vec::new();
            for chunk in input.chunks(2 * 1024) {
                out.extend(s.process(chunk));
            }
            let expected = input.len() as f64 / speed;
            let ratio = out.len() as f64 / expected;
            assert!(
                (0.85..1.05).contains(&ratio),
                "speed {speed}: ratio {ratio}"
            );
            let f = freq(&out);
            assert!((f - 440.0).abs() < 15.0, "speed {speed}: pitch {f}");
        }
    }

    #[test]
    fn unity_speed_passes_through() {
        let mut s = Stretch::new(2, 48000);
        let input = tone(440.0, 0.1);
        assert_eq!(s.process(&input), input);
    }
}
