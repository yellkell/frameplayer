//! Flat / curved virtual cinema screen.

use super::{Mesh, MeshDensity, MeshVertex};

/// A screen `width_m` wide centred at eye height `distance_m` in front of the
/// viewer. `curvature` 0 = flat; 1 = cylinder segment centred on the viewer;
/// values in between use a cylinder of radius `distance / curvature` whose
/// front still touches `distance_m`. The arc length always equals `width_m`.
pub fn screen_mesh(
    width_m: f32,
    distance_m: f32,
    curvature: f32,
    eye_aspect: f32,
    density: MeshDensity,
) -> Mesh {
    let width = width_m.max(0.01);
    let distance = distance_m.max(0.01);
    let aspect = if eye_aspect > 0.0 {
        eye_aspect
    } else {
        16.0 / 9.0
    };
    let height = width / aspect;
    let curvature = curvature.clamp(0.0, 1.0);

    let (cols, radius) = if curvature > 1e-4 {
        let radius = distance / curvature;
        let arc_deg = (width / radius).to_degrees();
        (density.segments_for(arc_deg).max(8), Some(radius))
    } else {
        (1, None)
    };
    let rows = 1;
    let mut mesh = Mesh::default();
    for r in 0..=rows {
        let v = r as f32 / rows as f32;
        let y = (0.5 - v) * height;
        for c in 0..=cols {
            let u = c as f32 / cols as f32;
            let s = (u - 0.5) * width; // arc length from the centre line
            let (x, z) = match radius {
                Some(rad) => {
                    let a = s / rad;
                    let cz = -distance + rad;
                    (rad * a.sin(), cz - rad * a.cos())
                }
                None => (s, -distance),
            };
            mesh.vertices.push(MeshVertex {
                pos: [x, y, z],
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
    use crate::mesh::test_util::assert_well_formed;

    #[test]
    fn flat_screen_corners() {
        let m = screen_mesh(4.0, 3.5, 0.0, 2.0, MeshDensity::default());
        assert_well_formed(&m);
        assert_eq!(m.vertices.len(), 4);
        assert_eq!(
            m.vertices[0],
            MeshVertex {
                pos: [-2.0, 1.0, -3.5],
                uv: [0.0, 0.0]
            }
        );
        assert_eq!(
            m.vertices[3],
            MeshVertex {
                pos: [2.0, -1.0, -3.5],
                uv: [1.0, 1.0]
            }
        );
    }

    #[test]
    fn full_curvature_is_equidistant() {
        let m = screen_mesh(4.0, 3.0, 1.0, 16.0 / 9.0, MeshDensity::default());
        assert_well_formed(&m);
        for v in &m.vertices {
            let r = (v.pos[0].powi(2) + v.pos[2].powi(2)).sqrt();
            assert!((r - 3.0).abs() < 1e-4);
        }
        // Arc length preserved.
        let row: Vec<_> = m.vertices.iter().filter(|v| v.uv[1] == 0.0).collect();
        let arc: f32 = row
            .windows(2)
            .map(|w| {
                ((w[1].pos[0] - w[0].pos[0]).powi(2) + (w[1].pos[2] - w[0].pos[2]).powi(2)).sqrt()
            })
            .sum();
        assert!((arc - 4.0).abs() < 0.01, "{arc}");
    }

    #[test]
    fn partial_curvature_touches_distance_at_centre() {
        let m = screen_mesh(
            4.0,
            3.0,
            0.5,
            1.0,
            MeshDensity {
                segments_per_90deg: 32,
                foveation: 0.0,
            },
        );
        let centre = m
            .vertices
            .iter()
            .find(|v| (v.uv[0] - 0.5).abs() < 1e-6)
            .unwrap();
        assert!((centre.pos[2] + 3.0).abs() < 1e-5 && centre.pos[0].abs() < 1e-5);
        // Edges bend towards the viewer.
        assert!(m.vertices[0].pos[2] > -3.0);
    }
}
