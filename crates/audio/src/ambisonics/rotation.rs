//! Sound-field rotation for real spherical harmonics of any order.
//!
//! Uses the Ivanic–Ruedenberg recurrence (J. Phys. Chem. 1996, with the 1998
//! erratum): the band-`l` rotation matrix is built from band `l−1` and the
//! 3×3 Cartesian rotation. Rotation never mixes bands, and within a band
//! SN3D and N3D differ by one scalar, so the same matrices apply to AmbiX
//! (SN3D) signals unchanged.
//!
//! Semantics: [`ShRotation::from_matrix`]`(r)` turns a field containing a
//! source at direction `d` into one with the source at `r · d`.

use super::sh::channels_for_order;
use glam::{Mat3, Quat, Vec3};

#[derive(Debug, Clone, PartialEq)]
pub struct ShRotation {
    order: usize,
    /// Band `l` matrix, `(2l+1)²`, row-major, indices `m, n ∈ −l..=l`.
    bands: Vec<Vec<f32>>,
}

struct Band<'a> {
    l: i32,
    m: &'a [f32],
}

impl Band<'_> {
    fn get(&self, m: i32, n: i32) -> f32 {
        let w = 2 * self.l + 1;
        self.m[((m + self.l) * w + (n + self.l)) as usize]
    }
}

impl ShRotation {
    pub fn identity(order: usize) -> Self {
        Self::from_matrix(order, Mat3::IDENTITY)
    }

    /// Rotation for a Cartesian rotation `r` in ambisonic coordinates
    /// (+x front, +y left, +z up).
    pub fn from_matrix(order: usize, r: Mat3) -> Self {
        // Band 1 in (y, z, x) order: index m = −1 → y, 0 → z, 1 → x.
        let axis = |m: i32| [1usize, 2, 0][(m + 1) as usize];
        let rij = |i: usize, j: usize| r.col(j)[i];
        let mut r1 = vec![0f32; 9];
        for m in -1..=1 {
            for n in -1..=1 {
                r1[((m + 1) * 3 + (n + 1)) as usize] = rij(axis(m), axis(n));
            }
        }
        let mut bands = vec![vec![1.0f32], r1.clone()];
        for l in 2..=order as i32 {
            let prev = Band {
                l: l - 1,
                m: &bands[(l - 1) as usize],
            };
            let b1 = Band { l: 1, m: &r1 };
            let w = (2 * l + 1) as usize;
            let mut out = vec![0f32; w * w];
            for m in -l..=l {
                for n in -l..=l {
                    out[((m + l) as usize) * w + (n + l) as usize] =
                        Self::entry(l, m, n, &b1, &prev);
                }
            }
            bands.push(out);
        }
        bands.truncate(order + 1);
        ShRotation { order, bands }
    }

    fn p(i: i32, l: i32, a: i32, b: i32, r1: &Band, prev: &Band) -> f32 {
        let ri1 = r1.get(i, 1);
        let rim1 = r1.get(i, -1);
        let ri0 = r1.get(i, 0);
        if b == l {
            ri1 * prev.get(a, l - 1) - rim1 * prev.get(a, -l + 1)
        } else if b == -l {
            ri1 * prev.get(a, -l + 1) + rim1 * prev.get(a, l - 1)
        } else {
            ri0 * prev.get(a, b)
        }
    }

