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

/// A one-word label over a control while the pointer is on it. egui's own
/// tooltips wait for the pointer to rest, which a controller ray never
/// quite does, so they never showed in the headset.
pub trait Tip {
    fn tip(self, word: &str) -> Self;
}

impl Tip for Response {
    fn tip(self, word: &str) -> Response {
        let t = self
            .ctx
            .animate_bool_with_time(self.id.with("tip"), self.hovered(), 0.1);
        if t > 0.01 {
            let painter = self.ctx.layer_painter(egui::LayerId::new(
                egui::Order::Tooltip,
                self.id.with("tip"),
            ));
            let g = painter.layout_no_wrap(
                word.to_string(),
                theme::font(Weight::SemiBold, 15.0),
                theme::BG,
            );
            let size = g.size() + Vec2::new(18.0, 10.0);
            let screen = self.ctx.screen_rect();
            // Above the control, or below it at the top edge of the panel.
            let above = self.rect.top() - size.y - 8.0;
            let y = if above >= screen.top() + 2.0 {
                above + (1.0 - t) * 4.0
            } else {
                self.rect.bottom() + 8.0 - (1.0 - t) * 4.0
            };
            let x = (self.rect.center().x - size.x / 2.0)
                .clamp(screen.left() + 4.0, screen.right() - size.x - 4.0);
            let r = Rect::from_min_size(Pos2::new(x, y), size);
            painter.rect_filled(r, CornerRadius::same(8), theme::TEXT.gamma_multiply(t));
            painter.galley_with_override_text_color(
                r.min + Vec2::new(9.0, 5.0),
                g,
                theme::BG.gamma_multiply(t),
            );
        }
        self
    }
}

fn dimensional_id() -> egui::Id {
    egui::Id::new("fp-dimensional")
}

/// Draws this panel's widgets with depth: raised cards and keys, sunken
/// tracks and trays (the adjust panel). Other panels stay flat.
pub fn set_dimensional(ctx: &egui::Context, on: bool) {
    ctx.data_mut(|d| d.insert_temp(dimensional_id(), on));
}

fn dimensional(ui: &Ui) -> bool {
    ui.ctx()
        .data(|d| d.get_temp::<bool>(dimensional_id()))
        .unwrap_or(false)
}

/// Room round a floating panel's body in its panel, for the shadow the
/// body casts on the scene.
pub const SHADOW_ROOM: egui::Margin = egui::Margin {
    left: 20,
    right: 20,
    top: 8,
    bottom: 28,
};

