//! Labels, buttons, icon buttons, toggles, progress bars, spinners, badges,
//! tooltips and modal dialogs.

use crate::geom::{Rect, Vec2};
use crate::icons::{draw_icon, Icon};
use crate::layout::{Dir, FILL};
use crate::painter::Layer;
use crate::text::{Align, TextParams};
use crate::theme::Color;
use crate::ui::{Response, Sense, Ui};
use std::hash::Hash;

/// Visual variants for [`Ui::button_ex`].
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ButtonStyle {
    /// Accent-coloured call to action.
    pub primary: bool,
    /// Shown as toggled on (tabs, pickers).
    pub selected: bool,
    /// Take the full width of a vertical layout.
    pub fill: bool,
    pub icon: Option<Icon>,
    /// Explicit width (overrides intrinsic width).
    pub width: Option<f32>,
    pub danger: bool,
}

/// Result of [`Ui::modal`].
#[derive(Debug, Clone, PartialEq)]
pub struct ModalResponse<R> {
    pub inner: R,
    /// Backdrop clicked or B pressed.
    pub dismissed: bool,
}

impl Ui {
    /// Wrapped body text across the available width.
    pub fn label(&mut self, text: &str) -> Response {
        let (size, color) = (self.theme.text_size, self.theme.text);
        self.label_styled(text, size, color)
    }

    pub fn label_dim(&mut self, text: &str) -> Response {
        let (size, color) = (self.theme.small_text_size, self.theme.text_dim);
        self.label_styled(text, size, color)
    }

    pub fn heading(&mut self, text: &str) -> Response {
        let (size, color) = (self.theme.title_text_size, self.theme.text);
        self.label_styled(text, size, color)
    }

    pub fn label_styled(&mut self, text: &str, size: f32, color: Color) -> Response {
        let avail = self.available();
        let horizontal = self.layout_mut().dir == Dir::Horizontal;
        let params = if horizontal {
            TextParams::new(size)
        } else {
            TextParams::new(size).width(avail.w).wrap()
        };
        let layout = self.fonts.layout(text, params);
        let rect = if horizontal {
            self.allocate(Vec2::new(layout.size.x, FILL))
        } else {
            self.allocate(Vec2::new(avail.w, layout.size.y))
        };
        let pos = Vec2::new(rect.x, rect.y + (rect.h - layout.size.y) * 0.5);
        self.draw_text_at(pos, &layout, color);
        Response {
            id: self.make_id(("label", text)),
            rect,
            ..Default::default()
        }
    }

    pub fn button(&mut self, text: &str) -> Response {
        self.button_ex(text, ButtonStyle::default())
    }

    pub fn button_primary(&mut self, text: &str) -> Response {
        self.button_ex(
            text,
            ButtonStyle {
                primary: true,
                ..Default::default()
            },
        )
    }

    /// Button with optional leading icon and style flags.
    pub fn button_ex(&mut self, text: &str, style: ButtonStyle) -> Response {
        let t = &self.theme;
        let (pad, h, icon_s, ts) = (t.padding, t.widget_height, t.icon_size, t.text_size);
        let text_w = if text.is_empty() {
            0.0
        } else {
            self.fonts.measure(text, ts)
        };
        let icon_w = if style.icon.is_some() {
            icon_s + if text.is_empty() { 0.0 } else { pad * 0.5 }
        } else {
            0.0
        };
        let w = style.width.unwrap_or(text_w + icon_w + pad * 2.0).max(h);
        let rect = if style.fill {
            self.allocate_row(h)
        } else {
            self.allocate(Vec2::new(w, h))
        };
        let id = self.make_id(("button", text, style.icon));
        self.button_at(rect, id, text, style)
    }

    /// Draws/handles a button in an explicit rect.
    pub fn button_at(
        &mut self,
        rect: Rect,
        id: crate::ui::Id,
        text: &str,
        style: ButtonStyle,
    ) -> Response {
        let resp = self.interact(rect, id, Sense::CLICK);
        self.record(id, rect, || {
            if text.is_empty() {
                style.icon.map(|i| format!("{i:?}")).unwrap_or_default()
            } else {
                text.to_string()
            }
        });
        let t = &self.theme;
        let bg = if style.primary || style.selected {
            let base = if style.danger { t.danger } else { t.accent };
            let base = if style.selected && !style.primary {
                base.alpha(0.55)
            } else {
                base
            };
            if resp.pressed {
                base.lerp(Color::BLACK, 0.2)
            } else if resp.highlighted() {
                base.lerp(Color::WHITE, 0.15)
            } else {
                base
            }
        } else {
            self.surface_color(&resp)
        };
        let fg = if style.primary || style.selected {
            t.on_accent
        } else if style.danger {
            t.danger
        } else {
            t.text
        };
        let (radius, pad, icon_s, ts) = (t.corner_radius, t.padding, t.icon_size, t.text_size);
        self.painter.rect_rounded(rect, radius, bg);
        let text_w = if text.is_empty() {
            0.0
        } else {
            self.fonts.measure(text, ts).min(rect.w - pad * 2.0)
        };
        let icon_w = if style.icon.is_some() { icon_s } else { 0.0 };
        let gap = if style.icon.is_some() && !text.is_empty() {
            pad * 0.5
        } else {
            0.0
        };
        let content_w = text_w + icon_w + gap;
        let mut x = rect.x + ((rect.w - content_w) * 0.5).max(pad.min(rect.w * 0.5));
        if let Some(icon) = style.icon {
            let ir = Rect::new(x, rect.center().y - icon_s * 0.5, icon_s, icon_s);
            draw_icon(&mut self.painter, icon, ir, fg);
            x += icon_s + gap;
        }
        if !text.is_empty() {
            let tr = Rect::new(x, rect.y, (rect.right() - pad - x).max(0.0), rect.h);
            self.draw_text_in(tr, text, ts, fg, Align::Left);
        }
        self.focus_ring(&resp, radius);
        resp
    }

