//! Real spherical harmonics (ACN order, SN3D / N3D) up to 3rd order, and
//! FuMa ↔ AmbiX conversion.
//!
//! Coordinates follow the ambisonic convention: +x front, +y left, +z up.

use glam::Vec3;

/// Highest order supported by the closed-form evaluation and FuMa tables.
pub const MAX_ORDER: usize = 3;

/// Number of channels for a full-sphere order.
pub const fn channels_for_order(order: usize) -> usize {
    (order + 1) * (order + 1)
}

/// Order (degree `l`) of an ACN channel index.
pub fn acn_order(acn: usize) -> usize {
    (acn as f64).sqrt().floor() as usize
}

/// Evaluate SN3D real SH at unit direction `d` for ACN `0..channels_for_order(order)`.
pub fn sn3d(order: usize, d: Vec3) -> Vec<f32> {
    assert!(order <= MAX_ORDER, "order {order} > {MAX_ORDER}");
    let d = d.normalize_or_zero();
    let (x, y, z) = (d.x, d.y, d.z);
    let s3 = 3f32.sqrt();
    let mut v = vec![1.0, y, z, x];
    if order >= 2 {
        v.extend_from_slice(&[
            s3 * x * y,
            s3 * y * z,
            0.5 * (3.0 * z * z - 1.0),
            s3 * x * z,
            0.5 * s3 * (x * x - y * y),
        ]);
    }
    if order >= 3 {
        let a = (5.0f32 / 8.0).sqrt();
        let b = 15f32.sqrt();
        let c = (3.0f32 / 8.0).sqrt();
        v.extend_from_slice(&[
            a * y * (3.0 * x * x - y * y),
            b * x * y * z,
            c * y * (5.0 * z * z - 1.0),
            0.5 * z * (5.0 * z * z - 3.0),
            c * x * (5.0 * z * z - 1.0),
            0.5 * b * z * (x * x - y * y),
            a * x * (x * x - 3.0 * y * y),
        ]);
    }
    v.truncate(channels_for_order(order));
    v
}

/// SN3D → N3D gain for an ACN channel (`√(2l+1)`).
pub fn sn3d_to_n3d(acn: usize) -> f32 {
    ((2 * acn_order(acn) + 1) as f32).sqrt()
}

/// FuMa channel `i` (W X Y Z R S T U V K L M N O P Q) → ACN index.
pub const FUMA_TO_ACN: [usize; 16] = [0, 3, 1, 2, 6, 7, 5, 8, 4, 12, 13, 11, 14, 10, 15, 9];

/// FuMa weight relative to SN3D for each FuMa channel: `fuma = w · sn3d`.
pub fn fuma_weight(fuma_index: usize) -> f32 {
    let s = |v: f32| v.sqrt();
    match fuma_index {
        0 => std::f32::consts::FRAC_1_SQRT_2,
        1..=3 => 1.0,
        4 => 1.0,
        5..=8 => 2.0 / s(3.0),
        9 => 1.0,
        10 | 11 => s(45.0 / 32.0),
        12 | 13 => 3.0 / s(5.0),
        14 | 15 => s(8.0 / 5.0),
        _ => 1.0,
    }
}

/// Convert one frame of FuMa channels (in FuMa order) to AmbiX (ACN/SN3D).
pub fn fuma_to_ambix(fuma: &[f32], ambix: &mut [f32]) {
    let n = fuma.len().min(16).min(ambix.len());
    ambix.iter_mut().for_each(|v| *v = 0.0);
    for (i, &f) in fuma.iter().enumerate().take(n) {
        ambix[FUMA_TO_ACN[i]] = f / fuma_weight(i);
    }
}

/// Convert one frame of AmbiX (ACN/SN3D) channels to FuMa order and weights.
pub fn ambix_to_fuma(ambix: &[f32], fuma: &mut [f32]) {
    let n = ambix.len().min(16).min(fuma.len());
    for (i, f) in fuma.iter_mut().enumerate().take(n) {
        *f = ambix[FUMA_TO_ACN[i]] * fuma_weight(i);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fibonacci-sphere sample directions.
    pub(crate) fn sphere_points(n: usize) -> Vec<Vec3> {
        let ga = std::f32::consts::PI * (3.0 - 5f32.sqrt());
        (0..n)
            .map(|i| {
                let z = 1.0 - 2.0 * (i as f32 + 0.5) / n as f32;
                let r = (1.0 - z * z).sqrt();
                let t = ga * i as f32;
                Vec3::new(r * t.cos(), r * t.sin(), z)
            })
            .collect()
    }

    #[test]
    fn n3d_is_orthonormal_over_sphere() {
        // ∫ Yi Yj dΩ / 4π = δij for N3D.
        let pts = sphere_points(20_000);
        let n = channels_for_order(3);
        let mut gram = vec![0f64; n * n];
        for p in &pts {
            let y: Vec<f32> = sn3d(3, *p)
                .iter()
                .enumerate()
                .map(|(i, v)| v * sn3d_to_n3d(i))
                .collect();
            for i in 0..n {
                for j in 0..n {
                    gram[i * n + j] += (y[i] * y[j]) as f64;
                }
            }
        }
        for i in 0..n {
            for j in 0..n {
                let g = gram[i * n + j] / pts.len() as f64;
                let want = if i == j { 1.0 } else { 0.0 };
                assert!((g - want).abs() < 0.01, "gram[{i}][{j}] = {g}");
            }
        }
    }

    #[test]
    fn sn3d_first_order_values() {
        let v = sn3d(1, Vec3::X);
        assert_eq!(v, vec![1.0, 0.0, 0.0, 1.0]);
        let v = sn3d(1, Vec3::Y);
        assert_eq!(v, vec![1.0, 1.0, 0.0, 0.0]);
        // SN3D: every order's peak magnitude is 1.
        for p in sphere_points(500) {
            assert!(sn3d(3, p).iter().all(|v| v.abs() <= 1.0 + 1e-5));
        }
    }

    #[test]
    fn fuma_roundtrip_and_w_gain() {
        let d = Vec3::new(0.3, -0.5, 0.8).normalize();
        let amb = sn3d(3, d);
        let mut fuma = vec![0.0; 16];
        ambix_to_fuma(&amb, &mut fuma);
        assert!(
            (fuma[0] - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6,
            "FuMa W is -3 dB"
        );
        assert!(
            (fuma[1] - d.x).abs() < 1e-6
                && (fuma[2] - d.y).abs() < 1e-6
                && (fuma[3] - d.z).abs() < 1e-6
        );
        let mut back = vec![0.0; 16];
        fuma_to_ambix(&fuma, &mut back);
        for (a, b) in amb.iter().zip(&back) {
            assert!((a - b).abs() < 1e-5);
        }
    }

    #[test]
    fn fuma_max_gain_normalisation() {
        // FuMa components are max-normalised: each channel peaks at 1 over
        // the sphere (except W at 1/√2).
        let pts = sphere_points(20_000);
        let mut peak = [0f32; 16];
        for p in pts {
            let mut f = [0f32; 16];
            ambix_to_fuma(&sn3d(3, p), &mut f);
            for i in 0..16 {
                peak[i] = peak[i].max(f[i].abs());
            }
        }
        for (i, &p) in peak.iter().enumerate().skip(1) {
            assert!((p - 1.0).abs() < 0.02, "FuMa channel {i} peak {p}");
        }
    }
}
