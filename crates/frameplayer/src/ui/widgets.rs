//! Shared components: buttons, chips, switches, settings rows, tabs and
//! toasts, all in the theme's colors and type. Hover and press animate (a
//! pointer ray needs clear feedback), and every target is at least 44
//! points, a comfortable aim at arm's length.

use super::theme::{self, Weight};
use egui::{
    Align, Align2, Color32, CornerRadius, Layout, Pos2, Rect, Response, Sense, Stroke, StrokeKind,
    Ui, Vec2, text::LayoutJob,
};

const ANIM: f32 = 0.12;

/// 0 → 1 as `on` turns true, eased over [`ANIM`].
fn anim(ui: &Ui, id: egui::Id, on: bool) -> f32 {
    ui.ctx().animate_bool_with_time(id, on, ANIM)
}

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    a.lerp_to_gamma(b, t.clamp(0.0, 1.0))
}

/// What a button is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The one main action on a screen: accent fill.
    Primary,
    /// Other actions: raised surface.
    Secondary,
    /// Low-emphasis actions: no fill until hovered.
    Ghost,
    /// Destructive actions.
    Danger,
}

/// A rounded button with an optional leading icon.
pub fn button(ui: &mut Ui, icon: Option<&str>, label: &str, kind: Kind) -> Response {
    button_sized(ui, icon, label, kind, 48.0)
}

pub fn button_sized(
    ui: &mut Ui,
    icon: Option<&str>,
    label: &str,
    kind: Kind,
    height: f32,
) -> Response {
    let font = theme::font(Weight::SemiBold, (height * 0.375).max(15.0));
    let text = ui
        .painter()
        .layout_no_wrap(label.to_string(), font, Color32::WHITE);
    let icon_g = icon.map(|i| {
        ui.painter()
            .layout_no_wrap(i.to_string(), theme::icon(height * 0.46), Color32::WHITE)
    });
    let pad = height * 0.42;
    let gap = if label.is_empty() { 0.0 } else { 10.0 };
    let content = text.size().x + icon_g.as_ref().map(|g| g.size().x + gap).unwrap_or(0.0);
    let size = Vec2::new((content + pad * 2.0).max(height), height);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let t = anim(ui, resp.id, resp.hovered());
        let down = resp.is_pointer_button_down_on();
        let (bg, fg) = match kind {
            Kind::Primary => (mix(theme::ACCENT, theme::ACCENT_HOVER, t), Color32::WHITE),
            Kind::Secondary => (mix(theme::SURFACE_2, theme::SURFACE_3, t), theme::TEXT),
            Kind::Ghost => (
                Color32::from_white_alpha((t * 16.0) as u8),
                mix(theme::TEXT_2, Color32::WHITE, t),
            ),
            Kind::Danger => (
                mix(theme::SURFACE_2, theme::ERROR.gamma_multiply(0.25), t),
                theme::ERROR,
            ),
        };
        let bg = if down {
            mix(bg, Color32::BLACK, 0.2)
        } else {
            bg
        };
        let r = rect.expand(t * 1.5);
        let radius = CornerRadius::same((height * 0.25) as u8);
        let p = ui.painter();
        p.rect_filled(r, radius, bg);
        if t > 0.0 && kind != Kind::Ghost {
            p.rect_stroke(
                r,
                radius,
                Stroke::new(1.5_f32, Color32::from_white_alpha((t * 60.0) as u8)),
                StrokeKind::Outside,
            );
        }
        let mut x = r.center().x - content / 2.0;
        if let Some(g) = icon_g {
            let y = r.center().y - g.size().y / 2.0;
            p.galley_with_override_text_color(Pos2::new(x, y), g.clone(), fg);
            x += g.size().x + gap;
        }
        let y = r.center().y - text.size().y / 2.0;
        p.galley_with_override_text_color(Pos2::new(x, y), text, fg);
    }
    resp
}

