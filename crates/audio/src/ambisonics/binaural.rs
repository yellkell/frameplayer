//! Head-tracked binaural rendering of AmbiX / FuMa sound fields.
//!
//! The virtual-speaker decoder and per-speaker HRIRs are folded into one
//! filter per (SH channel, ear) at construction:
//! `F[c][ear] = Σ_s D[s][c] · hrir[s][ear]`. Run time is then a rotation of
//! the SH signals plus a `channels × 2` partitioned convolution, independent
//! of the virtual speaker count. Head orientation changes are cross-faded
//! across each processed chunk so fast head turns do not click.

use super::decoder::VirtualSpeakerDecoder;
use super::hrtf::SphericalHeadHrtf;
use super::rotation::ShRotation;
use super::sh::{channels_for_order, fuma_to_ambix, MAX_ORDER};
use crate::convolve::PartitionedConvolver;
use crate::format::AmbisonicNorm;
use glam::Quat;

pub struct BinauralRenderer {
    order: usize,
    norm: AmbisonicNorm,
    conv: PartitionedConvolver,
    current: ShRotation,
    target: ShRotation,
    planar: Vec<Vec<f32>>,
    planar_old: Vec<f32>,
    out: Vec<Vec<f32>>,
    frame_buf: Vec<f32>,
}

impl BinauralRenderer {
    /// `order` is clamped to 1..=3.
    pub fn new(order: usize, norm: AmbisonicNorm, sample_rate: u32) -> Self {
        let order = order.clamp(1, MAX_ORDER);
        let k = channels_for_order(order);
        let dec = VirtualSpeakerDecoder::for_binaural(order);
        let hrirs = SphericalHeadHrtf::new(sample_rate).hrirs(dec.speakers());
        let len = hrirs[0][0].len();
        let mut filters = vec![vec![vec![0f32; len]; 2]; k];
        for (s, pair) in hrirs.iter().enumerate() {
            for (c, f) in filters.iter_mut().enumerate() {
                let g = dec.matrix()[s * k + c];
                for (ear, h) in pair.iter().enumerate() {
                    for (dst, &v) in f[ear].iter_mut().zip(h) {
                        *dst += g * v;
                    }
                }
            }
        }
        let block = if sample_rate > 50_000 { 512 } else { 256 };
        BinauralRenderer {
            order,
            norm,
            conv: PartitionedConvolver::new(block, &filters),
            current: ShRotation::identity(order),
            target: ShRotation::identity(order),
            planar: vec![Vec::new(); k],
            planar_old: Vec::new(),
            out: vec![Vec::new(), Vec::new()],
            frame_buf: vec![0.0; 16],
        }
    }

    pub fn order(&self) -> usize {
        self.order
    }

    /// Output latency in samples.
    pub fn latency(&self) -> usize {
        self.conv.latency()
    }

    /// Head orientation as an OpenXR pose quaternion (head → world).
    pub fn set_head_orientation(&mut self, head: Quat) {
        self.target = ShRotation::for_head_orientation_openxr(self.order, head);
    }

    /// Set the sound-field rotation directly (ambisonic axes).
    pub fn set_rotation(&mut self, rot: ShRotation) {
        self.target = rot;
    }

    pub fn reset(&mut self) {
        self.conv.reset();
    }