    /// Square icon-only button.
    pub fn icon_button(&mut self, key: impl Hash, icon: Icon) -> Response {
        let h = self.theme.widget_height;
        let rect = self.allocate(Vec2::new(h, h));
        self.icon_button_at(rect, key, icon, false)
    }

    pub fn icon_button_at(
        &mut self,
        rect: Rect,
        key: impl Hash,
        icon: Icon,
        selected: bool,
    ) -> Response {
        let id = self.make_id(("icon", key));
        let resp = self.interact(rect, id, Sense::CLICK);
        self.record(id, rect, || format!("{icon:?}"));
        let bg = if selected {
            self.theme.accent.alpha(0.6)
        } else {
            self.surface_color(&resp)
        };
        let r = rect.w.min(rect.h) * 0.5;
        self.painter.rect_rounded(rect, r, bg);
        let s = rect.w.min(rect.h) * 0.55;
        draw_icon(
            &mut self.painter,
            icon,
            Rect::from_center(rect.center(), Vec2::splat(s)),
            self.theme.text,
        );
        self.focus_ring(&resp, r);
        resp
    }

    /// Labelled on/off switch spanning the row. Flips on click / A.
    pub fn toggle(&mut self, label: &str, value: &mut bool) -> Response {
        let h = self.theme.widget_height;
        let rect = self.allocate_row(h);
        let id = self.make_id(("toggle", label));
        let mut resp = self.interact(rect, id, Sense::CLICK);
        self.record(id, rect, || label.to_string());
        if resp.clicked {
            *value = !*value;
            resp.changed = true;
        }
        let t = self.theme.clone();
        if resp.highlighted() {
            self.painter
                .rect_rounded(rect, t.corner_radius, t.surface_hover.alpha(0.5));
        }
        let sw = Vec2::new(h * 1.1, h * 0.55);
        let track = Rect::new(
            rect.right() - t.padding - sw.x,
            rect.center().y - sw.y * 0.5,
            sw.x,
            sw.y,
        );
        let on = self.animate(id.with("anim"), if *value { 1.0 } else { 0.0 }, 18.0);
        self.painter
            .rect_rounded(track, sw.y * 0.5, t.track.lerp(t.accent, on));
        let knob_r = sw.y * 0.5 - 3.0;
        let kx = track.x + sw.y * 0.5 + (track.w - sw.y) * on;
        self.painter
            .circle(Vec2::new(kx, track.center().y), knob_r, t.text);
        let label_rect = Rect::new(
            rect.x + t.padding,
            rect.y,
            (track.x - rect.x - t.padding * 2.0).max(0.0),
            rect.h,
        );
        self.draw_text_in(label_rect, label, t.text_size, t.text, Align::Left);
        self.focus_ring(&resp, t.corner_radius);
        resp
    }

    /// Horizontal progress bar, `fraction` in 0..1.
    pub fn progress_bar(&mut self, fraction: f32) -> Rect {
        let h = self.theme.track_thickness;
        let rect = self.allocate_row(h);
        self.draw_progress(rect, fraction);
        rect
    }

    pub fn draw_progress(&mut self, rect: Rect, fraction: f32) {
        let r = rect.h * 0.5;
        self.painter.rect_rounded(rect, r, self.theme.track);
        let f = fraction.clamp(0.0, 1.0);
        if f > 0.0 {
            self.painter.rect_rounded(
                Rect::new(rect.x, rect.y, (rect.w * f).max(rect.h), rect.h),
                r,
                self.theme.accent,
            );
        }
    }

    /// Indeterminate spinner of diameter `size`.
    pub fn spinner(&mut self, size: f32) -> Rect {
        let rect = self.allocate(Vec2::splat(size));
        self.draw_spinner(rect);
        rect
    }

    pub fn draw_spinner(&mut self, rect: Rect) {
        let r = rect.w.min(rect.h) * 0.5;
        let w = (r * 0.18).max(2.0);
        let t = self.time as f32;
        let start = t * 5.0;
        let span = 1.2 + 0.9 * (t * 2.3).sin().abs() * 2.0;
        self.painter.arc(
            rect.center(),
            r - w,
            w,
            0.0,
            std::f32::consts::TAU,
            self.theme.track,
        );
        self.painter.arc(
            rect.center(),
            r - w,
            w,
            start,
            start + span,
            self.theme.accent,
        );
    }

