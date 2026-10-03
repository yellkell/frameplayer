//! Procedurally generated generic HRIRs (spherical-head + pinna model).
//!
//! Brown & Duda, "A structural model for binaural sound synthesis" (1998):
//!
//! * head shadow: one-pole/one-zero filter per ear,
//!   `H(ω,θ) = (1 + jα(θ)ω/2ω₀) / (1 + jω/2ω₀)`, `ω₀ = c/a`,
//!   `α(θ) = 1.05 + 0.95·cos(θ/150°·180°)` where θ is the angle between the
//!   source and the ear axis;
//! * interaural delay: Woodworth's formula on a sphere of radius `a`;
//! * pinna: five elevation/azimuth-dependent echoes (gains halved here for a
//!   gentler, more "generic" colouration).
//!
//! Responses are synthesised directly in the frequency domain (exact
//! fractional delays), diffuse-field equalised over the sphere, and
//! transformed to short real impulse responses. No dataset is embedded.

use glam::Vec3;
use rustfft::num_complex::Complex32;
use rustfft::FftPlanner;
use std::f32::consts::PI;

const HEAD_RADIUS: f32 = 0.0875;
const SPEED_OF_SOUND: f32 = 343.0;

/// Generic HRIR generator.
#[derive(Debug, Clone)]
pub struct SphericalHeadHrtf {
    sample_rate: u32,
    /// Impulse-response length in samples.
    pub len: usize,
    pre_delay: f32,
}

