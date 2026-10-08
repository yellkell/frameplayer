//! Projection and view matrices following OpenXR conventions, for Vulkan clip
//! space (x right, y down, depth 0..1).

use glam::{Mat4, Quat, Vec3};

/// Field of view as OpenXR reports it: angles in radians, left and down
/// negative.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fov {
    pub left: f32,
    pub right: f32,
    pub up: f32,
    pub down: f32,
}

impl Fov {
    /// Symmetric field of view, `deg` degrees wide and tall.
    pub fn symmetric(deg: f32) -> Fov {
        let h = (deg / 2.0).to_radians();
        Fov {
            left: -h,
            right: h,
            up: h,
            down: -h,
        }
    }
}

/// Port of `XrMatrix4x4f_CreateProjectionFov` for Vulkan.
pub fn projection(fov: Fov, near: f32, far: f32) -> Mat4 {
    let tan_l = fov.left.tan();
    let tan_r = fov.right.tan();
    let tan_d = fov.down.tan();
    let tan_u = fov.up.tan();
    let w = tan_r - tan_l;
    let h = tan_d - tan_u; // Vulkan: y points down
    let mut m = [0.0f32; 16];
    m[0] = 2.0 / w;
    m[8] = (tan_r + tan_l) / w;
    m[5] = 2.0 / h;
    m[9] = (tan_u + tan_d) / h;
    m[10] = -far / (far - near);
    m[14] = -(far * near) / (far - near);
    m[11] = -1.0;
    Mat4::from_cols_array(&m)
}

/// World → eye transform for an eye at `position` with `orientation`.
pub fn view(position: Vec3, orientation: Quat) -> Mat4 {
    Mat4::from_rotation_translation(orientation, position).inverse()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_maps_to_centre_and_up_to_top() {
        let p = projection(Fov::symmetric(90.0), 0.05, 100.0);
        let c = p * glam::Vec4::new(0.0, 0.0, -1.0, 1.0);
        assert!((c.x / c.w).abs() < 1e-6 && (c.y / c.w).abs() < 1e-6);
        let up = p * glam::Vec4::new(0.0, 0.5, -1.0, 1.0);
        assert!(
            up.y / up.w < 0.0,
            "up should be towards -y in Vulkan clip space"
        );
        let z = p * glam::Vec4::new(0.0, 0.0, -0.05, 1.0);
        assert!((z.z / z.w).abs() < 1e-5, "near plane maps to depth 0");
    }
}
