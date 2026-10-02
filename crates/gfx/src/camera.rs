//! Eye views and Vulkan-convention projection matrices built from OpenXR
//! field-of-view angles. Kept free of OpenXR types so fp-gfx does not depend
//! on fp-xr; the app copies the four angles and the pose across.

use glam::{Mat4, Quat, Vec3};

/// Asymmetric field of view in radians (OpenXR `XrFovf` semantics:
/// `angle_left`/`angle_down` are normally negative).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Fov {
    pub angle_left: f32,
    pub angle_right: f32,
    pub angle_up: f32,
    pub angle_down: f32,
}

impl Fov {
    /// Symmetric FOV, handy for tests and previews.
    pub fn symmetric(h_deg: f32, v_deg: f32) -> Fov {
        let h = h_deg.to_radians() / 2.0;
        let v = v_deg.to_radians() / 2.0;
        Fov {
            angle_left: -h,
            angle_right: h,
            angle_up: v,
            angle_down: -v,
        }
    }
}

/// One eye's pose in the app's reference space plus its FOV.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct EyeView {
    pub position: Vec3,
    pub orientation: Quat,
    pub fov: Fov,
}

impl EyeView {
    /// World → view matrix. With `rotation_only` the translation is dropped
    /// so the projection sphere stays centred on the head.
    pub fn view_matrix(&self, rotation_only: bool) -> Mat4 {
        let t = if rotation_only {
            Vec3::ZERO
        } else {
            self.position
        };
        Mat4::from_rotation_translation(self.orientation, t).inverse()
    }
}

/// Vulkan clip-space projection (y down, z in `[0, 1]`) for an asymmetric
/// FOV. Mirrors `XrMatrix4x4f_CreateProjectionFov(GRAPHICS_VULKAN)` from the
/// OpenXR SDK.
pub fn projection_matrix(fov: Fov, near: f32, far: f32) -> Mat4 {
    let tan_l = fov.angle_left.tan();
    let tan_r = fov.angle_right.tan();
    let tan_u = fov.angle_up.tan();
    let tan_d = fov.angle_down.tan();
    let tan_w = tan_r - tan_l;
    let tan_h = tan_d - tan_u; // Vulkan: +y is down
    Mat4::from_cols_array(&[
        2.0 / tan_w,
        0.0,
        0.0,
        0.0,
        0.0,
        2.0 / tan_h,
        0.0,
        0.0,
        (tan_r + tan_l) / tan_w,
        (tan_u + tan_d) / tan_h,
        -far / (far - near),
        -1.0,
        0.0,
        0.0,
        -(far * near) / (far - near),
        0.0,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ndc(m: Mat4, p: Vec3) -> Vec3 {
        let c = m * p.extend(1.0);
        c.truncate() / c.w
    }

    #[test]
    fn symmetric_fov_edges() {
        let fov = Fov::symmetric(90.0, 90.0);
        let m = projection_matrix(fov, 0.1, 100.0);
        assert!(ndc(m, Vec3::new(0.0, 0.0, -1.0)).truncate().length() < 1e-6);
        let top = ndc(m, Vec3::new(0.0, 1.0, -1.0));
        assert!(
            (top.y + 1.0).abs() < 1e-5,
            "top must be -1 in Vulkan: {top:?}"
        );
        let right = ndc(m, Vec3::new(1.0, 0.0, -1.0));
        assert!((right.x - 1.0).abs() < 1e-5);
        assert!(ndc(m, Vec3::new(0.0, 0.0, -0.1)).z.abs() < 1e-5);
        assert!((ndc(m, Vec3::new(0.0, 0.0, -100.0)).z - 1.0).abs() < 1e-5);
    }

    #[test]
    fn asymmetric_fov_edges() {
        let fov = Fov {
            angle_left: -0.9,
            angle_right: 0.7,
            angle_up: 0.8,
            angle_down: -0.85,
        };
        let m = projection_matrix(fov, 0.05, 1000.0);
        let left = ndc(m, Vec3::new(-(0.9f32).tan(), 0.0, -1.0));
        assert!((left.x + 1.0).abs() < 1e-5);
        let bottom = ndc(m, Vec3::new(0.0, -(0.85f32).tan(), -1.0));
        assert!((bottom.y - 1.0).abs() < 1e-5);
    }

    #[test]
    fn view_matrix_inverts_pose() {
        let e = EyeView {
            position: Vec3::new(0.03, 1.6, 0.2),
            orientation: Quat::from_rotation_y(0.5),
            fov: Fov::default(),
        };
        let v = e.view_matrix(false);
        assert!(v.transform_point3(e.position).length() < 1e-5);
        let r = e.view_matrix(true);
        assert!(r.transform_point3(Vec3::ZERO).length() < 1e-6);
        // Looking along the eye's forward gives view-space -Z.
        let fwd = e.orientation * Vec3::NEG_Z;
        assert!((r.transform_vector3(fwd) - Vec3::NEG_Z).length() < 1e-5);
    }
}
