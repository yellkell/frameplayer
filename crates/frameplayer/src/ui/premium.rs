//! The look of paid features (passthrough videos, crate::unlock): FramePlayer's
//! blue-to-violet gradient (the two rings of its icon) on a glowing header
//! card, badges, buttons, swatches and section labels.

use super::icons;
use super::theme::{self, Weight};
use egui::epaint::{PathShape, PathStroke};
use egui::text::LayoutJob;
use egui::{Color32, CornerRadius, Pos2, Rect, Response, Sense, Shape, Ui, Vec2};

/// The gradient's ends, from the icon's rings.
pub const BLUE: Color32 = Color32::from_rgb(92, 160, 255);
pub const VIOLET: Color32 = Color32::from_rgb(170, 110, 255);

/// How long the header celebrates a fresh unlock, in seconds.
pub const CELEBRATE_SECS: f32 = 8.0;

fn lerp(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let c = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(
        c(a.r(), b.r()),
        c(a.g(), b.g()),
        c(a.b(), b.b()),
        c(a.a(), b.a()),
    )
}

/// The gradient at `t` (0 blue, 1 violet), at `alpha`.
fn grad(t: f32, alpha: f32) -> Color32 {
    let c = lerp(BLUE, VIOLET, t);
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (alpha * 255.0) as u8)
}

/// Points around a rounded rectangle, clockwise.
fn rounded(r: Rect, radius: f32) -> Vec<Pos2> {
    let rad = radius.min(r.width() / 2.0).min(r.height() / 2.0);
    let corners = [
        (Pos2::new(r.max.x - rad, r.min.y + rad), -90.0_f32),
        (Pos2::new(r.max.x - rad, r.max.y - rad), 0.0),
        (Pos2::new(r.min.x + rad, r.max.y - rad), 90.0),
        (Pos2::new(r.min.x + rad, r.min.y + rad), 180.0),
    ];
    let mut pts = Vec::with_capacity(4 * 9);
    for (c, start) in corners {
        for i in 0..=8 {
            let a = (start + 90.0 * i as f32 / 8.0).to_radians();
            pts.push(c + rad * Vec2::new(a.cos(), a.sin()));
        }
    }
    pts
}

/// A rounded rectangle filled left to right with the gradient.
fn fill(ui: &Ui, r: Rect, radius: f32, alpha: f32) {
    let mut mesh = egui::Mesh::default();
    let at = |p: Pos2| grad((p.x - r.min.x) / r.width().max(1.0), alpha);
    mesh.colored_vertex(r.center(), at(r.center()));
    let pts = rounded(r, radius);
    for &p in &pts {
        mesh.colored_vertex(p, at(p));
    }
    let n = pts.len() as u32;
    for i in 0..n {
        mesh.add_triangle(0, 1 + i, 1 + (i + 1) % n);
    }
    ui.painter().add(Shape::mesh(mesh));
}

/// A gradient outline; `phase` slides the colours round (0..1 loops).
fn outline(ui: &Ui, r: Rect, radius: f32, width: f32, alpha: f32, phase: f32) {
    let stroke = PathStroke::new_uv(width, move |bounds: Rect, p: Pos2| {
        let u = ((p.x - bounds.min.x) / bounds.width().max(1.0) * 0.5 + phase).rem_euclid(1.0);
        grad(1.0 - (2.0 * u - 1.0).abs(), alpha)
    });
    ui.painter().add(Shape::Path(PathShape::closed_line(
        rounded(r, radius),
        stroke,
    )));
}

/// A soft violet glow behind `r`.
fn glow(ui: &Ui, r: Rect, radius: u8, strength: f32) {
    let s = egui::epaint::Shadow {
        offset: [0, 6],
        blur: 40,
        spread: 0,
        color: Color32::from_rgba_unmultiplied(130, 110, 255, (strength * 70.0) as u8),
    };
    ui.painter().add(s.as_shape(r, CornerRadius::same(radius)));
}

