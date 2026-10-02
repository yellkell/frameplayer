//! Projection mesh generation.
//!
//! Every projection is rendered the same way: a static triangle mesh whose
//! vertices carry a world-space position (metres, OpenXR convention: -Z
//! forward, +Y up, +X right) and a *base UV* in eye-image space (`[0,1]²`
//! covering one eye's sub-image, v down). The per-eye sub-rectangle
//! ([`fp_core::StereoMode::eye_rect`]) and the HereSphere-style corrections
//! are applied afterwards by the projection shader (see [`crate::correction`]),
//! so a mesh only has to be rebuilt when the projection itself changes.

mod eac;
mod equirect;
mod fisheye;
mod obj;
mod screen;

pub use eac::{cube_to_eac, eac_mesh, eac_to_cube, EacFace, FaceRotation, EAC_LAYOUT};
pub use equirect::equirect_mesh;
pub use fisheye::fisheye_mesh;
pub use obj::{parse_obj, ObjError};
pub use screen::screen_mesh;

use bytemuck::{Pod, Zeroable};
use fp_core::Projection;

/// Radius of the sphere immersive projections are drawn on. Large enough that
/// it sits well behind UI layers, small enough for float precision.
pub const SPHERE_RADIUS_M: f32 = 50.0;

/// One mesh vertex as uploaded to the GPU (20 bytes, tightly packed).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default, Pod, Zeroable)]
pub struct MeshVertex {
    /// World-space position in metres.
    pub pos: [f32; 3],
    /// Base UV in eye-image space, v pointing down.
    pub uv: [f32; 2],
}

/// An indexed triangle list.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Mesh {
    pub vertices: Vec<MeshVertex>,
    pub indices: Vec<u32>,
}

impl Mesh {
    pub fn triangle_count(&self) -> usize {
        self.indices.len() / 3
    }

    /// Append a `cols × rows` grid of quads whose vertices start at `base`
    /// (row-major, `cols + 1` vertices per row).
    pub(crate) fn push_grid_indices(&mut self, base: u32, cols: u32, rows: u32) {
        let stride = cols + 1;
        for r in 0..rows {
            for c in 0..cols {
                let i0 = base + r * stride + c;
                let i1 = i0 + 1;
                let i2 = i0 + stride;
                let i3 = i2 + 1;
                self.indices.extend_from_slice(&[i0, i2, i1, i1, i2, i3]);
            }
        }
    }
}

/// Mesh tessellation controls.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeshDensity {
    /// Grid segments per 90° of arc at the *average* density.
    pub segments_per_90deg: u32,
    /// 0 = uniform, 1 = strongly denser towards the forward direction.
    /// Concentrating vertices where the viewer looks keeps interpolation
    /// error low where it is visible and saves vertices behind the viewer.
    pub foveation: f32,
}

impl Default for MeshDensity {
    fn default() -> Self {
        MeshDensity {
            segments_per_90deg: 32,
            foveation: 0.5,
        }
    }
}

impl MeshDensity {
    /// Number of segments for an arc of `deg` degrees (at least 1).
    pub(crate) fn segments_for(&self, deg: f32) -> u32 {
        ((self.segments_per_90deg.max(1) as f32 * deg.abs() / 90.0).ceil() as u32).max(1)
    }

    /// Monotonic warp of `t ∈ [-1, 1]` onto itself with slope `a < 1` at the
    /// origin, so uniformly spaced `t` samples end up denser near 0.
    pub fn warp(&self, t: f32) -> f32 {
        let a = 1.0 - 0.66 * self.foveation.clamp(0.0, 1.0);
        t * (a + (1.0 - a) * t * t)
    }
}