    pub fn separator(&mut self) {
        let rect = self.allocate_row(1.0);
        self.painter.rect_filled(rect, self.theme.border.alpha(0.6));
    }

    /// Small pill with text at `pos` (top-left); returns its rect.
    pub fn draw_badge(&mut self, pos: Vec2, text: &str, bg: Color, fg: Color) -> Rect {
        let ts = self.theme.small_text_size * 0.85;
        let layout = self.fonts.layout(text, TextParams::new(ts));
        let pad = Vec2::new(ts * 0.4, ts * 0.15);
        let rect = Rect::new(
            pos.x,
            pos.y,
            layout.size.x + pad.x * 2.0,
            layout.size.y + pad.y * 2.0,
        );
        self.painter.rect_rounded(rect, rect.h * 0.3, bg);
        self.draw_text_at(pos + pad, &layout, fg);
        rect
    }

    /// Floating text bubble above `anchor` on the tooltip layer, kept on-panel.
    pub fn tooltip(&mut self, anchor: Rect, text: &str) -> Rect {
        let ts = self.theme.small_text_size;
        let layout = self.fonts.layout(text, TextParams::new(ts));
        let pad = self.theme.padding * 0.5;
        let size = layout.size + Vec2::splat(pad * 2.0);
        let rect = tooltip_rect(anchor, size, self.size, self.theme.spacing);
        let prev = self.painter.set_layer(Layer::Tooltip);
        self.painter.push_clip_absolute(self.screen_rect());
        self.painter.rect_rounded(
            rect,
            self.theme.corner_radius * 0.6,
            self.theme.surface_active,
        );
        self.draw_text_at(rect.min() + Vec2::splat(pad), &layout, self.theme.text);
        self.painter.pop_clip();
        self.painter.set_layer(prev);
        rect
    }

    /// Centred modal card `width` px wide over a dimmed backdrop. Blocks input
    /// to everything beneath it.
    pub fn modal<R>(
        &mut self,
        key: impl Hash,
        title: &str,
        width: f32,
        f: impl FnOnce(&mut Ui) -> R,
    ) -> ModalResponse<R> {
        let id = self.make_id(("modal", key));
        let screen = self.screen_rect();
        let prev_layer = self.painter.set_layer(Layer::Modal);
        self.painter.push_clip_absolute(screen);
        self.painter.rect_filled(screen, self.theme.backdrop);
        self.add_area(Layer::Modal, screen);

        let pad = self.theme.padding * 1.5;
        let est_h = self
            .memory
            .get(&id.with("h"))
            .copied()
            .unwrap_or(screen.h * 0.4);
        let w = width.min(screen.w - 2.0 * self.theme.padding);
        let card = Rect::from_center(
            screen.center(),
            Vec2::new(w, est_h.min(screen.h - 2.0 * self.theme.padding)),
        );
        self.painter.rect_rounded(
            card,
            self.theme.corner_radius * 1.5,
            self.theme.panel_bg.lerp(self.theme.surface, 0.5),
        );
        self.push_id(id.0);
        let (inner, used) = self.region(card.shrink(pad), Dir::Vertical, |ui| {
            if !title.is_empty() {
                ui.heading(title);
                ui.add_space(ui.theme.spacing);
            }
            f(ui)
        });
        self.pop_id();
        self.memory.insert(id.with("h"), used.y + pad * 2.0);

        let mut dismissed = self.take_back();
        if let Some(p) = self.any_just_pressed() {
            if !card.contains(p) {
                dismissed = true;
            }
        }
        self.painter.pop_clip();
        self.painter.set_layer(prev_layer);
        ModalResponse { inner, dismissed }
    }
}

/// Places a `size` bubble centred above `anchor`, flipping below when there's
/// no room and clamping horizontally to the panel.
pub fn tooltip_rect(anchor: Rect, size: Vec2, panel: Vec2, gap: f32) -> Rect {
    let x = (anchor.center().x - size.x * 0.5).clamp(0.0, (panel.x - size.x).max(0.0));
    let above = anchor.y - gap - size.y;
    let y = if above >= 0.0 {
        above
    } else {
        (anchor.bottom() + gap).min((panel.y - size.y).max(0.0))
    };
    Rect::new(x, y, size.x, size.y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tooltip_placement() {
        let panel = Vec2::new(1000.0, 600.0);
        let r = tooltip_rect(
            Rect::new(500.0, 300.0, 20.0, 20.0),
            Vec2::new(100.0, 40.0),
            panel,
            8.0,
        );
        assert_eq!(r, Rect::new(460.0, 252.0, 100.0, 40.0));
        let edge = tooltip_rect(
            Rect::new(990.0, 5.0, 10.0, 10.0),
            Vec2::new(100.0, 40.0),
            panel,
            8.0,
        );
        assert_eq!(edge.x, 900.0);
        assert_eq!(edge.y, 23.0, "flipped below");
    }
}