/// A round icon-only button. `selected` marks a toggle that is on.
pub fn icon_button(ui: &mut Ui, icon: &str, size: f32, selected: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    if ui.is_rect_visible(rect) {
        let t = anim(ui, resp.id, resp.hovered());
        let down = resp.is_pointer_button_down_on();
        let p = ui.painter();
        let c = rect.center();
        let radius = size / 2.0 + t * 1.5;
        let bg = if selected {
            mix(
                theme::ACCENT.gamma_multiply(0.22),
                theme::ACCENT.gamma_multiply(0.34),
                t,
            )
        } else {
            Color32::from_white_alpha((t * 20.0) as u8 + if down { 14 } else { 0 })
        };
        p.circle_filled(c, radius, bg);
        if t > 0.0 {
            p.circle_stroke(
                c,
                radius,
                Stroke::new(1.5_f32, Color32::from_white_alpha((t * 50.0) as u8)),
            );
        }
        let fg = if selected {
            theme::ACCENT_HOVER
        } else {
            mix(theme::TEXT, Color32::WHITE, t)
        };
        p.text(c, Align2::CENTER_CENTER, icon, theme::icon(size * 0.48), fg);
    }
    resp
}

/// A chip: a short choice in a row of choices (filters, formats, tabs).
pub fn chip(ui: &mut Ui, label: &str, selected: bool) -> Response {
    chip_icon(ui, None, label, selected)
}

pub fn chip_icon(ui: &mut Ui, icon: Option<&str>, label: &str, selected: bool) -> Response {
    let h = 40.0;
    let text = ui.painter().layout_no_wrap(
        label.to_string(),
        theme::font(Weight::Medium, 16.0),
        Color32::WHITE,
    );
    let icon_g = icon.map(|i| {
        ui.painter()
            .layout_no_wrap(i.to_string(), theme::icon(19.0), Color32::WHITE)
    });
    let gap = if icon_g.is_some() && !label.is_empty() {
        8.0
    } else {
        0.0
    };
    let content = text.size().x + icon_g.as_ref().map(|g| g.size().x + gap).unwrap_or(0.0);
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new((content + 32.0).max(h), h), Sense::click());
    if ui.is_rect_visible(rect) {
        let t = anim(ui, resp.id, resp.hovered());
        let s = anim(ui, resp.id.with("sel"), selected);
        let bg = mix(mix(theme::SURFACE_2, theme::SURFACE_3, t), theme::TEXT, s);
        let fg = mix(mix(theme::TEXT_2, Color32::WHITE, t), theme::BG, s);
        let p = ui.painter();
        p.rect_filled(rect, CornerRadius::same((h / 2.0) as u8), bg);
        let mut x = rect.center().x - content / 2.0;
        if let Some(g) = icon_g {
            p.galley_with_override_text_color(
                Pos2::new(x, rect.center().y - g.size().y / 2.0),
                g.clone(),
                fg,
            );
            x += g.size().x + gap;
        }
        p.galley_with_override_text_color(
            Pos2::new(x, rect.center().y - text.size().y / 2.0),
            text,
            fg,
        );
    }
    resp
}

/// An on/off switch.
pub fn switch(ui: &mut Ui, on: &mut bool) -> Response {
    let size = Vec2::new(56.0, 32.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        let h = anim(ui, resp.id, resp.hovered());
        let t = anim(ui, resp.id.with("on"), *on);
        let p = ui.painter();
        let track = mix(
            mix(theme::SURFACE_3, Color32::from_rgb(58, 68, 82), h),
            theme::ACCENT,
            t,
        );
        p.rect_filled(rect, CornerRadius::same(16), track);
        let r = 12.0 + h;
        let x = egui::lerp((rect.left() + 16.0)..=(rect.right() - 16.0), t);
        p.circle_filled(
            Pos2::new(x, rect.center().y + 1.0),
            r,
            Color32::from_black_alpha(60),
        );
        p.circle_filled(Pos2::new(x, rect.center().y), r, Color32::WHITE);
    }
    resp
}

