//! Ambisonic → virtual-loudspeaker decoding.
//!
//! A mode-matching (pseudo-inverse) decoder onto a near-uniform Fibonacci
//! sphere of virtual speakers, with optional max-rE order weighting. The
//! matrix is scaled so a plane wave of unit amplitude produces unit total
//! speaker energy. Used by the binaural renderer (speaker feeds are
//! convolved with per-direction HRIRs) but usable for real arrays too.

use super::sh::{acn_order, channels_for_order, sn3d, sn3d_to_n3d};
use glam::Vec3;

/// `n` near-uniformly distributed unit vectors (golden-angle spiral).
pub fn fibonacci_sphere(n: usize) -> Vec<Vec3> {
    let ga = std::f32::consts::PI * (3.0 - 5f32.sqrt());
    (0..n)
        .map(|i| {
            let z = 1.0 - 2.0 * (i as f32 + 0.5) / n as f32;
            let r = (1.0 - z * z).max(0.0).sqrt();
            let t = ga * i as f32;
            Vec3::new(r * t.cos(), r * t.sin(), z)
        })
        .collect()
}

/// Legendre polynomial P_l(x).
fn legendre(l: usize, x: f32) -> f32 {
    let (mut p0, mut p1) = (1.0f32, x);
    if l == 0 {
        return 1.0;
    }
    for k in 2..=l {
        let p2 = ((2 * k - 1) as f32 * x * p1 - (k - 1) as f32 * p0) / k as f32;
        p0 = p1;
        p1 = p2;
    }
    p1
}

/// max-rE per-order weights (Zotter & Frank): `P_l(cos(137.9° / (N + 1.51)))`.
pub fn max_re_weights(order: usize) -> Vec<f32> {
    let re = (137.9f32.to_radians() / (order as f32 + 1.51)).cos();
    (0..=order).map(|l| legendre(l, re)).collect()
}