/// Errors from [`build_mesh`].
#[derive(Debug, thiserror::Error)]
pub enum MeshError {
    #[error("custom mesh: {0}")]
    Obj(#[from] ObjError),
    #[error("custom mesh {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
}

/// Build the mesh for `projection`.
///
/// `eye_aspect` is width / height of one eye's sub-image (it sizes flat
/// screens and keeps fisheye circles round). `load_custom` resolves a
/// [`Projection::CustomMesh`] path to OBJ source text.
pub fn build_mesh(
    projection: &Projection,
    eye_aspect: f32,
    density: MeshDensity,
    load_custom: impl FnOnce(&str) -> std::io::Result<String>,
) -> Result<Mesh, MeshError> {
    Ok(match projection {
        Projection::Flat {
            width_m,
            distance_m,
            curvature,
        } => screen_mesh(*width_m, *distance_m, *curvature, eye_aspect, density),
        Projection::Equirect { h_fov_deg } => equirect_mesh(*h_fov_deg, density),
        Projection::Fisheye {
            fov_deg,
            center_x,
            center_y,
            radius,
            ..
        } => fisheye_mesh(
            *fov_deg,
            [*center_x, *center_y],
            *radius,
            eye_aspect,
            density,
        ),
        Projection::Eac => eac_mesh(density),
        Projection::CustomMesh { path } => {
            let src = load_custom(path).map_err(|source| MeshError::Io {
                path: path.clone(),
                source,
            })?;
            parse_obj(&src)?
        }
    })
}

#[cfg(test)]
pub(crate) mod test_util {
    use super::*;

    /// Every index in range, every UV finite.
    pub fn assert_well_formed(m: &Mesh) {
        assert!(!m.indices.is_empty());
        assert_eq!(m.indices.len() % 3, 0);
        let n = m.vertices.len() as u32;
        assert!(m.indices.iter().all(|&i| i < n), "index out of range");
        assert!(m
            .vertices
            .iter()
            .all(|v| v.pos.iter().chain(v.uv.iter()).all(|x| x.is_finite())));
    }

    /// Vertex whose direction is closest to `dir`.
    pub fn nearest(m: &Mesh, dir: glam::Vec3) -> MeshVertex {
        *m.vertices
            .iter()
            .max_by(|a, b| {
                let da = glam::Vec3::from(a.pos).normalize().dot(dir);
                let db = glam::Vec3::from(b.pos).normalize().dot(dir);
                da.total_cmp(&db)
            })
            .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warp_is_monotonic_and_denser_at_centre() {
        let d = MeshDensity {
            segments_per_90deg: 8,
            foveation: 1.0,
        };
        assert_eq!(d.warp(0.0), 0.0);
        assert!((d.warp(1.0) - 1.0).abs() < 1e-6);
        assert!((d.warp(-1.0) + 1.0).abs() < 1e-6);
        let mut prev = -1.0;
        for i in 1..=100 {
            let w = d.warp(-1.0 + i as f32 * 0.02);
            assert!(w > prev);
            prev = w;
        }
        // Step near the centre smaller than step near the edge.
        let centre = d.warp(0.1) - d.warp(0.0);
        let edge = d.warp(1.0) - d.warp(0.9);
        assert!(centre < edge);
        let uniform = MeshDensity {
            foveation: 0.0,
            ..d
        };
        assert!((uniform.warp(0.3) - 0.3).abs() < 1e-6);
    }

    #[test]
    fn build_dispatches_every_projection() {
        let d = MeshDensity {
            segments_per_90deg: 4,
            foveation: 0.0,
        };
        for p in [
            Projection::FLAT_DEFAULT,
            Projection::EQUIRECT_180,
            Projection::EQUIRECT_360,
            Projection::fisheye(fp_core::FisheyeLens::Mkx200),
            Projection::Eac,
        ] {
            let m = build_mesh(&p, 16.0 / 9.0, d, |_| unreachable!()).unwrap();
            test_util::assert_well_formed(&m);
        }
        let quad = "v -1 -1 -2\nv 1 -1 -2\nv 1 1 -2\nv -1 1 -2\nvt 0 0\nvt 1 0\nvt 1 1\nvt 0 1\nf 1/1 2/2 3/3 4/4\n";
        let m = build_mesh(
            &Projection::CustomMesh {
                path: "q.obj".into(),
            },
            1.0,
            d,
            |p| {
                assert_eq!(p, "q.obj");
                Ok(quad.to_string())
            },
        )
        .unwrap();
        assert_eq!(m.triangle_count(), 2);
        let err = build_mesh(&Projection::CustomMesh { path: "x".into() }, 1.0, d, |_| {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "nope"))
        });
        assert!(matches!(err, Err(MeshError::Io { .. })));
    }
}
