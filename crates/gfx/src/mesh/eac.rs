//! YouTube equi-angular cubemap (EAC).
//!
//! Each cube face is sampled with an equi-angular mapping: the face-local
//! texture coordinate is linear in the *angle* `α = atan(a)` rather than in
//! the cube coordinate `a`, which spreads pixels evenly over the sphere.
//! Faces are packed 3×2: top row Left | Front | Right, bottom row
//! Down | Back | Up with the bottom row rotated so edges stay continuous
//! (same table as ffmpeg's `v360` EAC input).

use super::{Mesh, MeshDensity, MeshVertex, SPHERE_RADIUS_M};
use glam::Vec3;
use std::f32::consts::FRAC_PI_4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EacFace {
    Left,
    Front,
    Right,
    Down,
    Back,
    Up,
}

/// How a face image is rotated inside its layout cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaceRotation {
    R0,
    /// 90° clockwise.
    Cw90,
    /// 90° counter-clockwise (= 270° clockwise).
    Ccw90,
}

/// `(face, column, row, rotation)` for the YouTube 3×2 layout.
// [verify] Bottom-row rotations follow ffmpeg's v360 EAC table; confirm the
// orientation with a YouTube EAC test clip (look straight down/back/up).
pub const EAC_LAYOUT: [(EacFace, u32, u32, FaceRotation); 6] = [
    (EacFace::Left, 0, 0, FaceRotation::R0),
    (EacFace::Front, 1, 0, FaceRotation::R0),
    (EacFace::Right, 2, 0, FaceRotation::R0),
    (EacFace::Down, 0, 1, FaceRotation::Ccw90),
    (EacFace::Back, 1, 1, FaceRotation::Cw90),
    (EacFace::Up, 2, 1, FaceRotation::Ccw90),
];

impl EacFace {
    /// `(forward, right, up)` axes of the face as seen from the centre with
    /// the canonical (unrotated) image orientation.
    pub fn axes(self) -> (Vec3, Vec3, Vec3) {
        match self {
            EacFace::Front => (Vec3::NEG_Z, Vec3::X, Vec3::Y),
            EacFace::Right => (Vec3::X, Vec3::Z, Vec3::Y),
            EacFace::Back => (Vec3::Z, Vec3::NEG_X, Vec3::Y),
            EacFace::Left => (Vec3::NEG_X, Vec3::NEG_Z, Vec3::Y),
            // Looking up, image top towards the back.
            EacFace::Up => (Vec3::Y, Vec3::X, Vec3::Z),
            // Looking down, image top towards the front.
            EacFace::Down => (Vec3::NEG_Y, Vec3::X, Vec3::NEG_Z),
        }
    }
}

impl FaceRotation {
    /// Map canonical face coords `(s, t)` (s right, t down, `[0,1]`) to the
    /// position of that texel in the stored, rotated face image.
    pub fn apply(self, s: f32, t: f32) -> (f32, f32) {
        match self {
            FaceRotation::R0 => (s, t),
            FaceRotation::Cw90 => (1.0 - t, s),
            FaceRotation::Ccw90 => (t, 1.0 - s),
        }
    }
}

/// Map an equi-angular face coordinate `e ∈ [0,1]` to the cube coordinate
/// `a ∈ [-1,1]`.
pub fn eac_to_cube(e: f32) -> f32 {
    ((e * 2.0 - 1.0) * FRAC_PI_4).tan()
}

/// Inverse of [`eac_to_cube`].
pub fn cube_to_eac(a: f32) -> f32 {
    (a.atan() / FRAC_PI_4 + 1.0) * 0.5
}