/// Solve `a · x = b` in place for a small dense SPD-ish matrix (Gauss–Jordan
/// with partial pivoting). `a` is `n×n` row-major, `b` is `n×m`.
fn solve(n: usize, a: &mut [f64], b: &mut [f64], m: usize) {
    for col in 0..n {
        let piv = (col..n)
            .max_by(|&i, &j| a[i * n + col].abs().total_cmp(&a[j * n + col].abs()))
            .unwrap();
        if piv != col {
            for k in 0..n {
                a.swap(col * n + k, piv * n + k);
            }
            for k in 0..m {
                b.swap(col * m + k, piv * m + k);
            }
        }
        let d = a[col * n + col];
        for k in 0..n {
            a[col * n + k] /= d;
        }
        for k in 0..m {
            b[col * m + k] /= d;
        }
        for r in 0..n {
            if r != col {
                let f = a[r * n + col];
                if f != 0.0 {
                    for k in 0..n {
                        a[r * n + k] -= f * a[col * n + k];
                    }
                    for k in 0..m {
                        b[r * m + k] -= f * b[col * m + k];
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct VirtualSpeakerDecoder {
    order: usize,
    speakers: Vec<Vec3>,
    /// `speakers × channels`, row-major; input is ACN/SN3D.
    matrix: Vec<f32>,
}

impl VirtualSpeakerDecoder {
    /// Decoder for `order` onto `speakers` directions.
    pub fn new(order: usize, speakers: Vec<Vec3>, max_re: bool) -> Self {
        let k = channels_for_order(order);
        let l = speakers.len();
        assert!(l >= k, "need at least {k} speakers for order {order}");
        // Y: k × l, N3D.
        let mut y = vec![0f64; k * l];
        for (s, d) in speakers.iter().enumerate() {
            for (c, v) in sn3d(order, *d).iter().enumerate() {
                y[c * l + s] = (*v * sn3d_to_n3d(c)) as f64;
            }
        }
        // D = Yᵀ (Y Yᵀ)⁻¹  →  solve (Y Yᵀ) X = Y, D = Xᵀ.
        let mut yyt = vec![0f64; k * k];
        for i in 0..k {
            for j in 0..k {
                yyt[i * k + j] = (0..l).map(|s| y[i * l + s] * y[j * l + s]).sum();
            }
        }
        let mut x = y.clone();
        solve(k, &mut yyt, &mut x, l);
        let weights = if max_re {
            max_re_weights(order)
        } else {
            vec![1.0; order + 1]
        };
        let mut matrix = vec![0f32; l * k];
        for s in 0..l {
            for c in 0..k {
                // Input is SN3D: convert to N3D before applying the N3D decoder.
                matrix[s * k + c] = (x[c * l + s] as f32) * sn3d_to_n3d(c) * weights[acn_order(c)];
            }
        }
        let mut dec = VirtualSpeakerDecoder {
            order,
            speakers,
            matrix,
        };
        // Normalise: mean plane-wave speaker energy = 1.
        let probe = fibonacci_sphere(256);
        let mean_e: f32 = probe
            .iter()
            .map(|d| {
                dec.gains(&sn3d(order, *d))
                    .iter()
                    .map(|g| g * g)
                    .sum::<f32>()
            })
            .sum::<f32>()
            / probe.len() as f32;
        let s = 1.0 / mean_e.sqrt();
        dec.matrix.iter_mut().for_each(|v| *v *= s);
        dec
    }

    /// Default layout for binaural use: enough well-spread speakers that the
    /// HRIR interpolation is smooth for the order, mirrored left/right so a
    /// frontal source renders with identical ear signals.
    pub fn for_binaural(order: usize) -> Self {
        let n = (2 * channels_for_order(order)).max(12);
        let base = fibonacci_sphere(n);
        let mirrored = base.iter().map(|d| Vec3::new(d.x, -d.y, d.z));
        Self::new(order, base.iter().copied().chain(mirrored).collect(), true)
    }

    pub fn order(&self) -> usize {
        self.order
    }
    pub fn speakers(&self) -> &[Vec3] {
        &self.speakers
    }
    /// Row-major `speakers × channels` matrix (SN3D input).
    pub fn matrix(&self) -> &[f32] {
        &self.matrix
    }

    /// Speaker gains for one frame of SN3D coefficients.
    pub fn gains(&self, frame: &[f32]) -> Vec<f32> {
        let k = channels_for_order(self.order);
        self.matrix
            .chunks(k)
            .map(|row| row.iter().zip(frame).map(|(a, b)| a * b).sum())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn energy_is_direction_independent() {
        for order in 1..=3 {
            let dec = VirtualSpeakerDecoder::for_binaural(order);
            let energies: Vec<f32> = fibonacci_sphere(300)
                .iter()
                .map(|d| {
                    dec.gains(&sn3d(order, *d))
                        .iter()
                        .map(|g| g * g)
                        .sum::<f32>()
                })
                .collect();
            let (lo, hi) = energies
                .iter()
                .fold((f32::MAX, 0f32), |(a, b), &e| (a.min(e), b.max(e)));
            let spread_db = 10.0 * (hi / lo).log10();
            assert!(
                spread_db < 0.5,
                "order {order}: energy spread {spread_db:.2} dB"
            );
            let mean = energies.iter().sum::<f32>() / energies.len() as f32;
            assert!((mean - 1.0).abs() < 0.05);
        }
    }

    #[test]
    fn energy_vector_points_at_source() {
        let dec = VirtualSpeakerDecoder::for_binaural(3);
        for d in [Vec3::X, Vec3::Y, Vec3::new(-0.3, 0.4, 0.86).normalize()] {
            let g = dec.gains(&sn3d(3, d));
            let re: Vec3 = dec
                .speakers()
                .iter()
                .zip(&g)
                .map(|(s, gi)| *s * gi * gi)
                .sum::<Vec3>()
                / g.iter().map(|x| x * x).sum::<f32>();
            assert!(re.normalize().dot(d) > 0.99, "rE {re:?} for {d:?}");
            assert!(re.length() > 0.8, "3rd-order max-rE |rE| = {}", re.length());
        }
    }

    #[test]
    fn max_re_weights_known_values() {
        let w = max_re_weights(1);
        assert_eq!(w[0], 1.0);
        assert!((w[1] - 0.577).abs() < 0.01, "{w:?}");
    }
}
