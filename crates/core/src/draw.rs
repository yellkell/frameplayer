//! Renderer-agnostic 2D draw lists.
//!
//! `fp-ui` produces these in panel-local pixel coordinates; `fp-gfx`
//! rasterizes them into the texture of an OpenXR quad/cylinder layer.
//! Keeping the type here lets the two crates evolve independently.

use serde::{Deserialize, Serialize};

/// Which texture a vertex samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TextureId {
    /// Solid colour; UVs ignored.
    White,
    /// The UI font atlas (single-channel coverage).
    FontAtlas,
    /// An application image (thumbnail etc.), keyed by the image cache.
    Image(u64),
    /// The current video frame (for previews / mini-player).
    Video,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Vertex {
    /// Panel-local pixels, origin top-left.
    pub pos: [f32; 2],
    pub uv: [f32; 2],
    /// Linear-space premultiplied RGBA.
    pub color: [f32; 4],
}

/// A run of triangles sharing one texture and clip rectangle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DrawCmd {
    pub texture: TextureId,
    /// Scissor rect `[x, y, w, h]` in panel pixels.
    pub clip: [f32; 4],
    pub first_index: u32,
    pub index_count: u32,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct DrawList {
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
    pub cmds: Vec<DrawCmd>,
}

/// Font atlas pixels the renderer must upload when `version` changes.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AtlasImage {
    pub version: u64,
    pub width: u32,
    pub height: u32,
    /// R8 coverage.
    pub pixels: Vec<u8>,
}

impl DrawList {
    pub fn clear(&mut self) {
        self.vertices.clear();
        self.indices.clear();
        self.cmds.clear();
    }

    /// Append an axis-aligned quad, merging into the previous command when
    /// texture and clip match.
    pub fn quad(
        &mut self,
        texture: TextureId,
        clip: [f32; 4],
        rect: [f32; 4],
        uv: [f32; 4],
        color: [f32; 4],
    ) {
        let [x, y, w, h] = rect;
        let base = self.vertices.len() as u32;
        self.vertices.extend_from_slice(&[
            Vertex {
                pos: [x, y],
                uv: [uv[0], uv[1]],
                color,
            },
            Vertex {
                pos: [x + w, y],
                uv: [uv[2], uv[1]],
                color,
            },
            Vertex {
                pos: [x + w, y + h],
                uv: [uv[2], uv[3]],
                color,
            },
            Vertex {
                pos: [x, y + h],
                uv: [uv[0], uv[3]],
                color,
            },
        ]);
        let first = self.indices.len() as u32;
        self.indices
            .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
        match self.cmds.last_mut() {
            Some(c)
                if c.texture == texture
                    && c.clip == clip
                    && c.first_index + c.index_count == first =>
            {
                c.index_count += 6
            }
            _ => self.cmds.push(DrawCmd {
                texture,
                clip,
                first_index: first,
                index_count: 6,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quads_merge() {
        let mut d = DrawList::default();
        let clip = [0.0, 0.0, 100.0, 100.0];
        d.quad(
            TextureId::White,
            clip,
            [0.0, 0.0, 10.0, 10.0],
            [0.0; 4],
            [1.0; 4],
        );
        d.quad(
            TextureId::White,
            clip,
            [10.0, 0.0, 10.0, 10.0],
            [0.0; 4],
            [1.0; 4],
        );
        d.quad(
            TextureId::FontAtlas,
            clip,
            [0.0, 0.0, 1.0, 1.0],
            [0.0; 4],
            [1.0; 4],
        );
        assert_eq!(d.cmds.len(), 2);
        assert_eq!(d.cmds[0].index_count, 12);
        assert_eq!(d.indices.len(), 18);
    }
}
