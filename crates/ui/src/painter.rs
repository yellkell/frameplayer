//! Tessellation into [`DrawList`]s: filled/rounded rects, strokes, circles,
//! arcs, lines, convex polygons, images and laid-out text.
//!
//! Draw calls go to one of several [`Layer`]s which are concatenated in order
//! at the end of the frame, so popups and modals drawn mid-frame still end up
//! on top.

use crate::geom::{Rect, Vec2};
use crate::text::{Fonts, TextLayout};
use crate::theme::Color;
use fp_core::draw::{DrawCmd, DrawList, TextureId, Vertex};

/// Z-order bucket; later layers draw on top and block input to earlier ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum Layer {
    #[default]
    Base,
    Popup,
    Modal,
    Tooltip,
}

impl Layer {
    pub const ALL: [Layer; 4] = [Layer::Base, Layer::Popup, Layer::Modal, Layer::Tooltip];
    fn idx(self) -> usize {
        self as usize
    }
}

#[derive(Debug)]
pub struct Painter {
    lists: [DrawList; 4],
    layer: Layer,
    clip_stack: Vec<Rect>,
    screen: Rect,
}

impl Painter {
    pub fn new(size: Vec2) -> Painter {
        let screen = Rect::new(0.0, 0.0, size.x, size.y);
        Painter {
            lists: Default::default(),
            layer: Layer::Base,
            clip_stack: vec![screen],
            screen,
        }
    }

    pub fn reset(&mut self, size: Vec2) {
        for l in &mut self.lists {
            l.clear();
        }
        self.screen = Rect::new(0.0, 0.0, size.x, size.y);
        self.clip_stack.clear();
        self.clip_stack.push(self.screen);
        self.layer = Layer::Base;
    }

    pub fn layer(&self) -> Layer {
        self.layer
    }

    /// Switches layer, returning the previous one.
    pub fn set_layer(&mut self, layer: Layer) -> Layer {
        std::mem::replace(&mut self.layer, layer)
    }

    pub fn clip(&self) -> Rect {
        *self.clip_stack.last().unwrap_or(&self.screen)
    }

    /// Pushes `rect ∩ current clip`.
    pub fn push_clip(&mut self, rect: Rect) {
        let c = self.clip().intersect(&rect);
        self.clip_stack.push(c);
    }

    /// Pushes a clip that ignores the current one (used by overlay layers).
    pub fn push_clip_absolute(&mut self, rect: Rect) {
        self.clip_stack.push(self.screen.intersect(&rect));
    }

    pub fn pop_clip(&mut self) {
        if self.clip_stack.len() > 1 {
            self.clip_stack.pop();
        }
    }

    fn visible(&self, bounds: Rect) -> bool {
        let c = self.clip();
        !c.is_empty() && c.intersects(&bounds)
    }

    /// Appends indexed triangles (indices relative to `verts`).
    pub fn triangles(&mut self, texture: TextureId, verts: &[Vertex], idx: &[u32]) {
        if verts.is_empty() || idx.is_empty() {
            return;
        }
        let clip = self.clip().to_array();
        let list = &mut self.lists[self.layer.idx()];
        let base = list.vertices.len() as u32;
        list.vertices.extend_from_slice(verts);
        let first = list.indices.len() as u32;
        list.indices.extend(idx.iter().map(|i| base + i));
        let count = idx.len() as u32;
        match list.cmds.last_mut() {
            Some(c)
                if c.texture == texture
                    && c.clip == clip
                    && c.first_index + c.index_count == first =>
            {
                c.index_count += count
            }
            _ => list.cmds.push(DrawCmd {
                texture,
                clip,
                first_index: first,
                index_count: count,
            }),
        }
    }

    pub fn rect_filled(&mut self, r: Rect, color: Color) {
        if r.is_empty() || color.a() <= 0.0 || !self.visible(r) {
            return;
        }
        let clip = self.clip().to_array();
        self.lists[self.layer.idx()].quad(TextureId::White, clip, r.to_array(), [0.0; 4], color.0);
    }