/// Small spaced capitals over a group ("PLAYBACK").
pub fn section_label(ui: &mut Ui, text: &str) {
    let mut job = LayoutJob::default();
    job.append(
        &text.to_uppercase(),
        0.0,
        egui::TextFormat {
            font_id: theme::font(Weight::SemiBold, 13.0),
            color: theme::TEXT_3,
            extra_letter_spacing: 1.4,
            ..Default::default()
        },
    );
    ui.label(job);
}

/// A raised group of rows.
pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(theme::SURFACE)
        .stroke(Stroke::new(1.0_f32, theme::STROKE))
        .corner_radius(14)
        .inner_margin(egui::Margin::symmetric(4, 4))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            add(ui)
        })
        .inner
}

/// Rows inside a [`card`]: a title, an optional description and a control
/// on the right, with hairlines between rows.
pub struct Rows<'a> {
    ui: &'a mut Ui,
    n: usize,
}

pub fn rows(ui: &mut Ui, add: impl FnOnce(&mut Rows)) {
    card(ui, |ui| add(&mut Rows { ui, n: 0 }));
}

impl Rows<'_> {
    pub fn ui(&mut self) -> &mut Ui {
        self.ui
    }

    pub fn row<R>(
        &mut self,
        title: &str,
        desc: Option<&str>,
        control: impl FnOnce(&mut Ui) -> R,
    ) -> R {
        let ui = &mut *self.ui;
        if self.n > 0 {
            let x = ui.max_rect().x_range();
            let y = ui.cursor().top();
            ui.painter().hline(
                (x.min + 16.0)..=(x.max - 16.0),
                y,
                Stroke::new(1.0_f32, theme::STROKE),
            );
        }
        self.n += 1;
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(16, 12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.set_min_height(48.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    let r = control(ui);
                    let w = ui.available_width();
                    ui.allocate_ui_with_layout(
                        Vec2::new(w, 0.0),
                        Layout::top_down(Align::Min),
                        |ui| {
                            ui.spacing_mut().item_spacing.y = 3.0;
                            ui.label(
                                egui::RichText::new(title)
                                    .font(theme::font(Weight::Medium, 18.0))
                                    .color(theme::TEXT),
                            );
                            if let Some(d) = desc {
                                ui.label(
                                    egui::RichText::new(d)
                                        .font(theme::font(Weight::Regular, 15.0))
                                        .color(theme::TEXT_2),
                                );
                            }
                        },
                    );
                    r
                })
                .inner
            })
            .inner
    }

    /// A row with a switch; true when it changed.
    pub fn switch(&mut self, title: &str, desc: Option<&str>, on: &mut bool) -> bool {
        self.row(title, desc, |ui| switch(ui, on).changed())
    }

    /// A row with a slider, its value and a reset button; true when changed.
    #[allow(clippy::too_many_arguments)]
    pub fn slider(
        &mut self,
        title: &str,
        desc: Option<&str>,
        value: &mut f32,
        range: std::ops::RangeInclusive<f32>,
        default: f32,
        suffix: &str,
        decimals: usize,
    ) -> bool {
        self.row(title, desc, |ui| {
            slider_control(ui, value, range, default, suffix, decimals)
        })
    }

    /// A full-width line of content (a hint, a list) inside the card.
    pub fn content<R>(&mut self, add: impl FnOnce(&mut Ui) -> R) -> R {
        let ui = &mut *self.ui;
        if self.n > 0 {
            let x = ui.max_rect().x_range();
            let y = ui.cursor().top();
            ui.painter().hline(
                (x.min + 16.0)..=(x.max - 16.0),
                y,
                Stroke::new(1.0_f32, theme::STROKE),
            );
        }
        self.n += 1;
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(16, 12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                add(ui)
            })
            .inner
    }
}