    /// Render interleaved ambisonic input (`channels` wide, ≥ the order's
    /// channel count) to interleaved stereo appended to `out`.
    pub fn process(&mut self, input: &[f32], channels: usize, out: &mut Vec<f32>) {
        let k = channels_for_order(self.order);
        let frames = input.len() / channels.max(1);
        if frames == 0 || channels < k.min(4) {
            return;
        }
        let used = k.min(channels);
        for p in &mut self.planar {
            p.clear();
            p.resize(frames, 0.0);
        }
        self.planar_old.resize(frames * k, 0.0);
        let changed = self.current != self.target;
        for (i, f) in input.chunks_exact(channels).enumerate() {
            let fb = &mut self.frame_buf[..k];
            fb.fill(0.0);
            match self.norm {
                AmbisonicNorm::AmbiX => fb[..used].copy_from_slice(&f[..used]),
                AmbisonicNorm::FuMa => fuma_to_ambix(&f[..used], fb),
            }
            if changed {
                self.planar_old[i * k..(i + 1) * k].copy_from_slice(fb);
                self.current
                    .apply_frame(&mut self.planar_old[i * k..(i + 1) * k]);
            }
            self.target.apply_frame(fb);
            for (p, &v) in self.planar.iter_mut().zip(fb.iter()) {
                p[i] = v;
            }
        }
        if changed {
            // Linear cross-fade from the old to the new orientation.
            for i in 0..frames {
                let t = (i + 1) as f32 / frames as f32;
                for c in 0..k {
                    let old = self.planar_old[i * k + c];
                    self.planar[c][i] = old + t * (self.planar[c][i] - old);
                }
            }
            self.current = self.target.clone();
        }
        let refs: Vec<&[f32]> = self.planar.iter().map(|v| v.as_slice()).collect();
        self.conv.process(&refs, &mut self.out);
        out.reserve(frames * 2);
        for i in 0..frames {
            out.push(self.out[0][i]);
            out.push(self.out[1][i]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::sh::sn3d;
    use super::*;
    use glam::Vec3;

    fn render(r: &mut BinauralRenderer, dir: Vec3, frames: usize) -> (f32, f32) {
        // Noise burst encoded at `dir`.
        let k = channels_for_order(r.order());
        let enc = sn3d(r.order(), dir);
        let mut seed = 7u32;
        let mut input = Vec::with_capacity(frames * k);
        for _ in 0..frames {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let s = (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
            input.extend(enc.iter().map(|g| g * s));
        }
        let mut out = Vec::new();
        r.process(&input, k, &mut out);
        let skip = 2048 * 2;
        let (mut el, mut er) = (0f32, 0f32);
        for f in out[skip..].chunks(2) {
            el += f[0] * f[0];
            er += f[1] * f[1];
        }
        (el, er)
    }

    #[test]
    fn left_source_is_louder_left() {
        for order in 1..=3 {
            let mut r = BinauralRenderer::new(order, AmbisonicNorm::AmbiX, 48_000);
            let (l, rr) = render(&mut r, Vec3::Y, 16_384);
            assert!(l > 2.0 * rr, "order {order}: L {l} R {rr}");
            let (l, rr) = render(&mut r, Vec3::NEG_Y, 16_384);
            assert!(rr > 2.0 * l, "order {order}: L {l} R {rr}");
        }
    }

    #[test]
    fn frontal_source_is_balanced() {
        let mut r = BinauralRenderer::new(3, AmbisonicNorm::AmbiX, 48_000);
        let (l, rr) = render(&mut r, Vec3::X, 16_384);
        assert!((l / rr - 1.0).abs() < 0.05, "L {l} R {rr}");
    }

    #[test]
    fn head_tracking_keeps_source_world_fixed() {
        // Source on the left; turn the head 90° left → source now in front.
        let mut r = BinauralRenderer::new(2, AmbisonicNorm::AmbiX, 48_000);
        r.set_head_orientation(Quat::from_rotation_y(std::f32::consts::FRAC_PI_2));
        let _ = render(&mut r, Vec3::Y, 4096); // settle the cross-fade
        let (l, rr) = render(&mut r, Vec3::Y, 16_384);
        assert!((l / rr - 1.0).abs() < 0.1, "L {l} R {rr}");
    }

    #[test]
    fn fuma_input_matches_ambix() {
        let mut a = BinauralRenderer::new(1, AmbisonicNorm::AmbiX, 48_000);
        let mut f = BinauralRenderer::new(1, AmbisonicNorm::FuMa, 48_000);
        let enc = sn3d(1, Vec3::new(0.3, 0.8, 0.2).normalize());
        let mut fuma = [0f32; 4];
        super::super::sh::ambix_to_fuma(&enc, &mut fuma);
        let n = 2048;
        let ia: Vec<f32> = (0..n)
            .flat_map(|i| enc.iter().map(move |g| g * ((i as f32) * 0.05).sin()))
            .collect();
        let ifu: Vec<f32> = (0..n)
            .flat_map(|i| fuma.iter().map(move |g| g * ((i as f32) * 0.05).sin()))
            .collect();
        let (mut oa, mut of) = (Vec::new(), Vec::new());
        a.process(&ia, 4, &mut oa);
        f.process(&ifu, 4, &mut of);
        assert!(oa.iter().zip(&of).all(|(x, y)| (x - y).abs() < 1e-4));
    }
}
