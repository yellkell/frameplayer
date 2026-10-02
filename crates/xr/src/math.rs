//! Pose math shared by the XR layer and the app: conversions between OpenXR
//! and glam, rays, yaw extraction and recentering. All pure and tested.

use glam::{Quat, Vec2, Vec3};
use openxr as xr;

/// A rigid transform in metres (OpenXR convention: -Z forward, +Y up).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pose {
    pub position: Vec3,
    pub orientation: Quat,
}

impl Default for Pose {
    fn default() -> Self {
        Pose::IDENTITY
    }
}

impl Pose {
    pub const IDENTITY: Pose = Pose {
        position: Vec3::ZERO,
        orientation: Quat::IDENTITY,
    };

    pub fn new(position: Vec3, orientation: Quat) -> Pose {
        Pose {
            position,
            orientation,
        }
    }

    pub fn from_xr(p: &xr::Posef) -> Pose {
        let o = p.orientation;
        let q = Quat::from_xyzw(o.x, o.y, o.z, o.w);
        // Runtimes may hand back an all-zero quaternion for invalid poses.
        let q = if q.length_squared() > 1e-8 {
            q.normalize()
        } else {
            Quat::IDENTITY
        };
        Pose {
            position: Vec3::new(p.position.x, p.position.y, p.position.z),
            orientation: q,
        }
    }

    pub fn to_xr(&self) -> xr::Posef {
        xr::Posef {
            orientation: xr::Quaternionf {
                x: self.orientation.x,
                y: self.orientation.y,
                z: self.orientation.z,
                w: self.orientation.w,
            },
            position: xr::Vector3f {
                x: self.position.x,
                y: self.position.y,
                z: self.position.z,
            },
        }
    }

    /// Forward direction (-Z of the pose).
    pub fn forward(&self) -> Vec3 {
        self.orientation * Vec3::NEG_Z
    }

    pub fn transform_point(&self, p: Vec3) -> Vec3 {
        self.position + self.orientation * p
    }

    /// `self ∘ other`: `other` expressed in `self`'s frame, moved to the parent frame.
    pub fn mul(&self, other: &Pose) -> Pose {
        Pose {
            position: self.transform_point(other.position),
            orientation: (self.orientation * other.orientation).normalize(),
        }
    }

    pub fn inverse(&self) -> Pose {
        let inv = self.orientation.inverse();
        Pose {
            position: inv * -self.position,
            orientation: inv,
        }
    }

    pub fn to_mat4(&self) -> glam::Mat4 {
        glam::Mat4::from_rotation_translation(self.orientation, self.position)
    }

    /// Pointing ray along the pose's -Z axis.
    pub fn ray(&self) -> Ray {
        Ray {
            origin: self.position,
            dir: self.forward().normalize(),
        }
    }

    /// Heading around +Y in radians (0 = looking down -Z, positive = turned left).
    pub fn yaw(&self) -> f32 {
        yaw_of(self.orientation)
    }
}

/// Yaw of an orientation around +Y, robust to pitch up to (but excluding) ±90°
/// and still meaningful when looking nearly straight up/down (uses the
/// up vector in that case).
pub fn yaw_of(q: Quat) -> f32 {
    let f = q * Vec3::NEG_Z;
    let flat = Vec2::new(f.x, f.z);
    if flat.length_squared() > 1e-6 {
        (-f.x).atan2(-f.z)
    } else {
        // Looking straight up/down: the head's up vector points along the
        // horizontal heading (or opposite).
        let u = q * Vec3::Y;
        let s = if f.y > 0.0 { -1.0 } else { 1.0 };
        (-u.x * s).atan2(-u.z * s)
    }
}

/// Recentering pose: yaw-only rotation at the head's horizontal position.
/// Creating a reference space with this pose makes the current head
/// heading the new "forward" and the head position the new origin.
/// `keep_height` keeps y = 0 (STAGE: floor stays at y = 0); otherwise the
/// head height is moved too (LOCAL: eyes at y = 0).
pub fn recenter_pose(head: &Pose, keep_height: bool) -> Pose {
    let yaw = head.yaw();
    let y = if keep_height { 0.0 } else { head.position.y };
    Pose {
        position: Vec3::new(head.position.x, y, head.position.z),
        orientation: Quat::from_rotation_y(yaw),
    }
}

/// A ray with a normalized direction.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ray {
    pub origin: Vec3,
    pub dir: Vec3,
}

impl Ray {
    pub fn at(&self, t: f32) -> Vec3 {
        self.origin + self.dir * t
    }

    /// Intersection with a plane given by a point and normal. Returns the ray
    /// parameter `t ≥ 0`.
    pub fn intersect_plane(&self, point: Vec3, normal: Vec3) -> Option<f32> {
        let denom = self.dir.dot(normal);
        if denom.abs() < 1e-6 {
            return None;
        }
        let t = (point - self.origin).dot(normal) / denom;
        (t >= 0.0).then_some(t)
    }