impl SphericalHeadHrtf {
    pub fn new(sample_rate: u32) -> Self {
        let len = if sample_rate > 50_000 { 512 } else { 256 };
        SphericalHeadHrtf {
            sample_rate,
            len,
            pre_delay: 0.0005,
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Raw (un-equalised) complex response for `ear` (0 = left at +y,
    /// 1 = right at −y), FFT size `2·len`, bins `0..=len`.
    fn spectrum(&self, dir: Vec3, ear: usize) -> Vec<Complex32> {
        let n = 2 * self.len;
        let fs = self.sample_rate as f32;
        let d = dir.normalize_or_zero();
        let ear_axis = if ear == 0 { Vec3::Y } else { Vec3::NEG_Y };
        let theta = d.dot(ear_axis).clamp(-1.0, 1.0).acos();
        // Head shadow.
        let alpha_min = 0.1f32;
        let theta_min = 150f32.to_radians();
        let alpha =
            (1.0 + alpha_min / 2.0) + (1.0 - alpha_min / 2.0) * (theta / theta_min * PI).cos();
        let w0 = SPEED_OF_SOUND / HEAD_RADIUS;
        // Woodworth delay, relative to the head centre, made positive.
        let a_c = HEAD_RADIUS / SPEED_OF_SOUND;
        let tau = if theta < PI / 2.0 {
            -a_c * theta.cos()
        } else {
            a_c * (theta - PI / 2.0)
        } + a_c
            + self.pre_delay;
        // Pinna echoes; azimuth/elevation in the head frame.
        let az = d.y.atan2(d.x) * if ear == 0 { 1.0 } else { -1.0 };
        let el = d.z.clamp(-1.0, 1.0).asin();
        const RHO: [f32; 5] = [0.5, -1.0, 0.5, -0.25, 0.25];
        const A: [f32; 5] = [1.0, 5.0, 5.0, 5.0, 5.0];
        const B: [f32; 5] = [2.0, 4.0, 7.0, 11.0, 13.0];
        const D: [f32; 5] = [1.0, 0.5, 0.5, 0.5, 0.5];
        let echoes: Vec<(f32, f32)> = (0..5)
            .map(|k| {
                // Paper values are in samples at 44.1 kHz.
                let t = (A[k] * (az / 2.0).cos() * (D[k] * (PI / 2.0 - el)).sin() + B[k]).max(0.0)
                    / 44_100.0;
                (0.5 * RHO[k], t)
            })
            .collect();
        (0..=self.len)
            .map(|bin| {
                let w = 2.0 * PI * bin as f32 * fs / n as f32;
                let shadow = Complex32::new(1.0, alpha * w / (2.0 * w0))
                    / Complex32::new(1.0, w / (2.0 * w0));
                let mut pinna = Complex32::new(1.0, 0.0);
                for &(g, t) in &echoes {
                    pinna += Complex32::from_polar(g, -w * t);
                }
                shadow * pinna * Complex32::from_polar(1.0, -w * tau)
            })
            .collect()
    }

    /// Diffuse-field-equalised HRIR pairs `[left, right]` for each direction.
    pub fn hrirs(&self, dirs: &[Vec3]) -> Vec<[Vec<f32>; 2]> {
        let n = 2 * self.len;
        let specs: Vec<[Vec<Complex32>; 2]> = dirs
            .iter()
            .map(|d| [self.spectrum(*d, 0), self.spectrum(*d, 1)])
            .collect();
        // Diffuse-field average power per bin over a uniform probe sphere.
        let probe = super::decoder::fibonacci_sphere(200);
        let mut avg = vec![0f32; self.len + 1];
        for d in &probe {
            for ear in 0..2 {
                for (a, h) in avg.iter_mut().zip(self.spectrum(*d, ear)) {
                    *a += h.norm_sqr();
                }
            }
        }
        // Light smoothing to avoid sharp EQ peaks from pinna notches.
        let raw: Vec<f32> = avg.iter().map(|a| a / (2 * probe.len()) as f32).collect();
        let eq: Vec<f32> = (0..raw.len())
            .map(|i| {
                let lo = i.saturating_sub(3);
                let hi = (i + 3).min(raw.len() - 1);
                let m = raw[lo..=hi].iter().sum::<f32>() / (hi - lo + 1) as f32;
                1.0 / m.max(1e-6).sqrt()
            })
            .collect();
        let mut planner = FftPlanner::<f32>::new();
        let ifft = planner.plan_fft_inverse(n);
        let fade = self.len / 4;
        specs
            .into_iter()
            .map(|pair| {
                pair.map(|half| {
                    let mut full = vec![Complex32::default(); n];
                    for (i, h) in half.iter().enumerate() {
                        full[i] = h * eq[i];
                    }
                    // Hermitian symmetry → real impulse response.
                    full[self.len].im = 0.0;
                    full[0].im = 0.0;
                    for i in 1..self.len {
                        full[n - i] = full[i].conj();
                    }
                    ifft.process(&mut full);
                    let mut ir: Vec<f32> =
                        full[..self.len].iter().map(|c| c.re / n as f32).collect();
                    // Fade the tail to suppress circular wrap-around.
                    for k in 0..fade {
                        ir[self.len - 1 - k] *= 0.5 - 0.5 * (PI * k as f32 / fade as f32).cos();
                    }
                    ir
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn energy(x: &[f32]) -> f32 {
        x.iter().map(|v| v * v).sum()
    }

    fn onset(x: &[f32]) -> usize {
        let peak = x.iter().fold(0f32, |m, v| m.max(v.abs()));
        x.iter().position(|v| v.abs() > 0.3 * peak).unwrap()
    }

    #[test]
    fn lateral_source_has_ild_and_itd() {
        let h = SphericalHeadHrtf::new(48_000);
        let irs = h.hrirs(&[Vec3::Y, Vec3::X]);
        let [l, r] = &irs[0];
        assert!(
            energy(l) > 2.0 * energy(r),
            "left source louder in left ear"
        );
        let itd = onset(r) as i64 - onset(l) as i64;
        // Woodworth max ITD ≈ (a/c)(1 + π/2) ≈ 0.66 ms ≈ 31 samples.
        assert!((20..=40).contains(&itd), "ITD {itd} samples");
        let [fl, fr] = &irs[1];
        assert!(
            (energy(fl) / energy(fr) - 1.0).abs() < 0.01,
            "frontal source is symmetric"
        );
        assert_eq!(onset(fl), onset(fr));
    }

    #[test]
    fn diffuse_field_average_is_flat_ish() {
        let h = SphericalHeadHrtf::new(48_000);
        let dirs = crate::ambisonics::decoder::fibonacci_sphere(100);
        let irs = h.hrirs(&dirs);
        let mean_e: f32 = irs
            .iter()
            .map(|p| energy(&p[0]) + energy(&p[1]))
            .sum::<f32>()
            / (2 * irs.len()) as f32;
        // Unit average power per bin ⇒ IR energy ≈ 1/2 of the band (≈ 1).
        assert!((0.5..2.0).contains(&mean_e), "mean HRIR energy {mean_e}");
    }
}