    pub fn image(&mut self, r: Rect, texture: TextureId, uv: [f32; 4], tint: Color) {
        if r.is_empty() || !self.visible(r) {
            return;
        }
        let clip = self.clip().to_array();
        self.lists[self.layer.idx()].quad(texture, clip, r.to_array(), uv, tint.0);
    }

    /// Perimeter of a rounded rect (clockwise in screen space).
    fn rounded_path(r: Rect, radius: f32) -> Vec<Vec2> {
        let rad = radius.min(r.w * 0.5).min(r.h * 0.5).max(0.0);
        if rad < 0.5 {
            return vec![
                r.min(),
                Vec2::new(r.right(), r.y),
                r.max(),
                Vec2::new(r.x, r.bottom()),
            ];
        }
        let seg = ((rad / 3.0).ceil() as usize).clamp(2, 8);
        let corners = [
            (Vec2::new(r.right() - rad, r.y + rad), -90.0f32),
            (Vec2::new(r.right() - rad, r.bottom() - rad), 0.0),
            (Vec2::new(r.x + rad, r.bottom() - rad), 90.0),
            (Vec2::new(r.x + rad, r.y + rad), 180.0),
        ];
        let mut pts = Vec::with_capacity(4 * (seg + 1));
        for (c, a0) in corners {
            for s in 0..=seg {
                let a = (a0 + 90.0 * s as f32 / seg as f32).to_radians();
                pts.push(c + Vec2::new(a.cos(), a.sin()) * rad);
            }
        }
        pts
    }

    pub fn rect_rounded(&mut self, r: Rect, radius: f32, color: Color) {
        if r.is_empty() || color.a() <= 0.0 || !self.visible(r) {
            return;
        }
        if radius < 0.5 {
            return self.rect_filled(r, color);
        }
        let pts = Painter::rounded_path(r, radius);
        self.convex_polygon(&pts, color);
    }

    /// Stroke inside the rect's edge.
    pub fn rect_stroke(&mut self, r: Rect, radius: f32, width: f32, color: Color) {
        if r.is_empty() || width <= 0.0 || color.a() <= 0.0 || !self.visible(r.expand(width)) {
            return;
        }
        let outer = Painter::rounded_path(r, radius);
        let inner = Painter::rounded_path(r.shrink(width), (radius - width).max(0.0));
        if outer.len() == inner.len() {
            self.ring_strip(&outer, &inner, color);
        } else {
            // Inner path degenerated to a sharp rect; fall back to four bars.
            let (t, rest) = r.split_top(width);
            let (rest, b) = rest.split_bottom(width);
            let (l, rest) = rest.split_left(width);
            let (_, rr) = rest.split_right(width);
            for bar in [t, b, l, rr] {
                self.rect_filled(bar, color);
            }
        }
    }

    fn ring_strip(&mut self, outer: &[Vec2], inner: &[Vec2], color: Color) {
        let n = outer.len() as u32;
        let mut verts = Vec::with_capacity(outer.len() * 2);
        for (o, i) in outer.iter().zip(inner) {
            verts.push(vtx(*o, color));
            verts.push(vtx(*i, color));
        }
        let mut idx = Vec::with_capacity(outer.len() * 6);
        for k in 0..n {
            let a = 2 * k;
            let b = 2 * ((k + 1) % n);
            idx.extend_from_slice(&[a, b, a + 1, a + 1, b, b + 1]);
        }
        self.triangles(TextureId::White, &verts, &idx);
    }

    /// Filled convex polygon (triangle fan from the first point).
    pub fn convex_polygon(&mut self, pts: &[Vec2], color: Color) {
        if pts.len() < 3 || color.a() <= 0.0 {
            return;
        }
        let verts: Vec<Vertex> = pts.iter().map(|p| vtx(*p, color)).collect();
        let mut idx = Vec::with_capacity((pts.len() - 2) * 3);
        for k in 1..pts.len() as u32 - 1 {
            idx.extend_from_slice(&[0, k, k + 1]);
        }
        self.triangles(TextureId::White, &verts, &idx);
    }