/// A small pill with gradient fill: "UNLOCKED", "$4.99".
pub fn badge(ui: &mut Ui, text: &str) -> Response {
    let g = ui.painter().layout_no_wrap(
        text.to_uppercase(),
        theme::font(Weight::Bold, 12.0),
        Color32::WHITE,
    );
    let size = Vec2::new(g.size().x + 20.0, 24.0);
    let (r, resp) = ui.allocate_exact_size(size, Sense::hover());
    fill(ui, r, 12.0, 1.0);
    ui.painter()
        .galley(r.center() - g.size() / 2.0, g, Color32::WHITE);
    resp
}

/// The header of a paid feature: gradient icon tile, title, badge, subtitle
/// and a glowing gradient border. `celebrate` is seconds since the unlock,
/// while it is fresh: the border flows and the glow pulses.
pub fn header(
    ui: &mut Ui,
    title: &str,
    badge_text: &str,
    subtitle: &str,
    compact: bool,
    celebrate: Option<f32>,
) {
    let width = ui.available_width();
    let tile = if compact { 48.0 } else { 64.0 };
    let pad = if compact { 18.0 } else { 24.0 };
    let text_w = width - pad * 3.0 - tile;
    let sub = ui.painter().layout(
        subtitle.to_string(),
        theme::font(Weight::Regular, 16.0),
        theme::TEXT_2,
        text_w,
    );
    let title_h = if compact { 28.0 } else { 32.0 };
    let h = (pad * 2.0 + title_h + 6.0 + sub.size().y).max(pad * 2.0 + tile);
    let (r, _) = ui.allocate_exact_size(Vec2::new(width, h), Sense::hover());
    if !ui.is_rect_visible(r) {
        return;
    }
    let (phase, pulse) = match celebrate {
        Some(t) => {
            ui.ctx().request_repaint();
            let fade = (1.0 - t / CELEBRATE_SECS).clamp(0.0, 1.0);
            (t * 0.35, 1.0 + fade * (0.6 + 0.4 * (t * 3.0).sin()))
        }
        None => (0.0, 1.0),
    };
    glow(ui, r, 18, 0.55 * pulse);
    ui.painter()
        .rect_filled(r, CornerRadius::same(18), theme::SURFACE);
    fill(ui, r, 18.0, 0.10);
    outline(ui, r.shrink(0.75), 18.0, 1.5, 0.85, phase);

    // Icon tile: the gradient, glasses and a sparkle.
    let t = Rect::from_min_size(
        Pos2::new(r.min.x + pad, r.center().y - tile / 2.0),
        Vec2::splat(tile),
    );
    glow(ui, t, (tile * 0.28) as u8, 0.8);
    fill(ui, t, tile * 0.28, 1.0);
    let p = ui.painter();
    p.text(
        t.center() + Vec2::new(0.0, tile * 0.04),
        egui::Align2::CENTER_CENTER,
        icons::EYEGLASSES,
        theme::icon(tile * 0.5),
        Color32::WHITE,
    );
    p.text(
        Pos2::new(t.max.x - tile * 0.2, t.min.y + tile * 0.22),
        egui::Align2::CENTER_CENTER,
        icons::SPARKLE,
        theme::icon_fill(tile * 0.24),
        Color32::WHITE,
    );

    // Title and badge, subtitle under them.
    let x = t.max.x + pad;
    let top = r.center().y - (title_h + 6.0 + sub.size().y) / 2.0;
    let tg = p.layout_no_wrap(
        title.to_string(),
        theme::font(Weight::Bold, if compact { 22.0 } else { 26.0 }),
        theme::TEXT,
    );
    let tw = tg.size().x;
    p.galley(
        Pos2::new(x, top + (title_h - tg.size().y) / 2.0),
        tg,
        theme::TEXT,
    );
    let mut b = ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_min_size(
        Pos2::new(x + tw + 12.0, top + (title_h - 24.0) / 2.0),
        Vec2::new(200.0, 24.0),
    )));
    badge(&mut b, badge_text);
    ui.painter()
        .galley(Pos2::new(x, top + title_h + 6.0), sub, theme::TEXT_2);
}

