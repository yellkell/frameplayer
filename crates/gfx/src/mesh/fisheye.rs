//! Equidistant fisheye: image radius proportional to the angle from the
//! optical axis, `r = θ / (fov/2) · radius`.

use super::{Mesh, MeshDensity, MeshVertex, SPHERE_RADIUS_M};

/// Fisheye dome covering `fov_deg` around the forward axis.
///
/// * `center` — image-circle centre offset in normalized eye-image units
///   (0,0 = centre, ±0.5 = edge), as in [`fp_core::Projection::Fisheye`].
/// * `radius` — image-circle radius as a fraction of the eye image's
///   half-height.
/// * `eye_aspect` — eye image width / height, so the circle stays round in
///   pixels when the eye image is not square.
pub fn fisheye_mesh(
    fov_deg: f32,
    center: [f32; 2],
    radius: f32,
    eye_aspect: f32,
    density: MeshDensity,
) -> Mesh {
    let half_fov = (fov_deg.clamp(1.0, 359.0) / 2.0).to_radians();
    let rings = density.segments_for(fov_deg / 2.0);
    let sectors = (density.segments_per_90deg.max(4) * 4).max(16);
    let aspect = if eye_aspect > 0.0 { eye_aspect } else { 1.0 };
    let rv = 0.5 * radius;
    let ru = rv / aspect;

    let mut mesh = Mesh::default();
    mesh.vertices.reserve((1 + rings * (sectors + 1)) as usize);
    // Centre vertex.
    mesh.vertices.push(MeshVertex {
        pos: [0.0, 0.0, -SPHERE_RADIUS_M],
        uv: [0.5 + center[0], 0.5 + center[1]],
    });
    for ring in 1..=rings {
        let t = density.warp(ring as f32 / rings as f32);
        let theta = t * half_fov;
        let (st, ct) = theta.sin_cos();
        for s in 0..=sectors {
            // α measured in image space: x right, y down.
            let alpha = s as f32 / sectors as f32 * std::f32::consts::TAU;
            let (sa, ca) = alpha.sin_cos();
            let dir = [st * ca, -st * sa, -ct];
            let uv = [0.5 + center[0] + t * ru * ca, 0.5 + center[1] + t * rv * sa];
            mesh.vertices.push(MeshVertex {
                pos: dir.map(|x| x * SPHERE_RADIUS_M),
                uv,
            });
        }
    }
    // Fan around the centre.
    for s in 0..sectors {
        mesh.indices.extend_from_slice(&[0, 1 + s, 2 + s]);
    }
    // Quads between consecutive rings.
    for ring in 1..rings {
        let a = 1 + (ring - 1) * (sectors + 1);
        mesh.push_grid_indices(a, sectors, 1);
    }
    mesh
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::test_util::{assert_well_formed, nearest};
    use glam::Vec3;

    #[test]
    fn equidistant_mapping() {
        let d = MeshDensity {
            segments_per_90deg: 16,
            foveation: 0.0,
        };
        let m = fisheye_mesh(180.0, [0.0, 0.0], 1.0, 1.0, d);
        assert_well_formed(&m);
        assert_eq!(nearest(&m, Vec3::NEG_Z).uv, [0.5, 0.5]);
        // 90° to the right sits on the circle edge: u = 0.5 + 0.5.
        let r = nearest(&m, Vec3::X);
        assert!(
            (r.uv[0] - 1.0).abs() < 1e-4 && (r.uv[1] - 0.5).abs() < 1e-4,
            "{:?}",
            r.uv
        );
        // Straight up → top edge (v = 0).
        let u = nearest(&m, Vec3::Y);
        assert!(
            (u.uv[1]).abs() < 1e-4 && (u.uv[0] - 0.5).abs() < 1e-4,
            "{:?}",
            u.uv
        );
        // Equidistance: every vertex' image radius proportional to its angle.
        for v in &m.vertices {
            let theta = Vec3::from(v.pos)
                .normalize()
                .dot(Vec3::NEG_Z)
                .clamp(-1.0, 1.0)
                .acos();
            let r = ((v.uv[0] - 0.5).powi(2) + (v.uv[1] - 0.5).powi(2)).sqrt();
            assert!((r - theta / (std::f32::consts::PI / 2.0) * 0.5).abs() < 1e-4);
        }
    }

    #[test]
    fn centre_offset_radius_and_aspect() {
        let d = MeshDensity {
            segments_per_90deg: 8,
            foveation: 0.5,
        };
        // 200° lens: 90° off-axis is at 90/100 of the radius.
        let m = fisheye_mesh(200.0, [0.05, -0.02], 0.9, 2.0, d);
        assert_well_formed(&m);
        assert_eq!(nearest(&m, Vec3::NEG_Z).uv, [0.55, 0.48]);
        let all_dirs_ok = m.vertices.iter().all(|v| {
            let theta = Vec3::from(v.pos)
                .normalize()
                .dot(Vec3::NEG_Z)
                .clamp(-1.0, 1.0)
                .acos();
            theta <= 100f32.to_radians() + 1e-4
        });
        assert!(all_dirs_ok);
        // Find a vertex at exactly 90° right if the ring hits it; otherwise
        // check the scaling through the outermost ring.
        let outer_right = m
            .vertices
            .iter()
            .filter(|v| v.pos[1].abs() < 1e-3 && v.pos[0] > 0.0)
            .max_by(|a, b| a.uv[0].total_cmp(&b.uv[0]))
            .unwrap();
        // u extent of the full circle = 0.5 * 0.9 / 2.0 (aspect 2).
        assert!(
            (outer_right.uv[0] - (0.55 + 0.225)).abs() < 1e-4,
            "{:?}",
            outer_right.uv
        );
    }
}