    pub fn triangle(&mut self, a: Vec2, b: Vec2, c: Vec2, color: Color) {
        self.convex_polygon(&[a, b, c], color);
    }

    pub fn line(&mut self, a: Vec2, b: Vec2, width: f32, color: Color) {
        let d = b - a;
        if d.length_squared() < 1e-6 || color.a() <= 0.0 {
            return;
        }
        let n = Vec2::new(-d.y, d.x).normalize() * (width * 0.5);
        self.convex_polygon(&[a + n, b + n, b - n, a - n], color);
    }

    pub fn polyline(&mut self, pts: &[Vec2], width: f32, color: Color) {
        for w in pts.windows(2) {
            self.line(w[0], w[1], width, color);
        }
    }

    pub fn circle(&mut self, c: Vec2, r: f32, color: Color) {
        let seg = ((r * 0.8) as usize).clamp(8, 48);
        let pts: Vec<Vec2> = (0..seg)
            .map(|i| {
                let a = std::f32::consts::TAU * i as f32 / seg as f32;
                c + Vec2::new(a.cos(), a.sin()) * r
            })
            .collect();
        self.convex_polygon(&pts, color);
    }

    /// Ring segment between angles `a0..a1` (radians, 0 = +x, clockwise on screen).
    pub fn arc(&mut self, c: Vec2, r: f32, width: f32, a0: f32, a1: f32, color: Color) {
        let span = a1 - a0;
        let seg = ((span.abs() * r / 6.0).ceil() as usize).clamp(4, 64);
        let mut verts = Vec::with_capacity((seg + 1) * 2);
        let (ro, ri) = (r + width * 0.5, (r - width * 0.5).max(0.0));
        for s in 0..=seg {
            let a = a0 + span * s as f32 / seg as f32;
            let d = Vec2::new(a.cos(), a.sin());
            verts.push(vtx(c + d * ro, color));
            verts.push(vtx(c + d * ri, color));
        }
        let mut idx = Vec::with_capacity(seg * 6);
        for s in 0..seg as u32 {
            let a = 2 * s;
            idx.extend_from_slice(&[a, a + 2, a + 1, a + 1, a + 2, a + 3]);
        }
        self.triangles(TextureId::White, &verts, &idx);
    }

    /// Draws a laid-out text block with its top-left at `pos`.
    pub fn text(&mut self, fonts: &mut Fonts, pos: Vec2, layout: &TextLayout, color: Color) {
        let bounds = Rect::new(pos.x, pos.y, layout.size.x.max(1.0), layout.size.y.max(1.0));
        if layout.glyphs.is_empty()
            || color.a() <= 0.0
            || !self.visible(bounds.expand(layout.line_height))
        {
            return;
        }
        let clip = self.clip();
        let clip_a = clip.to_array();
        for g in &layout.glyphs {
            let Some(e) = fonts.glyph(g.face, g.glyph, g.px) else {
                continue;
            };
            let p = (pos + g.pos + e.offset).round();
            let r = Rect::new(p.x, p.y, e.size.x, e.size.y);
            if !clip.intersects(&r) {
                continue;
            }
            self.lists[self.layer.idx()].quad(
                TextureId::FontAtlas,
                clip_a,
                r.to_array(),
                e.uv,
                color.0,
            );
        }
    }

    /// Concatenates all layers into `out`, scaling every colour by `opacity`.
    pub fn finish_into(&mut self, out: &mut DrawList, opacity: f32) {
        out.clear();
        for l in Layer::ALL {
            let src = &self.lists[l.idx()];
            let vbase = out.vertices.len() as u32;
            let ibase = out.indices.len() as u32;
            out.vertices.extend_from_slice(&src.vertices);
            out.indices.extend(src.indices.iter().map(|i| i + vbase));
            for c in &src.cmds {
                let c = DrawCmd {
                    first_index: c.first_index + ibase,
                    ..c.clone()
                };
                match out.cmds.last_mut() {
                    Some(p)
                        if p.texture == c.texture
                            && p.clip == c.clip
                            && p.first_index + p.index_count == c.first_index =>
                    {
                        p.index_count += c.index_count
                    }
                    _ => out.cmds.push(c),
                }
            }
        }
        if opacity < 1.0 {
            let o = opacity.clamp(0.0, 1.0);
            for v in &mut out.vertices {
                v.color = v.color.map(|c| c * o);
            }
        }
    }
}

