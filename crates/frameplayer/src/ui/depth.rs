//! Depth for flat shapes: vertical gradients and lit or shaded edges, as
//! meshes that follow a rounded rectangle (a circle is a square with half
//! its width as radius). egui's antialiased fill goes down first; these
//! meshes fade out at their edges so the outline stays smooth.

use std::f32::consts::{FRAC_PI_2, PI};

use egui::epaint::Shadow;
use egui::{Color32, CornerRadius, Mesh, Painter, Pos2, Rect, Shape, Vec2};

/// Points round a rounded rectangle, clockwise on screen from the left
/// end of the top-left corner, each with its outward normal.
fn outline(rect: Rect, radius: f32) -> Vec<(Pos2, Vec2)> {
    let r = radius
        .min(rect.width() / 2.0)
        .min(rect.height() / 2.0)
        .max(0.0);
    let seg = ((r * 0.6).ceil() as usize).clamp(2, 20);
    let corners = [
        (Pos2::new(rect.left() + r, rect.top() + r), PI),
        (Pos2::new(rect.right() - r, rect.top() + r), 1.5 * PI),
        (Pos2::new(rect.right() - r, rect.bottom() - r), 0.0),
        (Pos2::new(rect.left() + r, rect.bottom() - r), FRAC_PI_2),
    ];
    let mut pts = Vec::with_capacity(4 * (seg + 1));
    for (c, a0) in corners {
        for i in 0..=seg {
            let a = a0 + FRAC_PI_2 * i as f32 / seg as f32;
            let n = Vec2::new(a.cos(), a.sin());
            pts.push((c + n * r, n));
        }
    }
    pts
}

fn corner(radius: f32) -> CornerRadius {
    CornerRadius::same(radius.round().clamp(0.0, 255.0) as u8)
}

/// Fills a rounded rectangle with a vertical gradient, softened over its
/// last pixel so the antialiased fill beneath shows the edge.
pub fn fill_vgradient(p: &Painter, rect: Rect, radius: f32, top: Color32, bottom: Color32) {
    let col = |y: f32| {
        let t = ((y - rect.top()) / rect.height().max(1.0)).clamp(0.0, 1.0);
        top.lerp_to_gamma(bottom, t)
    };
    let pts = outline(rect.shrink(1.0), (radius - 1.0).max(0.0));
    let n = pts.len() as u32;
    let mut mesh = Mesh::default();
    mesh.colored_vertex(rect.center(), col(rect.center().y));
    for (q, _) in &pts {
        mesh.colored_vertex(*q, col(q.y));
    }
    for (q, nrm) in &pts {
        mesh.colored_vertex(*q + *nrm * 1.0, Color32::TRANSPARENT);
    }
    for i in 0..n {
        let j = (i + 1) % n;
        mesh.add_triangle(0, 1 + i, 1 + j);
        mesh.add_triangle(1 + i, 1 + n + i, 1 + j);
        mesh.add_triangle(1 + j, 1 + n + i, 1 + n + j);
    }
    p.add(Shape::mesh(mesh));
}

/// Which way an edge faces.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Top,
    Bottom,
}

/// A soft band along the `side` of a rounded rectangle's outline, strongest
/// where the edge faces straight that way and gone at the sides: a lit top
/// edge, or (with a dark colour) the shade inside a sunken shape. `outside`
/// puts it just outside the outline instead of just inside.
pub fn edge(
    p: &Painter,
    rect: Rect,
    radius: f32,
    width: f32,
    color: Color32,
    side: Side,
    outside: bool,
) {
    p.add(Shape::mesh(edge_mesh(
        rect, radius, width, color, side, outside,
    )));
}

fn edge_mesh(
    rect: Rect,
    radius: f32,
    width: f32,
    color: Color32,
    side: Side,
    outside: bool,
) -> Mesh {
    let pts = outline(rect, radius);
    let n = pts.len() as u32;
    let dir = if outside { 1.0 } else { -1.0 };
    let mut mesh = Mesh::default();
    for (q, nrm) in &pts {
        let face = match side {
            Side::Top => -nrm.y,
            Side::Bottom => nrm.y,
        };
        let w = face.max(0.0).powf(1.5);
        mesh.colored_vertex(*q - *nrm * dir * 0.5, Color32::TRANSPARENT);
        mesh.colored_vertex(*q + *nrm * dir * 0.6, color.gamma_multiply(w));
        mesh.colored_vertex(*q + *nrm * dir * width, Color32::TRANSPARENT);
    }
    for i in 0..n {
        let j = (i + 1) % n;
        for k in 0..2 {
            let (a, b) = (3 * i + k, 3 * j + k);
            mesh.add_triangle(a, a + 1, b);
            mesh.add_triangle(b, a + 1, b + 1);
        }
    }
    mesh
}

/// How a raised surface looks.
#[derive(Clone, Copy)]
pub struct Raised {
    pub top: Color32,
    pub bottom: Color32,
    /// The lit top edge.
    pub light: Color32,
    /// 0 sits flat on the surface, 1 floats well above it.
    pub lift: f32,
    /// A coloured glow under it (selected and accent controls).
    pub glow: Option<Color32>,
}

/// A raised rounded rectangle: shadow (and glow), gradient body, lit edge.
pub fn raised(p: &Painter, rect: Rect, radius: f32, look: Raised) {
    let cr = corner(radius);
    if look.lift > 0.0 {
        let l = look.lift.clamp(0.0, 1.5);
        p.add(
            Shadow {
                offset: [0, (2.0 + 2.0 * l) as i8],
                blur: (4.0 + 6.0 * l) as u8,
                spread: 0,
                color: Color32::from_black_alpha((130.0 * l.min(1.0)) as u8),
            }
            .as_shape(rect, cr),
        );
    }
    if let Some(g) = look.glow {
        p.add(
            Shadow {
                offset: [0, 4],
                blur: 18,
                spread: 0,
                color: g,
            }
            .as_shape(rect, cr),
        );
    }
    p.rect_filled(rect, cr, look.top.lerp_to_gamma(look.bottom, 0.6));
    fill_vgradient(p, rect, radius, look.top, look.bottom);
    edge(p, rect, radius, 2.2, look.light, Side::Top, false);
}

/// The shapes of a sunken rounded rectangle (a tray or a track): dark
/// fill, shade along its inside top edge, a faint lit lip below it. Shapes
/// rather than painting, so a tray can go under content laid out first.
pub fn sunken_shapes(rect: Rect, radius: f32, fill: Color32) -> Vec<Shape> {
    vec![
        Shape::rect_filled(rect, corner(radius), fill),
        Shape::mesh(edge_mesh(
            rect,
            radius,
            4.0,
            Color32::from_black_alpha(200),
            Side::Top,
            false,
        )),
        Shape::mesh(edge_mesh(
            rect,
            radius,
            1.8,
            Color32::from_white_alpha(18),
            Side::Bottom,
            true,
        )),
    ]
}

/// A sunken rounded rectangle, painted now.
pub fn sunken(p: &Painter, rect: Rect, radius: f32, fill: Color32) {
    p.extend(sunken_shapes(rect, radius, fill));
}
