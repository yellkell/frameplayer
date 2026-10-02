//! Equirectangular sphere segments (180°, 360°, or any horizontal FOV).

use super::{Mesh, MeshDensity, MeshVertex, SPHERE_RADIUS_M};
use std::f32::consts::PI;

/// Direction for longitude `lon` (radians, positive = right of forward) and
/// latitude `lat` (radians, positive = up).
pub(crate) fn sphere_dir(lon: f32, lat: f32) -> [f32; 3] {
    let (sl, cl) = lon.sin_cos();
    let (sp, cp) = lat.sin_cos();
    [sl * cp, sp, -cl * cp]
}

/// Sphere segment covering `h_fov_deg` of longitude (centred on forward) and
/// the full 180° of latitude. u runs left→right with longitude, v top→bottom.
pub fn equirect_mesh(h_fov_deg: f32, density: MeshDensity) -> Mesh {
    let h_fov = h_fov_deg.clamp(1.0, 360.0).to_radians();
    let cols = density.segments_for(h_fov_deg.clamp(1.0, 360.0));
    let rows = density.segments_for(180.0);
    let mut mesh = Mesh {
        vertices: Vec::with_capacity(((cols + 1) * (rows + 1)) as usize),
        indices: Vec::with_capacity((cols * rows * 6) as usize),
    };
    for r in 0..=rows {
        let tv = density.warp(r as f32 / rows as f32 * 2.0 - 1.0); // -1 top .. 1 bottom
        let lat = -tv * PI / 2.0;
        let v = 0.5 + tv * 0.5;
        for c in 0..=cols {
            let tu = density.warp(c as f32 / cols as f32 * 2.0 - 1.0);
            let lon = tu * h_fov / 2.0;
            let u = 0.5 + tu * 0.5;
            let d = sphere_dir(lon, lat);
            mesh.vertices.push(MeshVertex {
                pos: d.map(|x| x * SPHERE_RADIUS_M),
                uv: [u, v],
            });
        }
    }
    mesh.push_grid_indices(0, cols, rows);
    mesh
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::test_util::{assert_well_formed, nearest};
    use glam::Vec3;

    fn approx(a: [f32; 2], b: [f32; 2]) -> bool {
        (a[0] - b[0]).abs() < 1e-4 && (a[1] - b[1]).abs() < 1e-4
    }

    #[test]
    fn forward_is_image_centre() {
        for fov in [180.0, 360.0] {
            let m = equirect_mesh(fov, MeshDensity::default());
            assert_well_formed(&m);
            let v = nearest(&m, Vec3::NEG_Z);
            assert!(approx(v.uv, [0.5, 0.5]), "{fov}: {:?}", v.uv);
        }
    }

    #[test]
    fn right_and_up_map_correctly() {
        let d = MeshDensity {
            segments_per_90deg: 16,
            foveation: 0.3,
        };
        let m180 = equirect_mesh(180.0, d);
        assert!(approx(nearest(&m180, Vec3::X).uv, [1.0, 0.5]));
        assert!(approx(nearest(&m180, Vec3::NEG_X).uv, [0.0, 0.5]));
        assert!((nearest(&m180, Vec3::Y).uv[1]).abs() < 1e-4);
        // Uniform spacing so a column lands exactly on +X.
        let m360 = equirect_mesh(
            360.0,
            MeshDensity {
                foveation: 0.0,
                ..d
            },
        );
        assert!(approx(nearest(&m360, Vec3::X).uv, [0.75, 0.5]));
        assert!((nearest(&m360, Vec3::NEG_Y).uv[1] - 1.0).abs() < 1e-4);
    }

    #[test]
    fn all_vertices_on_sphere_and_uv_consistent() {
        let m = equirect_mesh(
            360.0,
            MeshDensity {
                segments_per_90deg: 6,
                foveation: 0.8,
            },
        );
        for v in &m.vertices {
            let p = Vec3::from(v.pos);
            assert!((p.length() - SPHERE_RADIUS_M).abs() < 1e-3);
            // Recover lon/lat from the position and compare with UV.
            let n = p / SPHERE_RADIUS_M;
            let lat = n.y.clamp(-1.0, 1.0).asin();
            assert!((v.uv[1] - (0.5 - lat / PI)).abs() < 1e-4);
            if n.y.abs() < 0.999 {
                let lon = n.x.atan2(-n.z);
                let u = 0.5 + lon / (2.0 * PI);
                // ±180° seam: either side is valid.
                assert!((v.uv[0] - u).abs() < 1e-3 || (v.uv[0] - u).abs() > 0.999);
            }
        }
    }

    #[test]
    fn foveation_concentrates_vertices_forward() {
        let d = MeshDensity {
            segments_per_90deg: 16,
            foveation: 1.0,
        };
        let m = equirect_mesh(360.0, d);
        let forward = m
            .vertices
            .iter()
            .filter(|v| (v.uv[0] - 0.5).abs() < 0.1)
            .count();
        let behind = m.vertices.iter().filter(|v| v.uv[0] < 0.1).count();
        assert!(forward > behind * 2, "forward {forward} behind {behind}");
    }
}