    fn entry(l: i32, m: i32, n: i32, r1: &Band, prev: &Band) -> f32 {
        let d = (m == 0) as i32 as f32;
        let denom = if n.abs() < l {
            ((l + n) * (l - n)) as f32
        } else {
            (2 * l * (2 * l - 1)) as f32
        };
        let am = m.abs();
        let u = (((l + m) * (l - m)) as f32 / denom).sqrt();
        let v =
            0.5 * ((1.0 + d) * ((l + am - 1) * (l + am)) as f32 / denom).sqrt() * (1.0 - 2.0 * d);
        let w = -0.5 * (((l - am - 1) * (l - am)) as f32 / denom).max(0.0).sqrt() * (1.0 - d);
        let mut out = 0.0;
        if u != 0.0 {
            out += u * Self::p(0, l, m, n, r1, prev);
        }
        if v != 0.0 {
            let vv = if m == 0 {
                Self::p(1, l, 1, n, r1, prev) + Self::p(-1, l, -1, n, r1, prev)
            } else if m > 0 {
                let d1 = (m == 1) as i32 as f32;
                Self::p(1, l, m - 1, n, r1, prev) * (1.0 + d1).sqrt()
                    - Self::p(-1, l, -m + 1, n, r1, prev) * (1.0 - d1)
            } else {
                let d1 = (m == -1) as i32 as f32;
                Self::p(1, l, m + 1, n, r1, prev) * (1.0 - d1)
                    + Self::p(-1, l, -m - 1, n, r1, prev) * (1.0 + d1).sqrt()
            };
            out += v * vv;
        }
        if w != 0.0 {
            let ww = if m > 0 {
                Self::p(1, l, m + 1, n, r1, prev) + Self::p(-1, l, -m - 1, n, r1, prev)
            } else {
                Self::p(1, l, m - 1, n, r1, prev) - Self::p(-1, l, -m + 1, n, r1, prev)
            };
            out += w * ww;
        }
        out
    }

    /// Rotation by a quaternion expressed in ambisonic coordinates.
    pub fn from_quat(order: usize, q: Quat) -> Self {
        Self::from_matrix(order, Mat3::from_quat(q))
    }

    /// Counter-rotation for a listener whose head orientation `head` is an
    /// OpenXR pose quaternion (+x right, +y up, −z forward, head → world).
    /// Applying it keeps the sound field fixed in the world while the head turns.
    pub fn for_head_orientation_openxr(order: usize, head: Quat) -> Self {
        let r_amb = openxr_to_ambisonic(Mat3::from_quat(head));
        Self::from_matrix(order, r_amb.transpose())
    }

    pub fn order(&self) -> usize {
        self.order
    }

    /// Band-`l` matrix (row-major, `(2l+1)²`).
    pub fn band(&self, l: usize) -> &[f32] {
        &self.bands[l]
    }

    pub fn inverse(&self) -> Self {
        let bands = self
            .bands
            .iter()
            .enumerate()
            .map(|(l, b)| {
                let w = 2 * l + 1;
                let mut t = vec![0f32; w * w];
                for i in 0..w {
                    for j in 0..w {
                        t[j * w + i] = b[i * w + j];
                    }
                }
                t
            })
            .collect();
        ShRotation {
            order: self.order,
            bands,
        }
    }

    /// Rotate one frame of ACN-ordered coefficients in place (extra channels
    /// beyond the rotation's order are left untouched).
    pub fn apply_frame(&self, frame: &mut [f32]) {
        let mut tmp = [0f32; 15];
        for (l, b) in self.bands.iter().enumerate().skip(1) {
            let w = 2 * l + 1;
            let base = l * l;
            if base + w > frame.len() {
                break;
            }
            let src = &frame[base..base + w];
            for (i, t) in tmp.iter_mut().enumerate().take(w) {
                *t = (0..w).map(|j| b[i * w + j] * src[j]).sum();
            }
            frame[base..base + w].copy_from_slice(&tmp[..w]);
        }
    }

    /// Rotate interleaved ambisonic audio (`channels` wide) in place.
    pub fn apply_interleaved(&self, samples: &mut [f32], channels: usize) {
        let n = channels.min(channels_for_order(self.order));
        for f in samples.chunks_exact_mut(channels) {
            self.apply_frame(&mut f[..n]);
        }
    }
}

/// Re-express an OpenXR-space rotation matrix in ambisonic axes.
pub fn openxr_to_ambisonic(r_xr: Mat3) -> Mat3 {
    // amb = C · xr with amb_x = −xr_z, amb_y = −xr_x, amb_z = xr_y.
    let c = Mat3::from_cols(
        Vec3::new(0.0, -1.0, 0.0),
        Vec3::new(0.0, 0.0, 1.0),
        Vec3::new(-1.0, 0.0, 0.0),
    );
    c * r_xr * c.transpose()
}