/// The main button of a paid feature: gradient pill with a sparkle.
pub fn button(ui: &mut Ui, label: &str) -> Response {
    let g = ui.painter().layout_no_wrap(
        label.to_string(),
        theme::font(Weight::Bold, 19.0),
        Color32::WHITE,
    );
    let ig = ui.painter().layout_no_wrap(
        icons::SPARKLE.to_string(),
        theme::icon_fill(20.0),
        Color32::WHITE,
    );
    let w = ig.size().x + 10.0 + g.size().x + 56.0;
    let (r, resp) = ui.allocate_exact_size(Vec2::new(w, 56.0), Sense::click());
    if ui.is_rect_visible(r) {
        let hover = ui.ctx().animate_bool(resp.id, resp.hovered());
        let r2 = r.shrink(if resp.is_pointer_button_down_on() {
            1.5
        } else {
            0.0
        });
        glow(ui, r2, 28, 0.7 + 0.6 * hover);
        fill(ui, r2, 28.0, 1.0);
        if hover > 0.0 {
            ui.painter().rect_filled(
                r2,
                CornerRadius::same(28),
                Color32::from_white_alpha((hover * 22.0) as u8),
            );
        }
        let x = r.center().x - (w - 56.0) / 2.0;
        let p = ui.painter();
        p.galley(
            Pos2::new(x, r.center().y - ig.size().y / 2.0),
            ig.clone(),
            Color32::WHITE,
        );
        p.galley(
            Pos2::new(x + ig.size().x + 10.0, r.center().y - g.size().y / 2.0),
            g,
            Color32::WHITE,
        );
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A section label in the gradient, letter by letter.
pub fn label(ui: &mut Ui, text: &str) {
    let text = text.to_uppercase();
    let n = text.chars().count().max(2) as f32 - 1.0;
    let mut job = LayoutJob::default();
    for (i, ch) in text.chars().enumerate() {
        job.append(
            &ch.to_string(),
            0.0,
            egui::TextFormat {
                font_id: theme::font(Weight::SemiBold, 13.0),
                color: grad(i as f32 / n, 1.0),
                extra_letter_spacing: 1.4,
                ..Default::default()
            },
        );
    }
    ui.label(job);
}

/// A colour choice: a swatch dot and its name; the chosen one has a
/// gradient ring.
pub fn swatch(ui: &mut Ui, label: &str, color: [f32; 3], selected: bool) -> Response {
    let g = ui.painter().layout_no_wrap(
        label.to_string(),
        theme::font(Weight::Medium, 16.0),
        Color32::WHITE,
    );
    let h = 44.0;
    let dot = 20.0;
    let (r, resp) = ui.allocate_exact_size(
        Vec2::new(14.0 + dot + 10.0 + g.size().x + 18.0, h),
        Sense::click(),
    );
    if ui.is_rect_visible(r) {
        let hover = ui.ctx().animate_bool(resp.id, resp.hovered());
        let bg = lerp(theme::SURFACE_2, theme::SURFACE_3, hover);
        ui.painter().rect_filled(r, CornerRadius::same(22), bg);
        if selected {
            fill(ui, r, 22.0, 0.16);
            outline(ui, r.shrink(1.0), 22.0, 2.0, 1.0, 0.0);
        }
        let [cr, cg, cb] = color.map(|c| (c.clamp(0.0, 1.0) * 255.0) as u8);
        let c = Pos2::new(r.min.x + 14.0 + dot / 2.0, r.center().y);
        let p = ui.painter();
        p.circle_filled(c, dot / 2.0, Color32::from_rgb(cr, cg, cb));
        p.circle_stroke(c, dot / 2.0, (1.0, Color32::from_white_alpha(60)));
        let fg = if selected {
            Color32::WHITE
        } else {
            theme::TEXT_2
        };
        p.galley(
            Pos2::new(c.x + dot / 2.0 + 10.0, r.center().y - g.size().y / 2.0),
            g,
            fg,
        );
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}
