//! Multichannel → stereo downmix with ITU-R BS.775 coefficients.
//!
//! `Lo = L + g·C + g·Ls (+ g·Lb)`, `Ro = R + g·C + g·Rs (+ g·Rb)` with
//! `g = 1/√2` (−3 dB). The LFE channel is dropped by default, as BS.775
//! recommends; an optional LFE gain is available. With `normalize` the matrix
//! is scaled so that a full-scale signal on every contributing channel cannot
//! clip.

use crate::format::ChannelLayout;

/// −3 dB.
pub const MINUS_3DB: f32 = std::f32::consts::FRAC_1_SQRT_2;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DownmixOptions {
    /// Scale so the per-output coefficient sum is ≤ 1.
    pub normalize: bool,
    /// Gain applied to the LFE channel (0 = dropped, BS.775 default).
    pub lfe_gain: f32,
}

impl Default for DownmixOptions {
    fn default() -> Self {
        DownmixOptions {
            normalize: true,
            lfe_gain: 0.0,
        }
    }
}

/// A static `outputs × inputs` mixing matrix.
#[derive(Debug, Clone, PartialEq)]
pub struct DownmixMatrix {
    pub inputs: usize,
    pub outputs: usize,
    /// Row-major: `coeffs[out * inputs + in]`.
    pub coeffs: Vec<f32>,
}

impl DownmixMatrix {
    /// ITU stereo downmix for a (non-ambisonic) layout.
    pub fn itu_stereo(layout: ChannelLayout, opts: DownmixOptions) -> DownmixMatrix {
        let g = MINUS_3DB;
        let (inputs, l, r): (usize, Vec<f32>, Vec<f32>) = match layout {
            ChannelLayout::Mono => (1, vec![g], vec![g]),
            ChannelLayout::Stereo => (2, vec![1.0, 0.0], vec![0.0, 1.0]),
            // L R C LFE Ls Rs
            ChannelLayout::Surround51 => (
                6,
                vec![1.0, 0.0, g, opts.lfe_gain, g, 0.0],
                vec![0.0, 1.0, g, opts.lfe_gain, 0.0, g],
            ),
            // L R C LFE Lb Rb Ls Rs
            ChannelLayout::Surround71 => (
                8,
                vec![1.0, 0.0, g, opts.lfe_gain, g, 0.0, g, 0.0],
                vec![0.0, 1.0, g, opts.lfe_gain, 0.0, g, 0.0, g],
            ),
            ChannelLayout::Other(n) => {
                let n = n.max(1) as usize;
                let mut l = vec![0.0; n];
                let mut r = vec![0.0; n];
                l[0] = 1.0;
                r[n.min(2) - 1] = 1.0;
                (n, l, r)
            }
            ChannelLayout::Ambisonic { order, .. } => {
                // Not a speaker layout; a crude W±Y "stereo" fallback for when
                // the binaural renderer is disabled.
                let n = (order as usize + 1).pow(2);
                let mut l = vec![0.0; n];
                let mut r = vec![0.0; n];
                l[0] = 0.5;
                r[0] = 0.5;
                l[1] = 0.5;
                r[1] = -0.5;
                (n, l, r)
            }
        };
        let mut coeffs = [l, r].concat();
        if opts.normalize && !matches!(layout, ChannelLayout::Mono) {
            let max_sum = (0..2)
                .map(|o| {
                    coeffs[o * inputs..(o + 1) * inputs]
                        .iter()
                        .map(|c| c.abs())
                        .sum::<f32>()
                })
                .fold(0.0f32, f32::max);
            if max_sum > 1.0 {
                coeffs.iter_mut().for_each(|c| *c /= max_sum);
            }
        }
        DownmixMatrix {
            inputs,
            outputs: 2,
            coeffs,
        }
    }

    /// Mix interleaved `input` (`inputs` wide) into `output` (`outputs`
    /// wide), replacing its contents.
    pub fn apply(&self, input: &[f32], output: &mut Vec<f32>) {
        let frames = input.len() / self.inputs;
        output.clear();
        output.reserve(frames * self.outputs);
        for f in input.chunks_exact(self.inputs) {
            for o in 0..self.outputs {
                let row = &self.coeffs[o * self.inputs..(o + 1) * self.inputs];
                output.push(row.iter().zip(f).map(|(c, s)| c * s).sum());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw() -> DownmixOptions {
        DownmixOptions {
            normalize: false,
            lfe_gain: 0.0,
        }
    }

    fn mix_one(m: &DownmixMatrix, ch: usize) -> (f32, f32) {
        let mut input = vec![0.0; m.inputs];
        input[ch] = 1.0;
        let mut out = Vec::new();
        m.apply(&input, &mut out);
        (out[0], out[1])
    }

    #[test]
    fn itu_51_gains() {
        let m = DownmixMatrix::itu_stereo(ChannelLayout::Surround51, raw());
        assert_eq!(mix_one(&m, 0), (1.0, 0.0));
        assert_eq!(mix_one(&m, 1), (0.0, 1.0));
        let (l, r) = mix_one(&m, 2);
        assert!((l - 0.70710677).abs() < 1e-6 && (r - 0.70710677).abs() < 1e-6);
        assert_eq!(mix_one(&m, 3), (0.0, 0.0), "LFE dropped");
        assert_eq!(mix_one(&m, 4), (MINUS_3DB, 0.0));
        assert_eq!(mix_one(&m, 5), (0.0, MINUS_3DB));
        // Centre stays centred at equal power: L² + R² = 1.
        let (l, r) = mix_one(&m, 2);
        assert!((l * l + r * r - 1.0).abs() < 1e-6);
    }

    #[test]
    fn itu_71_gains() {
        let m = DownmixMatrix::itu_stereo(ChannelLayout::Surround71, raw());
        assert_eq!(mix_one(&m, 4), (MINUS_3DB, 0.0), "Lb → left");
        assert_eq!(mix_one(&m, 7), (0.0, MINUS_3DB), "Rs → right");
    }

    #[test]
    fn normalized_cannot_clip() {
        for layout in [ChannelLayout::Surround51, ChannelLayout::Surround71] {
            let m = DownmixMatrix::itu_stereo(layout, DownmixOptions::default());
            let input = vec![1.0; m.inputs];
            let mut out = Vec::new();
            m.apply(&input, &mut out);
            assert!(out.iter().all(|&s| s <= 1.0 + 1e-6), "{layout:?} {out:?}");
            assert!(out[0] > 0.99, "full scale preserved");
        }
        // 5.1 normalisation factor is 1 + 2·(1/√2).
        let m = DownmixMatrix::itu_stereo(ChannelLayout::Surround51, DownmixOptions::default());
        assert!((m.coeffs[0] - 1.0 / (1.0 + 2.0 * MINUS_3DB)).abs() < 1e-6);
    }

    #[test]
    fn lfe_gain_option() {
        let m = DownmixMatrix::itu_stereo(
            ChannelLayout::Surround51,
            DownmixOptions {
                normalize: false,
                lfe_gain: 0.5,
            },
        );
        assert_eq!(mix_one(&m, 3), (0.5, 0.5));
    }

    #[test]
    fn mono_to_stereo_equal_power() {
        let m = DownmixMatrix::itu_stereo(ChannelLayout::Mono, DownmixOptions::default());
        let (l, r) = mix_one(&m, 0);
        assert!((l * l + r * r - 1.0).abs() < 1e-6);
    }
}