#[cfg(test)]
mod tests {
    use super::super::sh::sn3d;
    use super::*;

    fn close(a: &[f32], b: &[f32], tol: f32) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < tol)
    }

    fn some_rotations() -> Vec<Mat3> {
        vec![
            Mat3::from_rotation_z(0.7),
            Mat3::from_rotation_y(-1.1),
            Mat3::from_rotation_x(2.3),
            Mat3::from_quat(Quat::from_euler(glam::EulerRot::ZYX, 0.4, -0.9, 1.7)),
            Mat3::from_quat(Quat::from_axis_angle(
                Vec3::new(1.0, 2.0, -0.5).normalize(),
                2.9,
            )),
        ]
    }

    #[test]
    fn rotating_encoded_source_moves_it() {
        let d = Vec3::new(0.2, 0.7, -0.4).normalize();
        for r in some_rotations() {
            let rot = ShRotation::from_matrix(3, r);
            let mut field = sn3d(3, d);
            rot.apply_frame(&mut field);
            let want = sn3d(3, r * d);
            assert!(close(&field, &want, 1e-4), "{field:?} vs {want:?}");
        }
    }

    #[test]
    fn rotate_then_inverse_is_identity() {
        for r in some_rotations() {
            let rot = ShRotation::from_matrix(3, r);
            let inv = rot.inverse();
            let orig: Vec<f32> = (0..16).map(|i| (i as f32 * 0.37).sin()).collect();
            let mut v = orig.clone();
            rot.apply_frame(&mut v);
            inv.apply_frame(&mut v);
            assert!(close(&v, &orig, 1e-5));
            // The inverse also equals the rotation built from rᵀ.
            let from_t = ShRotation::from_matrix(3, r.transpose());
            for l in 0..=3 {
                assert!(close(from_t.band(l), inv.band(l), 1e-5));
            }
        }
    }

    #[test]
    fn energy_is_preserved() {
        for r in some_rotations() {
            let rot = ShRotation::from_matrix(3, r);
            let orig: Vec<f32> = (0..16).map(|i| ((i * 7 + 3) as f32).cos()).collect();
            let mut v = orig.clone();
            rot.apply_frame(&mut v);
            // Per band (so it holds for SN3D as well as N3D).
            for l in 0..=3usize {
                let e0: f32 = orig[l * l..(l + 1) * (l + 1)].iter().map(|x| x * x).sum();
                let e1: f32 = v[l * l..(l + 1) * (l + 1)].iter().map(|x| x * x).sum();
                assert!(
                    (e0 - e1).abs() < 1e-4 * e0.max(1.0),
                    "band {l}: {e0} vs {e1}"
                );
            }
        }
    }

    #[test]
    fn composition_matches_matrix_product() {
        let rs = some_rotations();
        let (a, b) = (rs[3], rs[4]);
        let mut v1 = sn3d(3, Vec3::new(0.5, -0.2, 0.3).normalize());
        let mut v2 = v1.clone();
        ShRotation::from_matrix(3, a).apply_frame(&mut v1);
        ShRotation::from_matrix(3, b).apply_frame(&mut v1);
        ShRotation::from_matrix(3, b * a).apply_frame(&mut v2);
        assert!(close(&v1, &v2, 1e-4));
    }

    #[test]
    fn head_turn_counter_rotates() {
        // A source straight ahead (+x ambisonic). Turning the head 90° left
        // (positive yaw about +y in OpenXR) should put it on the right (−y).
        let head = Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
        let rot = ShRotation::for_head_orientation_openxr(1, head);
        let mut f = sn3d(1, Vec3::X);
        rot.apply_frame(&mut f);
        assert!(close(&f, &[1.0, -1.0, 0.0, 0.0], 1e-5), "{f:?}");
    }
}