/// Slider, value and reset, laid out right to left (inside a row).
pub fn slider_control(
    ui: &mut Ui,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    default: f32,
    suffix: &str,
    decimals: usize,
) -> bool {
    let mut changed = false;
    let reset = (*value - default).abs() > 1e-4;
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::click());
    if reset {
        let t = anim(ui, resp.id, resp.hovered());
        ui.painter().circle_filled(
            rect.center(),
            19.0,
            Color32::from_white_alpha((t * 20.0) as u8),
        );
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            super::icons::ARROW_COUNTER_CLOCKWISE,
            theme::icon(20.0),
            mix(theme::TEXT_2, Color32::WHITE, t),
        );
        if resp.on_hover_text("Reset").clicked() {
            *value = default;
            changed = true;
        }
    }
    ui.add_sized(
        [74.0, 32.0],
        egui::Label::new(
            egui::RichText::new(format!("{value:.decimals$}{suffix}"))
                .font(theme::font(Weight::Medium, 16.0))
                .color(theme::TEXT_2),
        ),
    );
    ui.spacing_mut().slider_width = 240.0;
    changed |= ui
        .add(egui::Slider::new(value, range).show_value(false))
        .changed();
    changed
}

/// A top-level navigation tab: icon and label, an accent bar under the
/// selected one.
pub fn nav_tab(ui: &mut Ui, icon: &str, label: &str, selected: bool) -> Response {
    let text = ui.painter().layout_no_wrap(
        label.to_string(),
        theme::font(Weight::SemiBold, 18.0),
        Color32::WHITE,
    );
    let icon_g = ui
        .painter()
        .layout_no_wrap(icon.to_string(), theme::icon(22.0), Color32::WHITE);
    let content = icon_g.size().x + 8.0 + text.size().x;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(content + 28.0, 52.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let t = anim(ui, resp.id, resp.hovered());
        let s = anim(ui, resp.id.with("sel"), selected);
        let p = ui.painter();
        p.rect_filled(
            rect,
            CornerRadius::same(12),
            Color32::from_white_alpha((t * 12.0) as u8),
        );
        let fg = mix(mix(theme::TEXT_2, theme::TEXT, t), Color32::WHITE, s);
        let mut x = rect.center().x - content / 2.0;
        p.galley_with_override_text_color(
            Pos2::new(x, rect.center().y - icon_g.size().y / 2.0),
            icon_g,
            mix(fg, theme::ACCENT_HOVER, s),
        );
        x += content - text.size().x;
        p.galley_with_override_text_color(
            Pos2::new(x, rect.center().y - text.size().y / 2.0),
            text,
            fg,
        );
        if s > 0.0 {
            let w = content * s;
            let bar = Rect::from_center_size(
                Pos2::new(rect.center().x, rect.bottom() - 2.0),
                Vec2::new(w, 3.0),
            );
            p.rect_filled(bar, CornerRadius::same(2), theme::ACCENT);
        }
    }
    resp
}

/// A vertical gradient filling `rect`.
pub fn vgradient(painter: &egui::Painter, rect: Rect, top: Color32, bottom: Color32) {
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(rect.left_top(), top);
    mesh.colored_vertex(rect.right_top(), top);
    mesh.colored_vertex(rect.left_bottom(), bottom);
    mesh.colored_vertex(rect.right_bottom(), bottom);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(1, 3, 2);
    painter.add(egui::Shape::mesh(mesh));
}

/// A horizontal gradient filling `rect`.
pub fn hgradient(painter: &egui::Painter, rect: Rect, left: Color32, right: Color32) {
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(rect.left_top(), left);
    mesh.colored_vertex(rect.right_top(), right);
    mesh.colored_vertex(rect.left_bottom(), left);
    mesh.colored_vertex(rect.right_bottom(), right);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(1, 3, 2);
    painter.add(egui::Shape::mesh(mesh));
}

/// A soft drop shadow under `rect`.
pub fn shadow(painter: &egui::Painter, rect: Rect, radius: u8, strength: f32) {
    if strength <= 0.0 {
        return;
    }
    let s = egui::epaint::Shadow {
        offset: [0, 10],
        blur: 28,
        spread: 0,
        color: Color32::from_black_alpha((strength * 170.0) as u8),
    };
    painter.add(s.as_shape(rect, CornerRadius::same(radius)));
}