/// Build the six-face EAC sphere.
pub fn eac_mesh(density: MeshDensity) -> Mesh {
    let n = density.segments_for(90.0);
    let per_face = (n + 1) * (n + 1);
    let mut mesh = Mesh {
        vertices: Vec::with_capacity((per_face * 6) as usize),
        indices: Vec::with_capacity((n * n * 6 * 6) as usize),
    };
    for (face, col, row, rot) in EAC_LAYOUT {
        let (f, r, u) = face.axes();
        let base = mesh.vertices.len() as u32;
        for j in 0..=n {
            let t = j as f32 / n as f32;
            let b = -eac_to_cube(t); // t down, b up
            for i in 0..=n {
                let s = i as f32 / n as f32;
                let a = eac_to_cube(s);
                let dir = (f + r * a + u * b).normalize() * SPHERE_RADIUS_M;
                let (rs, rt) = rot.apply(s, t);
                let uv = [(col as f32 + rs) / 3.0, (row as f32 + rt) / 2.0];
                mesh.vertices.push(MeshVertex {
                    pos: dir.into(),
                    uv,
                });
            }
        }
        mesh.push_grid_indices(base, n, n);
    }
    mesh
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::test_util::{assert_well_formed, nearest};

    #[test]
    fn equi_angular_roundtrip() {
        for i in 0..=20 {
            let e = i as f32 / 20.0;
            assert!((cube_to_eac(eac_to_cube(e)) - e).abs() < 1e-5);
        }
        assert!((eac_to_cube(0.0) + 1.0).abs() < 1e-6);
        assert!(eac_to_cube(0.5).abs() < 1e-6);
        assert!((eac_to_cube(1.0) - 1.0).abs() < 1e-6);
        // Equal steps in e are equal steps in angle.
        let a1 = eac_to_cube(0.75).atan() - eac_to_cube(0.5).atan();
        let a2 = eac_to_cube(1.0).atan() - eac_to_cube(0.75).atan();
        assert!((a1 - a2).abs() < 1e-6);
    }

    #[test]
    fn face_centres_land_in_layout_cells() {
        let m = eac_mesh(MeshDensity {
            segments_per_90deg: 4,
            foveation: 0.0,
        });
        assert_well_formed(&m);
        let cell_centre = |c: f32, r: f32| [(c + 0.5) / 3.0, (r + 0.5) / 2.0];
        let check = |dir: Vec3, want: [f32; 2]| {
            let v = nearest(&m, dir);
            assert!(
                (v.uv[0] - want[0]).abs() < 1e-5 && (v.uv[1] - want[1]).abs() < 1e-5,
                "{dir:?}: {:?}",
                v.uv
            );
        };
        check(Vec3::NEG_X, cell_centre(0.0, 0.0));
        check(Vec3::NEG_Z, cell_centre(1.0, 0.0));
        check(Vec3::X, cell_centre(2.0, 0.0));
        check(Vec3::NEG_Y, cell_centre(0.0, 1.0));
        check(Vec3::Z, cell_centre(1.0, 1.0));
        check(Vec3::Y, cell_centre(2.0, 1.0));
    }

    #[test]
    fn front_face_orientation_and_rotations() {
        let m = eac_mesh(MeshDensity {
            segments_per_90deg: 4,
            foveation: 0.0,
        });
        // Shared edges/corners exist once per face, so look for the copy with
        // the expected UV rather than the nearest vertex.
        let has = |dir: Vec3, uv: [f32; 2]| {
            m.vertices.iter().any(|v| {
                Vec3::from(v.pos).normalize().dot(dir.normalize()) > 0.9999
                    && (v.uv[0] - uv[0]).abs() < 1e-5
                    && (v.uv[1] - uv[1]).abs() < 1e-5
            })
        };
        // Front face: up-right corner → top-right of the front cell.
        assert!(has(Vec3::new(1.0, 1.0, -1.0), [2.0 / 3.0, 0.0]));
        // Back is rotated 90° CW: its canonical top edge (towards +Y) is the
        // right edge of its cell, adjoining Up.
        assert!(has(Vec3::new(0.0, 1.0, 1.0), [2.0 / 3.0, 0.75]));
        // Up (rotated CCW) has its canonical top edge (towards +Z / Back) on
        // the left edge of its cell, i.e. the same seam.
        assert!(has(Vec3::new(0.0, 1.0, 1.0), [2.0 / 3.0, 0.75]));
        // Down (rotated CCW) touches Back along its right edge.
        assert!(has(Vec3::new(0.0, -1.0, 1.0), [1.0 / 3.0, 0.75]));
        assert_eq!(FaceRotation::Cw90.apply(0.0, 0.0), (1.0, 0.0));
        assert_eq!(FaceRotation::Ccw90.apply(0.0, 0.0), (0.0, 1.0));
    }
}