/// Paints a panel's body as a raised slab, lit along its top edge and
/// casting a soft shadow, under everything else in `ctx`; returns the
/// body's rect (the panel less [`SHADOW_ROOM`]).
pub fn panel_slab(ctx: &egui::Context, radius: f32) -> Rect {
    let rect = ctx.screen_rect() - SHADOW_ROOM;
    let p = ctx.layer_painter(egui::LayerId::background());
    p.add(
        egui::epaint::Shadow {
            offset: [0, 12],
            blur: 30,
            spread: 0,
            color: Color32::from_black_alpha(140),
        }
        .as_shape(rect, CornerRadius::same(radius as u8)),
    );
    super::depth::raised(
        &p,
        rect,
        radius,
        super::depth::Raised {
            top: Color32::from_rgb(29, 36, 46),
            bottom: Color32::from_rgb(14, 18, 24),
            light: Color32::from_white_alpha(30),
            lift: 0.6,
            glow: None,
        },
    );
    super::depth::edge(
        &p,
        rect,
        radius,
        2.0,
        Color32::from_black_alpha(140),
        super::depth::Side::Bottom,
        false,
    );
    rect
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
    if ui.is_rect_visible(rect) && dimensional(ui) && kind != Kind::Ghost {
        let (face, fg) = key_face_kind(ui, &resp, height * 0.25, false, kind);
        let mut x = face.center().x - content / 2.0;
        let p = ui.painter();
        if let Some(g) = icon_g {
            let y = face.center().y - g.size().y / 2.0;
            p.galley_with_override_text_color(Pos2::new(x, y), g.clone(), fg);
            x += g.size().x + gap;
        }
        let y = face.center().y - text.size().y / 2.0;
        p.galley_with_override_text_color(Pos2::new(x, y), text, fg);
    } else if ui.is_rect_visible(rect) {
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
    if dimensional(ui) {
        return key(ui, icon, size, selected);
    }
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

const KEY_TOP: Color32 = Color32::from_rgb(58, 69, 83);
const KEY_BOTTOM: Color32 = Color32::from_rgb(35, 42, 51);
const KEY_TOP_HOVER: Color32 = Color32::from_rgb(72, 84, 98);
const KEY_BOTTOM_HOVER: Color32 = Color32::from_rgb(45, 54, 65);
const KEY_TOP_ON: Color32 = Color32::from_rgb(31, 76, 114);
const KEY_BOTTOM_ON: Color32 = Color32::from_rgb(21, 52, 82);
const KEY_FG_ON: Color32 = Color32::from_rgb(125, 200, 255);
/// The sunken trays that hold groups of keys.
pub const WELL: Color32 = Color32::from_rgb(10, 13, 17);

/// Paints the raised body of a key filling `resp.rect` (less a margin for
/// its shadow), lifted under the pointer and sunk while pressed; returns
/// the body's rect and the colour for what goes on it.
pub fn key_face(ui: &Ui, resp: &Response, radius: f32, selected: bool) -> (Rect, Color32) {
    key_face_kind(ui, resp, radius, selected, Kind::Secondary)
}

/// [`key_face`] for a button of `kind`: Primary is an accent key, Danger
/// has red text.
pub fn key_face_kind(
    ui: &Ui,
    resp: &Response,
    radius: f32,
    selected: bool,
    kind: Kind,
) -> (Rect, Color32) {
    if kind == Kind::Primary {
        let t = anim(ui, resp.id, resp.hovered());
        let down = resp.is_pointer_button_down_on();
        let dy = if down { 0.5 } else { -1.5 * t };
        let rect = resp.rect.shrink(3.0).translate(Vec2::new(0.0, dy));
        let look = super::depth::Raised {
            top: mix(
                Color32::from_rgb(92, 190, 255),
                Color32::from_rgb(128, 208, 255),
                t,
            ),
            bottom: mix(
                Color32::from_rgb(16, 112, 196),
                Color32::from_rgb(24, 132, 224),
                t,
            ),
            light: Color32::from_white_alpha(130),
            lift: if down { 0.2 } else { 0.7 + 0.5 * t },
            glow: Some(theme::ACCENT.gamma_multiply(0.3 + 0.2 * t)),
        };
        super::depth::raised(ui.painter(), rect, radius.min(rect.height() / 2.0), look);
        return (rect, Color32::WHITE);
    }
    let t = anim(ui, resp.id, resp.hovered());
    let s = anim(ui, resp.id.with("sel"), selected);
    let down = resp.is_pointer_button_down_on();
    let dy = if down { 0.5 } else { -1.5 * t };
    let rect = resp.rect.shrink(3.0).translate(Vec2::new(0.0, dy));
    let shade = |c: Color32| {
        if down {
            mix(c, Color32::BLACK, 0.15)
        } else {
            c
        }
    };
    let look = super::depth::Raised {
        top: shade(mix(mix(KEY_TOP, KEY_TOP_HOVER, t), KEY_TOP_ON, s)),
        bottom: shade(mix(mix(KEY_BOTTOM, KEY_BOTTOM_HOVER, t), KEY_BOTTOM_ON, s)),
        light: mix(
            Color32::from_white_alpha((70.0 + 30.0 * t) as u8),
            KEY_FG_ON.gamma_multiply(0.4),
            s,
        ),
        lift: if down { 0.2 } else { 0.6 + 0.6 * t },
        glow: (s > 0.0).then(|| theme::ACCENT.gamma_multiply(0.35 * s)),
    };
    super::depth::raised(ui.painter(), rect, radius.min(rect.height() / 2.0), look);
    let fg = mix(mix(theme::TEXT, Color32::WHITE, t), KEY_FG_ON, s);
    let fg = if kind == Kind::Danger {
        theme::ERROR
    } else {
        fg
    };
    (rect, fg)
}

/// A raised round key, for the control bar. `selected` marks a toggle
/// that is on.
pub fn key(ui: &mut Ui, icon: &str, size: f32, selected: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    if ui.is_rect_visible(rect) {
        let (face, fg) = key_face(ui, &resp, size, selected);
        ui.painter().text(
            face.center(),
            Align2::CENTER_CENTER,
            icon,
            theme::icon(size * 0.46),
            fg,
        );
    }
    resp
}

/// A raised pill key with a short label, for the control bar.
pub fn pill_key(ui: &mut Ui, label: &str, selected: bool) -> Response {
    let g = ui.painter().layout_no_wrap(
        label.to_string(),
        theme::font(Weight::SemiBold, 17.0),
        Color32::WHITE,
    );
    let size = Vec2::new((g.size().x + 34.0).max(52.0), 46.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let (face, fg) = key_face(ui, &resp, size.y, selected);
        ui.painter()
            .galley_with_override_text_color(face.center() - g.size() / 2.0, g, fg);
    }
    resp
}

/// A sunken tray holding a group of keys on the control bar.
pub fn well<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let slot = ui.painter().add(egui::Shape::Noop);
    let inner = egui::Frame::new()
        .inner_margin(egui::Margin::same(5))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            add(ui)
        });
    let r = inner.response.rect;
    ui.painter().set(
        slot,
        egui::Shape::Vec(super::depth::sunken_shapes(r, r.height() / 2.0, WELL)),
    );
    inner.inner
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
    if ui.is_rect_visible(rect) && dimensional(ui) {
        let (face, fg) = key_face(ui, &resp, h / 2.0, selected);
        let p = ui.painter();
        let mut x = face.center().x - content / 2.0;
        if let Some(g) = icon_g {
            p.galley_with_override_text_color(
                Pos2::new(x, face.center().y - g.size().y / 2.0),
                g.clone(),
                fg,
            );
            x += g.size().x + gap;
        }
        p.galley_with_override_text_color(
            Pos2::new(x, face.center().y - text.size().y / 2.0),
            text,
            fg,
        );
    } else if ui.is_rect_visible(rect) {
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
    if ui.is_rect_visible(rect) && dimensional(ui) {
        // A sunken slot that fills with the accent, a raised knob in it.
        let h = anim(ui, resp.id, resp.hovered());
        let t = anim(ui, resp.id.with("on"), *on);
        let p = ui.painter();
        super::depth::sunken(p, rect, 16.0, WELL);
        if t > 0.0 {
            let fill = rect.shrink(1.0);
            p.rect_filled(
                fill,
                CornerRadius::same(15),
                theme::ACCENT.gamma_multiply(t),
            );
            super::depth::fill_vgradient(
                p,
                fill,
                15.0,
                Color32::from_rgb(98, 196, 255).gamma_multiply(t),
                Color32::from_rgb(22, 132, 226).gamma_multiply(t),
            );
            super::depth::edge(
                p,
                rect,
                16.0,
                4.0,
                Color32::from_black_alpha(120),
                super::depth::Side::Top,
                false,
            );
        }
        let x = egui::lerp((rect.left() + 16.0)..=(rect.right() - 16.0), t);
        knob(p, Pos2::new(x, rect.center().y - h), 12.0 + h);
    } else if ui.is_rect_visible(rect) {
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
    if dimensional(ui) {
        let slot = ui.painter().add(egui::Shape::Noop);
        let inner = egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(4, 4))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 0.0;
                add(ui)
            });
        let look = super::depth::Raised {
            top: Color32::from_rgb(30, 37, 47),
            bottom: Color32::from_rgb(21, 26, 33),
            light: Color32::from_white_alpha(26),
            lift: 0.5,
            glow: None,
        };
        ui.painter().set(
            slot,
            egui::Shape::Vec(super::depth::raised_shapes(inner.response.rect, 14.0, look)),
        );
        return inner.inner;
    }
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
    pub fn row<R>(
        &mut self,
        title: &str,
        desc: Option<&str>,
        control: impl FnOnce(&mut Ui) -> R,
    ) -> R {
        let ui = &mut *self.ui;
        if self.n > 0 {
            separator(ui);
        }
        self.n += 1;
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(16, 12))
            .show(ui, |ui| {
                // Text on the left (leaving room for a slider row's control,
                // wrapping if longer) and the control on the right, both
                // centred on the row's height.
                let w = ui.available_width();
                let text_w = (w - 420.0).max(w * 0.4);
                let title_g = ui.painter().layout(
                    title.to_string(),
                    theme::font(Weight::Medium, 18.0),
                    theme::TEXT,
                    text_w,
                );
                let desc_g = desc.map(|d| {
                    ui.painter().layout(
                        d.to_string(),
                        theme::font(Weight::Regular, 15.0),
                        theme::TEXT_2,
                        text_w,
                    )
                });
                let text_h =
                    title_g.size().y + desc_g.as_ref().map(|g| 3.0 + g.size().y).unwrap_or(0.0);
                let (rect, _) =
                    ui.allocate_exact_size(Vec2::new(w, text_h.max(48.0)), Sense::hover());
                let mut y = rect.center().y - text_h / 2.0;
                let p = ui.painter();
                let title_h = title_g.size().y;
                p.galley(Pos2::new(rect.left(), y), title_g, theme::TEXT);
                y += title_h + 3.0;
                if let Some(g) = desc_g {
                    p.galley(Pos2::new(rect.left(), y), g, theme::TEXT_2);
                }
                let control_rect = Rect::from_min_max(
                    Pos2::new(rect.left() + text_w + 12.0, rect.top()),
                    rect.max,
                );
                let mut c = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(control_rect)
                        .layout(Layout::right_to_left(Align::Center)),
                );
                c.spacing_mut().item_spacing.x = 10.0;
                control(&mut c)
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
            separator(ui);
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

/// The line between rows: a hairline, or a groove when dimensional.
fn separator(ui: &Ui) {
    let x = ui.max_rect().x_range();
    let x = (x.min + 16.0)..=(x.max - 16.0);
    let y = ui.cursor().top();
    if dimensional(ui) {
        ui.painter().hline(
            x.clone(),
            y - 0.5,
            Stroke::new(1.0_f32, Color32::from_black_alpha(120)),
        );
        ui.painter().hline(
            x,
            y + 0.5,
            Stroke::new(1.0_f32, Color32::from_white_alpha(12)),
        );
    } else {
        ui.painter()
            .hline(x, y, Stroke::new(1.0_f32, theme::STROKE));
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
        if resp.tip("Reset").clicked() {
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
    changed |= slider(ui, value, range, 240.0).changed();
    changed
}

/// A slider: a slim rail filled in the accent up to a white knob.
pub fn slider(
    ui: &mut Ui,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    width: f32,
) -> Response {
    let (rect, mut resp) = ui.allocate_exact_size(Vec2::new(width, 36.0), Sense::click_and_drag());
    let (lo, hi) = (*range.start(), *range.end());
    let inset = 11.0;
    let rail_x = (rect.left() + inset)..=(rect.right() - inset);
    if resp.is_pointer_button_down_on()
        && let Some(p) = resp.interact_pointer_pos()
    {
        let f = ((p.x - rail_x.start()) / (rail_x.end() - rail_x.start())).clamp(0.0, 1.0);
        let v = lo + f * (hi - lo);
        if (v - *value).abs() > f32::EPSILON {
            *value = v;
            resp.mark_changed();
        }
    }
    if ui.is_rect_visible(rect) {
        let t = anim(ui, resp.id, resp.hovered() || resp.dragged());
        let f = if hi > lo {
            ((*value - lo) / (hi - lo)).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let x = egui::lerp(rail_x.clone(), f);
        let h = 7.0 + t * 2.0;
        let y = rect.center().y;
        let rail = Rect::from_x_y_ranges(rail_x.clone(), (y - h / 2.0)..=(y + h / 2.0));
        let done = Rect::from_x_y_ranges(*rail_x.start()..=x, rail.y_range());
        super::depth::sunken(ui.painter(), rail, h / 2.0, WELL);
        rail_fill(ui.painter(), rail, done);
        knob(ui.painter(), Pos2::new(x, y), 9.0 + t * 2.0);
    }
    resp
}

/// The accent fill over the `done` part of a sunken rail (seek bar,
/// sliders), glowing a little.
pub fn rail_fill(p: &egui::Painter, rail: Rect, done: Rect) {
    let r = rail.height() / 2.0;
    if done.width() > 0.5 {
        let done = done.intersect(rail);
        p.add(
            egui::epaint::Shadow {
                offset: [0, 0],
                blur: 10,
                spread: 0,
                color: theme::ACCENT.gamma_multiply(0.3),
            }
            .as_shape(done, CornerRadius::same(r as u8)),
        );
        p.rect_filled(done, CornerRadius::same(r as u8), theme::ACCENT);
        super::depth::fill_vgradient(
            p,
            done,
            r,
            Color32::from_rgb(98, 196, 255),
            Color32::from_rgb(22, 132, 226),
        );
    }
}

/// The white knob on a rail: a raised disc of radius `r` at `c`.
pub fn knob(p: &egui::Painter, c: Pos2, r: f32) {
    super::depth::raised(
        p,
        Rect::from_center_size(c, Vec2::splat(r * 2.0)),
        r,
        super::depth::Raised {
            top: Color32::WHITE,
            bottom: Color32::from_rgb(208, 215, 224),
            light: Color32::WHITE,
            lift: 0.8,
            glow: None,
        },
    );
}

/// A row of equal segments, each an icon over a label, one selected;
/// returns the segment clicked.
pub fn segmented(ui: &mut Ui, items: &[(&str, &str)], selected: usize) -> Option<usize> {
    let h = 66.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), Sense::hover());
    let dim = dimensional(ui);
    if dim {
        super::depth::sunken(ui.painter(), rect, 16.0, WELL);
    } else {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(16), theme::SURFACE);
    }
    let n = items.len().max(1) as f32;
    let w = (rect.width() - 8.0) / n;
    let mut clicked = None;
    for (i, (icon, label)) in items.iter().enumerate() {
        let seg = Rect::from_min_size(
            Pos2::new(rect.left() + 4.0 + i as f32 * w, rect.top() + 4.0),
            Vec2::new(w, h - 8.0),
        );
        let resp = ui.interact(seg, ui.id().with(("segment", i)), Sense::click());
        let t = anim(ui, resp.id, resp.hovered());
        let s = anim(ui, resp.id.with("sel"), i == selected);
        let p = ui.painter();
        if dim {
            // The chosen tab is a raised key in the tray; the others rise
            // a little under the pointer.
            let lift = (t * (1.0 - s)).max(s);
            if lift > 0.01 {
                let down = resp.is_pointer_button_down_on();
                let face = seg.shrink(2.0).translate(Vec2::new(0.0, -t * (1.0 - s)));
                super::depth::raised(
                    p,
                    face,
                    11.0,
                    super::depth::Raised {
                        top: mix(KEY_TOP, KEY_TOP_HOVER, t).gamma_multiply(lift),
                        bottom: mix(KEY_BOTTOM, KEY_BOTTOM_HOVER, t).gamma_multiply(lift),
                        light: Color32::from_white_alpha((60.0 * lift) as u8),
                        lift: if down { 0.2 } else { 0.7 * lift },
                        glow: None,
                    },
                );
            }
        } else {
            let bg = Color32::from_white_alpha((t * 10.0) as u8).lerp_to_gamma(theme::SURFACE_3, s);
            p.rect_filled(seg, CornerRadius::same(12), bg);
        }
        let fg = mix(mix(theme::TEXT_2, theme::TEXT, t), Color32::WHITE, s);
        p.text(
            seg.center() - Vec2::new(0.0, 10.0),
            Align2::CENTER_CENTER,
            *icon,
            theme::icon(22.0),
            mix(fg, theme::ACCENT_HOVER, s),
        );
        p.text(
            seg.center() + Vec2::new(0.0, 14.0),
            Align2::CENTER_CENTER,
            *label,
            theme::font(Weight::SemiBold, 13.0),
            fg,
        );
        if resp.clicked() {
            clicked = Some(i);
        }
    }
    clicked
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
        let dim = dimensional(ui);
        let p = ui.painter();
        let mut rect = rect;
        if dim {
            // The chosen tab is a raised key; others rise under the pointer.
            if selected || t > 0.01 {
                let (face, _) = key_face(ui, &resp, 12.0, false);
                rect = face;
            }
        } else {
            p.rect_filled(
                rect,
                CornerRadius::same(12),
                Color32::from_white_alpha((t * 12.0) as u8),
            );
        }
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
        if s > 0.0 && !dim {
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

/// A play triangle `size` tall, its centroid on `center` so it looks
/// centred in a circle (a glyph's box centre sits too far left).
pub fn play_mark(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    let h = size;
    let w = h * 0.88;
    let left = center.x - w / 3.0;
    let pts = vec![
        Pos2::new(left, center.y - h / 2.0),
        Pos2::new(left + w, center.y),
        Pos2::new(left, center.y + h / 2.0),
    ];
    // A hairline in the same colour softens the points a little.
    painter.add(egui::Shape::convex_polygon(
        pts,
        color,
        Stroke::new(2.0_f32, color),
    ));
}

/// A pause mark (two rounded bars) `size` tall, centred on `center`.
pub fn pause_mark(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    let bar = Vec2::new(size * 0.28, size);
    let gap = size * 0.22;
    for dx in [-(gap / 2.0 + bar.x / 2.0), gap / 2.0 + bar.x / 2.0] {
        painter.rect_filled(
            Rect::from_center_size(center + Vec2::new(dx, 0.0), bar),
            CornerRadius::same((bar.x * 0.3) as u8),
            color,
        );
    }
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
