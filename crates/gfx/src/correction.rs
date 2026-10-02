//! Per-eye picture corrections: CPU reference implementation of what the
//! projection shader (`shaders/projection.wgsl`) does, plus the push-constant
//! block that feeds it.
//!
//! Pipeline for one fragment, given the mesh's base UV in eye-image space:
//! 1. zoom about the eye-image centre (`zoom > 1` magnifies),
//! 2. radial Brown–Conrady distortion `p · (1 + k1·r² + k2·r⁴)` with `r = 1`
//!    at the edge midpoints,
//! 3. crop mask: anything outside the cropped eye image (or outside `[0,1]²`)
//!    is transparent — cropping hides borders instead of stretching content,
//! 4. map into the frame through the stereo eye rectangle.
//!
//! Orientation corrections (yaw/pitch/roll, IPD, alignment) rotate the mesh
//! and are folded into the per-eye MVP on the CPU ([`eye_rotation`]).

use bytemuck::{Pod, Zeroable};
use fp_core::{Corrections, StereoMode};
use glam::{EulerRot, Mat4, Quat};

/// Flag bit in [`EyePush::lens`]`[3]`: apply the sRGB OETF in the shader
/// because the target is a UNORM (non-sRGB) image.
pub const FLAG_ENCODE_SRGB: u32 = 1;

/// Push constants for the projection pass (exactly 128 bytes, the minimum
/// `maxPushConstantsSize` every Vulkan implementation guarantees).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct EyePush {
    /// Column-major clip-from-mesh matrix (projection · view · model · eye rotation).
    pub mvp: [[f32; 4]; 4],
    /// Eye sub-rectangle in frame UV `(u0, v0, u1, v1)`.
    pub uv_rect: [f32; 4],
    /// Visible eye-image region after crop `(u_min, v_min, u_max, v_max)`.
    pub crop: [f32; 4],
    /// `(zoom, k1, k2, flags as f32 bits)`.
    pub lens: [f32; 4],
    /// Premultiplied RGBA multiplier (dimming / fades).
    pub tint: [f32; 4],
}

/// Lens part of the corrections, in the form both CPU and shader use.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LensParams {
    pub zoom: f32,
    pub k1: f32,
    pub k2: f32,
    /// `(u_min, v_min, u_max, v_max)`.
    pub crop: [f32; 4],
}

impl LensParams {
    pub fn from_corrections(c: &Corrections) -> LensParams {
        let [l, t, r, b] = c.crop.map(|x| x.clamp(0.0, 0.49));
        LensParams {
            zoom: if c.zoom > 1e-3 { c.zoom } else { 1.0 },
            k1: c.k1,
            k2: c.k2,
            crop: [l, t, 1.0 - r, 1.0 - b],
        }
    }
}

/// Zoom + radial distortion in eye-image space (no masking).
pub fn distort_uv(uv: [f32; 2], lens: &LensParams) -> [f32; 2] {
    let px = (uv[0] - 0.5) / lens.zoom;
    let py = (uv[1] - 0.5) / lens.zoom;
    let r2 = 4.0 * (px * px + py * py);
    let f = 1.0 + lens.k1 * r2 + lens.k2 * r2 * r2;
    [0.5 + px * f, 0.5 + py * f]
}

/// Full eye-space correction: `None` if the fragment is masked out.
pub fn correct_uv(uv: [f32; 2], lens: &LensParams) -> Option<[f32; 2]> {
    let d = distort_uv(uv, lens);
    let [u0, v0, u1, v1] = lens.crop;
    let inside = d[0] >= u0 && d[0] <= u1 && d[1] >= v0 && d[1] <= v1;
    inside.then_some(d)
}

/// Eye-image UV → frame UV through the stereo sub-rectangle.
pub fn frame_uv(eye_uv: [f32; 2], rect: [f32; 4]) -> [f32; 2] {
    [
        rect[0] + eye_uv[0] * (rect[2] - rect[0]),
        rect[1] + eye_uv[1] * (rect[3] - rect[1]),
    ]
}

/// Content rotation for `eye` (0 = left, 1 = right).
///
/// * yaw/pitch/roll rotate the whole sphere (positive yaw turns content to
///   the left, positive pitch moves it up, positive roll counter-clockwise),
/// * `ipd_deg` is split ±½ in yaw, pushing the eye images apart,
/// * `vertical_align_deg` is split ±½ in pitch,
/// * `horizontal_align_deg` shifts the right eye only, fixing a relative
///   horizontal misalignment without moving the left eye.
pub fn eye_rotation(c: &Corrections, eye: usize) -> Quat {
    let side = if eye == 0 { 1.0 } else { -1.0 };
    let mut yaw = c.yaw_deg + side * c.ipd_deg * 0.5;
    if eye != 0 {
        yaw -= c.horizontal_align_deg;
    }
    let pitch = c.pitch_deg + side * c.vertical_align_deg * 0.5;
    Quat::from_euler(
        EulerRot::YXZ,
        yaw.to_radians(),
        pitch.to_radians(),
        c.roll_deg.to_radians(),
    )
}

