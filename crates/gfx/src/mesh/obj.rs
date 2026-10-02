//! Minimal Wavefront OBJ loader for [`fp_core::Projection::CustomMesh`].
//!
//! Supports `v`, `vt`, and `f` statements (`v/vt`, `v/vt/vn`, negative
//! indices, polygons fan-triangulated). Everything else is ignored. OBJ
//! texture coordinates have v pointing up; they are flipped to the renderer's
//! v-down convention.

use super::{Mesh, MeshVertex};
use std::collections::HashMap;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ObjError {
    #[error("line {line}: {msg}")]
    Parse { line: usize, msg: String },
    #[error("mesh has no faces")]
    Empty,
}

fn perr(line: usize, msg: impl Into<String>) -> ObjError {
    ObjError::Parse {
        line,
        msg: msg.into(),
    }
}

fn resolve(idx: &str, len: usize, line: usize) -> Result<usize, ObjError> {
    let i: i64 = idx
        .parse()
        .map_err(|_| perr(line, format!("bad index '{idx}'")))?;
    let r = if i > 0 {
        i - 1
    } else if i < 0 {
        len as i64 + i
    } else {
        -1
    };
    if r < 0 || r as usize >= len {
        return Err(perr(line, format!("index {i} out of range")));
    }
    Ok(r as usize)
}

fn floats<const N: usize>(
    it: &mut std::str::SplitWhitespace<'_>,
    line: usize,
    min: usize,
) -> Result<[f32; N], ObjError> {
    let mut out = [0.0; N];
    for (k, slot) in out.iter_mut().enumerate() {
        match it.next() {
            Some(t) => {
                *slot = t
                    .parse()
                    .map_err(|_| perr(line, format!("bad number '{t}'")))?
            }
            None if k >= min => break,
            None => return Err(perr(line, "too few components")),
        }
    }
    Ok(out)
}

/// Parse OBJ text into a mesh. Faces must reference texture coordinates.
pub fn parse_obj(src: &str) -> Result<Mesh, ObjError> {
    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut uvs: Vec<[f32; 2]> = Vec::new();
    let mut mesh = Mesh::default();
    let mut dedup: HashMap<(usize, usize), u32> = HashMap::new();
    let mut poly: Vec<u32> = Vec::with_capacity(8);

    for (n, raw) in src.lines().enumerate() {
        let line = n + 1;
        let content = raw.split('#').next().unwrap_or("").trim();
        let mut it = content.split_whitespace();
        match it.next() {
            Some("v") => positions.push(floats::<3>(&mut it, line, 3)?),
            Some("vt") => uvs.push(floats::<2>(&mut it, line, 1)?),
            Some("f") => {
                poly.clear();
                for vert in it {
                    let mut parts = vert.split('/');
                    let vi = resolve(parts.next().unwrap_or(""), positions.len(), line)?;
                    let ti = match parts.next() {
                        Some(t) if !t.is_empty() => resolve(t, uvs.len(), line)?,
                        _ => return Err(perr(line, "face vertex without texture coordinate")),
                    };
                    let idx = *dedup.entry((vi, ti)).or_insert_with(|| {
                        let uv = uvs[ti];
                        mesh.vertices.push(MeshVertex {
                            pos: positions[vi],
                            uv: [uv[0], 1.0 - uv[1]],
                        });
                        (mesh.vertices.len() - 1) as u32
                    });
                    poly.push(idx);
                }
                if poly.len() < 3 {
                    return Err(perr(line, "face with fewer than 3 vertices"));
                }
                for k in 1..poly.len() - 1 {
                    mesh.indices
                        .extend_from_slice(&[poly[0], poly[k], poly[k + 1]]);
                }
            }
            _ => {}
        }
    }
    if mesh.indices.is_empty() {
        return Err(ObjError::Empty);
    }
    Ok(mesh)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quad_and_pentagon_triangulated() {
        let src = "# test\nv 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nv 0.5 1.5 0\nvt 0 0\nvt 1 0\nvt 1 1\nvt 0 1\nvt 0.5 1\n\
                   vn 0 0 1\nf 1/1/1 2/2/1 3/3/1 4/4/1\nf 1/1 2/2 3/3 5/5 4/4\n";
        let m = parse_obj(src).unwrap();
        assert_eq!(m.triangle_count(), 2 + 3);
        assert_eq!(m.vertices.len(), 5); // deduplicated
        assert_eq!(m.vertices[0].uv, [0.0, 1.0]); // v flipped
        assert_eq!(m.vertices[2].uv, [1.0, 0.0]);
        assert_eq!(&m.indices[..6], &[0, 1, 2, 0, 2, 3]);
    }

    #[test]
    fn negative_indices() {
        let src = "v 0 0 0\nv 1 0 0\nv 0 1 0\nvt 0 0\nvt 1 0\nvt 0 1\nf -3/-3 -2/-2 -1/-1\n";
        let m = parse_obj(src).unwrap();
        assert_eq!(m.indices, vec![0, 1, 2]);
        assert_eq!(m.vertices[1].pos, [1.0, 0.0, 0.0]);
    }

    #[test]
    fn errors() {
        assert_eq!(parse_obj("v 0 0 0\n"), Err(ObjError::Empty));
        assert!(matches!(
            parse_obj("v 0 0 0\nf 1 1 1\n"),
            Err(ObjError::Parse { line: 2, .. })
        ));
        assert!(matches!(
            parse_obj("v 0 0\n"),
            Err(ObjError::Parse { line: 1, .. })
        ));
        assert!(matches!(
            parse_obj("v 0 0 0\nvt 0 0\nf 1/1 2/1 1/1\n"),
            Err(ObjError::Parse { line: 3, .. })
        ));
        assert!(matches!(
            parse_obj("v 0 0 0\nvt 0 0\nf 1/1 1/1\n"),
            Err(ObjError::Parse { .. })
        ));
    }
}