fn vtx(p: Vec2, color: Color) -> Vertex {
    Vertex {
        pos: [p.x, p.y],
        uv: [0.0, 0.0],
        color: color.0,
    }
}

/// Checks the invariants a renderer relies on; used by tests and debug builds.
pub fn validate(list: &DrawList) -> Result<(), String> {
    let nv = list.vertices.len() as u32;
    if let Some(i) = list.indices.iter().find(|&&i| i >= nv) {
        return Err(format!("index {i} out of range ({nv} vertices)"));
    }
    if list.indices.len() % 3 != 0 {
        return Err("index count not a multiple of 3".into());
    }
    let mut expected = 0;
    for c in &list.cmds {
        if c.first_index != expected {
            return Err(format!("command gap at {}", c.first_index));
        }
        expected += c.index_count;
        if c.clip[2] < 0.0 || c.clip[3] < 0.0 {
            return Err("negative clip".into());
        }
    }
    if expected as usize != list.indices.len() {
        return Err("commands don't cover all indices".into());
    }
    if list.vertices.iter().any(|v| {
        v.pos
            .iter()
            .chain(&v.uv)
            .chain(&v.color)
            .any(|f| !f.is_finite())
    }) {
        return Err("non-finite vertex data".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layers_concatenate_in_order() {
        let mut p = Painter::new(Vec2::new(100.0, 100.0));
        p.set_layer(Layer::Modal);
        p.rect_filled(Rect::new(0.0, 0.0, 10.0, 10.0), Color::WHITE);
        p.set_layer(Layer::Base);
        p.rect_rounded(Rect::new(0.0, 0.0, 50.0, 30.0), 8.0, Color::BLACK);
        let mut out = DrawList::default();
        p.finish_into(&mut out, 0.5);
        validate(&out).unwrap();
        // Base (rounded, many verts) comes first; modal quad is last.
        let last = out.vertices.last().unwrap();
        assert_eq!(last.color, [0.5; 4]);
        assert_eq!(out.vertices[0].color, [0.0, 0.0, 0.0, 0.5]);
    }

    #[test]
    fn clipping_culls_and_records() {
        let mut p = Painter::new(Vec2::new(100.0, 100.0));
        p.push_clip(Rect::new(10.0, 10.0, 20.0, 20.0));
        p.rect_filled(Rect::new(50.0, 50.0, 5.0, 5.0), Color::WHITE);
        p.rect_filled(Rect::new(15.0, 15.0, 5.0, 5.0), Color::WHITE);
        p.pop_clip();
        let mut out = DrawList::default();
        p.finish_into(&mut out, 1.0);
        assert_eq!(out.indices.len(), 6);
        assert_eq!(out.cmds[0].clip, [10.0, 10.0, 20.0, 20.0]);
    }

    #[test]
    fn strokes_and_arcs_are_valid() {
        let mut p = Painter::new(Vec2::new(200.0, 200.0));
        p.rect_stroke(Rect::new(10.0, 10.0, 100.0, 50.0), 10.0, 3.0, Color::WHITE);
        p.rect_stroke(Rect::new(10.0, 10.0, 100.0, 50.0), 0.0, 3.0, Color::WHITE);
        p.arc(Vec2::new(100.0, 100.0), 30.0, 4.0, 0.0, 4.0, Color::WHITE);
        p.circle(Vec2::new(50.0, 50.0), 10.0, Color::WHITE);
        p.line(Vec2::ZERO, Vec2::new(10.0, 10.0), 2.0, Color::WHITE);
        let mut out = DrawList::default();
        p.finish_into(&mut out, 1.0);
        validate(&out).unwrap();
        assert_eq!(out.cmds.len(), 1);
    }
}