impl EyePush {
    /// Assemble the push block for one eye. `view_proj` is projection · view;
    /// `model` places the mesh (identity for head-centred spheres).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        view_proj: Mat4,
        model: Mat4,
        stereo: StereoMode,
        swap_eyes: bool,
        eye: usize,
        corrections: &Corrections,
        encode_srgb: bool,
        tint: [f32; 4],
    ) -> EyePush {
        let lens = LensParams::from_corrections(corrections);
        let mvp = view_proj * model * Mat4::from_quat(eye_rotation(corrections, eye));
        let flags = if encode_srgb { FLAG_ENCODE_SRGB } else { 0 };
        EyePush {
            mvp: mvp.to_cols_array_2d(),
            uv_rect: stereo.eye_rect(eye, swap_eyes),
            crop: lens.crop,
            lens: [lens.zoom, lens.k1, lens.k2, f32::from_bits(flags)],
            tint,
        }
    }

    /// CPU mirror of the fragment shader's UV path: base mesh UV → frame UV,
    /// `None` when masked.
    pub fn sample_uv(&self, base_uv: [f32; 2]) -> Option<[f32; 2]> {
        let lens = LensParams {
            zoom: self.lens[0],
            k1: self.lens[1],
            k2: self.lens[2],
            crop: self.crop,
        };
        correct_uv(base_uv, &lens).map(|d| frame_uv(d, self.uv_rect))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::Vec3;

    fn neutral() -> LensParams {
        LensParams::from_corrections(&Corrections::default())
    }

    #[test]
    fn push_block_is_128_bytes() {
        assert_eq!(std::mem::size_of::<EyePush>(), 128);
    }

    #[test]
    fn neutral_is_identity() {
        for uv in [[0.0, 0.0], [0.3, 0.7], [1.0, 1.0]] {
            assert_eq!(correct_uv(uv, &neutral()), Some(uv));
        }
    }

    #[test]
    fn zoom_magnifies_about_centre() {
        let lens = LensParams {
            zoom: 2.0,
            ..neutral()
        };
        assert_eq!(distort_uv([0.5, 0.5], &lens), [0.5, 0.5]);
        assert_eq!(distort_uv([1.0, 0.5], &lens), [0.75, 0.5]);
    }

    #[test]
    fn barrel_distortion_pushes_edges_out() {
        let lens = LensParams {
            k1: 0.1,
            k2: 0.01,
            ..neutral()
        };
        // r = 1 at the edge midpoint: factor 1 + 0.1 + 0.01.
        let d = distort_uv([1.0, 0.5], &lens);
        assert!((d[0] - (0.5 + 0.5 * 1.11)).abs() < 1e-6);
        assert_eq!(
            correct_uv([1.0, 0.5], &lens),
            None,
            "pushed outside → masked"
        );
        assert_eq!(distort_uv([0.5, 0.5], &lens), [0.5, 0.5]);
    }

    #[test]
    fn crop_masks_edges() {
        let c = Corrections {
            crop: [0.1, 0.0, 0.2, 0.05],
            ..Default::default()
        };
        let lens = LensParams::from_corrections(&c);
        assert_eq!(lens.crop, [0.1, 0.0, 0.8, 0.95]);
        assert_eq!(correct_uv([0.05, 0.5], &lens), None);
        assert_eq!(correct_uv([0.85, 0.5], &lens), None);
        assert_eq!(correct_uv([0.5, 0.97], &lens), None);
        assert_eq!(correct_uv([0.5, 0.5], &lens), Some([0.5, 0.5]));
    }

    #[test]
    fn frame_uv_follows_stereo_rect() {
        let p = EyePush::new(
            Mat4::IDENTITY,
            Mat4::IDENTITY,
            StereoMode::Sbs,
            false,
            1,
            &Corrections::default(),
            false,
            [1.0; 4],
        );
        assert_eq!(p.sample_uv([0.0, 0.0]), Some([0.5, 0.0]));
        assert_eq!(p.sample_uv([1.0, 1.0]), Some([1.0, 1.0]));
        let ou = EyePush::new(
            Mat4::IDENTITY,
            Mat4::IDENTITY,
            StereoMode::Ou,
            true,
            1,
            &Corrections::default(),
            true,
            [1.0; 4],
        );
        assert_eq!(ou.sample_uv([0.5, 1.0]), Some([0.5, 0.5]));
        assert_eq!(ou.lens[3].to_bits(), FLAG_ENCODE_SRGB);
    }

    #[test]
    fn eye_rotations() {
        let neutral = Corrections::default();
        assert!(eye_rotation(&neutral, 0).angle_between(Quat::IDENTITY) < 1e-6);
        let c = Corrections {
            ipd_deg: 2.0,
            ..Default::default()
        };
        let l = eye_rotation(&c, 0) * Vec3::NEG_Z;
        let r = eye_rotation(&c, 1) * Vec3::NEG_Z;
        assert!(
            l.x < 0.0 && r.x > 0.0,
            "IPD pushes left content left, right content right"
        );
        assert!((l.angle_between(r).to_degrees() - 2.0).abs() < 1e-3);
        let v = Corrections {
            vertical_align_deg: 1.0,
            ..Default::default()
        };
        assert!((eye_rotation(&v, 0) * Vec3::NEG_Z).y > 0.0);
        assert!((eye_rotation(&v, 1) * Vec3::NEG_Z).y < 0.0);
        let h = Corrections {
            horizontal_align_deg: 1.0,
            ..Default::default()
        };
        assert!(eye_rotation(&h, 0).angle_between(Quat::IDENTITY) < 1e-6);
        assert!((eye_rotation(&h, 1) * Vec3::NEG_Z).x > 0.0);
        let p = Corrections {
            pitch_deg: 10.0,
            yaw_deg: 30.0,
            ..Default::default()
        };
        let f = eye_rotation(&p, 0) * Vec3::NEG_Z;
        assert!(f.y > 0.0 && f.x < 0.0);
    }
}