/// A small label on a translucent dark pill, for badges over pictures.
pub fn badge(painter: &egui::Painter, text: &str, anchor: Pos2, align: Align2) {
    let g = painter.layout_no_wrap(
        text.to_string(),
        theme::font(Weight::SemiBold, 13.0),
        Color32::WHITE,
    );
    let r = align.anchor_size(anchor, g.size() + Vec2::new(14.0, 6.0));
    painter.rect_filled(r, CornerRadius::same(7), Color32::from_black_alpha(170));
    painter.galley(r.min + Vec2::new(7.0, 3.0), g, Color32::WHITE);
}

/// A video format in a few characters, for badges: "2D", "180° 3D",
/// "360°", "Fisheye 190° 3D".
pub fn format_short(f: &fp_core::format::VideoFormat) -> String {
    use fp_core::format::{Projection, StereoLayout};
    let stereo = f.stereo != StereoLayout::Mono;
    let p = match f.projection {
        Projection::Flat => return if stereo { "3D".into() } else { "2D".into() },
        Projection::Equirect { h_fov, .. } => format!("{}°", h_fov.round()),
        Projection::Fisheye { fov } => format!("Fisheye {}°", fov.round()),
        Projection::Eac { h_fov } => format!("EAC {}°", h_fov.round()),
    };
    if stereo { format!("{p} 3D") } else { p }
}

/// Words that name a video's format in its file name, dropped from titles
/// ("Aurora Timelapse_360_TB" → "Aurora Timelapse").
const FORMAT_WORDS: &[&str] = &[
    "180",
    "190",
    "200",
    "220",
    "360",
    "2d",
    "3d",
    "3dh",
    "3dv",
    "sbs",
    "hsbs",
    "fsbs",
    "lr",
    "rl",
    "tb",
    "bt",
    "ou",
    "ab",
    "htb",
    "vr",
    "vr180",
    "vr360",
    "mono",
    "stereo",
    "fisheye",
    "fisheye190",
    "fisheye200",
    "mkx200",
    "mkx220",
    "rf52",
    "eac",
    "4k",
    "5k",
    "6k",
    "7k",
    "8k",
    "12k",
    "60fps",
    "30fps",
    "hevc",
    "h265",
    "x265",
    "h264",
    "x264",
    "av1",
];

/// A file-derived title as shown in the UI: separators as spaces, trailing
/// format tags dropped. Keeps the original when nothing else would remain.
pub fn display_title(raw: &str) -> String {
    let spaced: String = raw
        .chars()
        .map(|c| if c == '_' || c == '.' { ' ' } else { c })
        .collect();
    let is_tag = |w: &&str| {
        let w = w.trim_matches(['-', '(', ')', '[', ']']);
        FORMAT_WORDS.iter().any(|f| f.eq_ignore_ascii_case(w))
    };
    let mut words: Vec<&str> = spaced.split_whitespace().collect();
    if words.iter().all(is_tag) {
        return raw.to_string();
    }
    while words.last().is_some_and(is_tag) {
        words.pop();
    }
    let out = words.join(" ");
    let out = out.trim_end_matches([' ', '-']).to_string();
    if out.is_empty() { raw.to_string() } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_drop_format_tags() {
        assert_eq!(display_title("Aurora Timelapse_360_TB"), "Aurora Timelapse");
        assert_eq!(
            display_title("Sunset Beach Walk_180_LR"),
            "Sunset Beach Walk"
        );
        assert_eq!(display_title("my.trip.8K.180.SBS"), "my trip");
        assert_eq!(display_title("Garden Party"), "Garden Party");
        // A tag-only name stays as it was.
        assert_eq!(display_title("180_SBS"), "180_SBS");
        // Tags mid-title stay: only trailing ones are format.
        assert_eq!(display_title("Top 360 Moments_LR"), "Top 360 Moments");
    }
}