    /// Hit a quad layer (centre pose, size in metres, facing +Z of the pose)
    /// and return panel UV in `[0,1]²` (v down) plus the distance.
    pub fn intersect_quad(&self, pose: &Pose, size: Vec2) -> Option<(Vec2, f32)> {
        let n = pose.orientation * Vec3::Z;
        let t = self.intersect_plane(pose.position, n)?;
        let local = pose.orientation.inverse() * (self.at(t) - pose.position);
        let uv = Vec2::new(local.x / size.x + 0.5, 0.5 - local.y / size.y);
        (uv.cmpge(Vec2::ZERO).all() && uv.cmple(Vec2::ONE).all()).then_some((uv, t))
    }
}

/// OpenXR FOV as `[left, right, up, down]` radians.
pub fn fov_to_array(f: &xr::Fovf) -> [f32; 4] {
    [f.angle_left, f.angle_right, f.angle_up, f.angle_down]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::{FRAC_PI_2, FRAC_PI_4};

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn xr_roundtrip() {
        let p = Pose::new(
            Vec3::new(1.0, 2.0, 3.0),
            Quat::from_rotation_y(0.3) * Quat::from_rotation_x(0.2),
        );
        let back = Pose::from_xr(&p.to_xr());
        assert!((back.position - p.position).length() < 1e-6);
        assert!(back.orientation.angle_between(p.orientation) < 1e-5);
        // Zero quaternion is sanitized.
        let z = xr::Posef {
            orientation: xr::Quaternionf {
                x: 0.0,
                y: 0.0,
                z: 0.0,
                w: 0.0,
            },
            position: xr::Vector3f::default(),
        };
        assert_eq!(Pose::from_xr(&z).orientation, Quat::IDENTITY);
    }

    #[test]
    fn compose_and_inverse() {
        let a = Pose::new(Vec3::new(0.0, 1.0, 0.0), Quat::from_rotation_y(FRAC_PI_2));
        let b = Pose::new(Vec3::new(0.0, 0.0, -1.0), Quat::IDENTITY);
        let c = a.mul(&b);
        // One metre "forward" of a pose turned 90° left is at -X.
        assert!((c.position - Vec3::new(-1.0, 1.0, 0.0)).length() < 1e-5);
        let id = a.mul(&a.inverse());
        assert!(id.position.length() < 1e-5 && id.orientation.angle_between(Quat::IDENTITY) < 1e-5);
    }

    #[test]
    fn yaw_extraction() {
        assert!(close(Pose::IDENTITY.yaw(), 0.0));
        assert!(close(yaw_of(Quat::from_rotation_y(0.7)), 0.7));
        assert!(close(yaw_of(Quat::from_rotation_y(-2.5)), -2.5));
        // Pitch and roll don't disturb yaw.
        let q =
            Quat::from_rotation_y(1.1) * Quat::from_rotation_x(0.6) * Quat::from_rotation_z(0.4);
        assert!(close(yaw_of(q), 1.1));
        // Looking straight down while turned 45° left.
        let down = Quat::from_rotation_y(FRAC_PI_4) * Quat::from_rotation_x(-FRAC_PI_2);
        assert!(close(yaw_of(down), FRAC_PI_4), "{}", yaw_of(down));
        let up = Quat::from_rotation_y(-FRAC_PI_4) * Quat::from_rotation_x(FRAC_PI_2);
        assert!(close(yaw_of(up), -FRAC_PI_4), "{}", yaw_of(up));
    }

    #[test]
    fn recentering() {
        let head = Pose::new(
            Vec3::new(0.5, 1.6, -0.2),
            Quat::from_rotation_y(0.9) * Quat::from_rotation_x(0.3),
        );
        let r = recenter_pose(&head, true);
        assert_eq!(r.position, Vec3::new(0.5, 0.0, -0.2));
        assert!(close(r.yaw(), 0.9));
        // In the recentered space the head faces -Z (yaw 0).
        let local = r.inverse().mul(&head);
        assert!(close(local.yaw(), 0.0));
        assert!(
            close(local.position.x, 0.0)
                && close(local.position.z, 0.0)
                && close(local.position.y, 1.6)
        );
        let l = recenter_pose(&head, false);
        assert!(close(l.inverse().mul(&head).position.y, 0.0));
    }

    #[test]
    fn rays() {
        let p = Pose::new(Vec3::new(0.0, 1.0, 0.0), Quat::from_rotation_y(FRAC_PI_2));
        let r = p.ray();
        assert!((r.dir - Vec3::NEG_X).length() < 1e-5);
        assert!((r.at(2.0) - Vec3::new(-2.0, 1.0, 0.0)).length() < 1e-5);
        let quad = Pose::new(Vec3::new(0.0, 1.0, -2.0), Quat::IDENTITY);
        let fwd = Ray {
            origin: Vec3::new(0.25, 1.25, 0.0),
            dir: Vec3::NEG_Z,
        };
        let (uv, t) = fwd.intersect_quad(&quad, Vec2::new(1.0, 1.0)).unwrap();
        assert!(close(t, 2.0) && close(uv.x, 0.75) && close(uv.y, 0.25));
        let miss = Ray {
            origin: Vec3::new(2.0, 1.0, 0.0),
            dir: Vec3::NEG_Z,
        };
        assert!(miss.intersect_quad(&quad, Vec2::ONE).is_none());
        let away = Ray {
            origin: Vec3::ZERO,
            dir: Vec3::Z,
        };
        assert!(away
            .intersect_plane(Vec3::new(0.0, 0.0, -1.0), Vec3::Z)
            .is_none());
    }
}
