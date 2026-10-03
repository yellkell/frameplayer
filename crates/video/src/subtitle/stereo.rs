//! Stereo subtitle placement: per-eye horizontal disparity that makes a
//! subtitle appear at a chosen depth (HereSphere-style "subtitle depth").
//!
//! * **Flat screen** at distance `D`: subtitles are drawn on the screen
//!   plane. For a point straight ahead at depth `d`, the ray from an eye at
//!   `x = ±ipd/2` crosses the plane at `x_e · (1 − D/d)`, so each eye's copy
//!   is shifted by that amount (crossed disparity when `d < D`).
//! * **Immersive** (equirect / fisheye / EAC / mesh): stereo footage has
//!   parallel axes, i.e. zero disparity at infinity, so each eye's copy is
//!   rotated toward the nose by `atan((ipd/2)/d)` and the shift is expressed
//!   as a fraction of that eye image's horizontal field of view.

use fp_core::Projection;

/// Default subtitle depth (metres) — comfortable reading distance.
pub const DEFAULT_DEPTH_M: f32 = 2.0;
/// Typical adult IPD.
pub const DEFAULT_IPD_M: f32 = 0.063;

/// Horizontal shift for each eye's copy of a subtitle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EyeOffsets {
    /// Shift as a fraction of the eye image width (positive = right).
    pub left_uv: f32,
    pub right_uv: f32,
    /// Shift on the screen plane in metres (flat projection), 0 otherwise.
    pub left_m: f32,
    pub right_m: f32,
    /// Per-eye convergence angle toward the nose, radians.
    pub angle_rad: f32,
}

/// Disparity for a subtitle at `depth_m` given the projection and IPD.
pub fn disparity_for(projection: &Projection, depth_m: f32, ipd_m: f32) -> EyeOffsets {
    let depth = depth_m.max(0.1);
    let half = ipd_m.max(0.0) / 2.0;
    let angle = (half / depth).atan();
    match projection {
        Projection::Flat {
            width_m,
            distance_m,
            curvature,
        } => {
            let d_screen = distance_m.max(0.1);
            // Left eye at −half: x = −half·(1 − D/d); right eye mirrors it.
            let left_m = -half * (1.0 - d_screen / depth);
            let right_m = -left_m;
            // A curved screen's width is its arc length; near the centre the
            // flat approximation holds.
            let _ = curvature;
            let w = width_m.max(0.01);
            EyeOffsets {
                left_uv: left_m / w,
                right_uv: right_m / w,
                left_m,
                right_m,
                angle_rad: angle,
            }
        }
        other => {
            let fov = match other {
                Projection::Equirect { h_fov_deg } => h_fov_deg.to_radians(),
                Projection::Fisheye { fov_deg, .. } => fov_deg.to_radians(),
                // EAC / custom meshes: subtitles are drawn on a 360° sphere layer.
                _ => std::f32::consts::TAU,
            };
            let shift = angle / fov.max(0.01);
            EyeOffsets {
                left_uv: shift,
                right_uv: -shift,
                left_m: 0.0,
                right_m: 0.0,
                angle_rad: angle,
            }
        }
    }
}

/// World-space text height (m) that keeps a constant angular size
/// (`angular_height_deg`) when the subtitle is placed at `depth_m`.
pub fn text_height_for_depth(depth_m: f32, angular_height_deg: f32) -> f32 {
    2.0 * depth_m.max(0.1) * (angular_height_deg.to_radians() / 2.0).tan()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_screen_disparity() {
        let screen = Projection::Flat {
            width_m: 4.0,
            distance_m: 3.0,
            curvature: 0.0,
        };
        // At the screen depth: no shift.
        let o = disparity_for(&screen, 3.0, 0.064);
        assert!(o.left_m.abs() < 1e-6 && o.right_m.abs() < 1e-6);
        // Closer than the screen: crossed disparity (left copy moves right).
        let o = disparity_for(&screen, 1.5, 0.064);
        assert!((o.left_m - 0.032).abs() < 1e-6, "{o:?}");
        assert!((o.right_m + 0.032).abs() < 1e-6);
        assert!((o.left_uv - 0.008).abs() < 1e-6);
        // Behind the screen: uncrossed.
        assert!(disparity_for(&screen, 6.0, 0.064).left_m < 0.0);
    }

    #[test]
    fn immersive_disparity() {
        let o = disparity_for(&Projection::EQUIRECT_180, 2.0, 0.064);
        let angle = (0.032f32 / 2.0).atan();
        assert!((o.angle_rad - angle).abs() < 1e-6);
        assert!((o.left_uv - angle / std::f32::consts::PI).abs() < 1e-6);
        assert_eq!(o.right_uv, -o.left_uv);
        // Further away → smaller shift; 360° halves the UV shift.
        assert!(disparity_for(&Projection::EQUIRECT_180, 10.0, 0.064).left_uv < o.left_uv);
        let o360 = disparity_for(&Projection::EQUIRECT_360, 2.0, 0.064);
        assert!((o360.left_uv * 2.0 - o.left_uv).abs() < 1e-6);
    }

    #[test]
    fn constant_angular_size() {
        let h1 = text_height_for_depth(1.0, 3.0);
        let h2 = text_height_for_depth(2.0, 3.0);
        assert!((h2 / h1 - 2.0).abs() < 1e-5);
    }
}
