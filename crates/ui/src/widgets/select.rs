//! Dropdown selects, segmented pickers and tab bars.

use crate::geom::{Rect, Vec2};
use crate::icons::{draw_icon, Icon};
use crate::layout::Dir;
use crate::painter::Layer;
use crate::text::Align;
use crate::ui::{Response, Sense, Ui};
use crate::widgets::basic::ButtonStyle;
use std::hash::Hash;

/// Where a dropdown's option list goes: below the header if it fits,
/// otherwise above, clamped to the panel.
pub fn popup_rect(header: Rect, list_h: f32, panel_h: f32, gap: f32) -> Rect {
    let below = header.bottom() + gap;
    let y = if below + list_h <= panel_h || header.y - gap - list_h < 0.0 {
        below.min((panel_h - list_h).max(0.0))
    } else {
        header.y - gap - list_h
    };
    Rect::new(header.x, y, header.w, list_h.min(panel_h))
}

impl Ui {
    /// Labelled dropdown row. Returns the newly selected index when it changes.
    pub fn dropdown<S: AsRef<str>>(
        &mut self,
        key: impl Hash,
        label: &str,
        selected: usize,
        options: &[S],
    ) -> Option<usize> {
        let t = self.theme.clone();
        let row = self.allocate_row(t.widget_height);
        let id = self.make_id(("dropdown", key));
        let (label_rect, header) = if label.is_empty() {
            (Rect::ZERO, row)
        } else {
            row.split_left(row.w * 0.4)
        };
        if !label.is_empty() {
            self.draw_text_in(label_rect, label, t.text_size, t.text, Align::Left);
        }
        let resp = self.interact(header, id, Sense::CLICK);
        self.record(id, header, || {
            if label.is_empty() {
                options
                    .get(selected)
                    .map(|s| s.as_ref().to_string())
                    .unwrap_or_default()
            } else {
                label.to_string()
            }
        });
        let open = self.open_popup == Some(id);
        if resp.clicked {
            self.open_popup = if open { None } else { Some(id) };
        }
        self.painter
            .rect_rounded(header, t.corner_radius, self.surface_color(&resp));
        let current = options.get(selected).map(|s| s.as_ref()).unwrap_or("—");
        let icon_s = t.icon_size * 0.7;
        let text_rect = Rect::new(
            header.x + t.padding,
            header.y,
            header.w - t.padding * 2.0 - icon_s,
            header.h,
        );
        self.draw_text_in(text_rect, current, t.text_size, t.text, Align::Left);
        let chevron = if open {
            Icon::ChevronUp
        } else {
            Icon::ChevronDown
        };
        draw_icon(
            &mut self.painter,
            chevron,
            Rect::new(
                header.right() - t.padding - icon_s,
                header.center().y - icon_s * 0.5,
                icon_s,
                icon_s,
            ),
            t.text_dim,
        );
        self.focus_ring(&resp, t.corner_radius);

        if self.open_popup != Some(id) {
            return None;
        }
        // Option list on the popup layer.
        let item_h = t.widget_height * 0.9;
        let list_h = item_h * options.len() as f32 + t.spacing;
        let popup = popup_rect(header, list_h, self.size.y, t.spacing * 0.5);
        let prev = self.painter.set_layer(Layer::Popup);
        self.painter.push_clip_absolute(popup.expand(2.0));
        self.add_area(Layer::Popup, popup);
        self.painter.rect_rounded(popup, t.corner_radius, t.surface);
        self.painter
            .rect_stroke(popup, t.corner_radius, 1.5, t.border);
        let mut picked = None;
        self.push_id(id.0);
        for (i, opt) in options.iter().enumerate() {
            let r = Rect::new(
                popup.x + t.spacing * 0.5,
                popup.y + t.spacing * 0.5 + i as f32 * item_h,
                popup.w - t.spacing,
                item_h,
            );
            let oid = self.make_id(("opt", i));
            let style = ButtonStyle {
                selected: i == selected,
                ..Default::default()
            };
            if self.option_row(r, oid, opt.as_ref(), style).clicked {
                picked = Some(i);
            }
        }
        self.pop_id();
        self.painter.pop_clip();
        self.painter.set_layer(prev);

        let pressed_outside = self
            .any_just_pressed()
            .is_some_and(|p| !popup.contains(p) && !header.contains(p));
        if picked.is_some() || pressed_outside || self.take_back() {
            self.open_popup = None;
        }
        picked.filter(|&i| i != selected)
    }

    fn option_row(
        &mut self,
        rect: Rect,
        id: crate::ui::Id,
        text: &str,
        style: ButtonStyle,
    ) -> Response {
        let t = self.theme.clone();
        let resp = self.interact(rect, id, Sense::CLICK);
        self.record(id, rect, || text.to_string());
        if style.selected {
            self.painter
                .rect_rounded(rect, t.corner_radius * 0.7, t.accent.alpha(0.35));
        } else if resp.highlighted() {
            self.painter
                .rect_rounded(rect, t.corner_radius * 0.7, t.surface_hover);
        }
        let check = t.icon_size * 0.6;
        if style.selected {
            draw_icon(
                &mut self.painter,
                Icon::Check,
                Rect::new(
                    rect.x + t.padding * 0.5,
                    rect.center().y - check * 0.5,
                    check,
                    check,
                ),
                t.text,
            );
        }
        let tr = Rect::new(
            rect.x + t.padding + check,
            rect.y,
            rect.w - t.padding * 2.0 - check,
            rect.h,
        );
        self.draw_text_in(tr, text, t.text_size, t.text, Align::Left);
        self.focus_ring(&resp, t.corner_radius * 0.7);
        resp
    }

    /// Row of equal-width segments; returns true when `selected` changed.
    pub fn segmented<S: AsRef<str>>(
        &mut self,
        key: impl Hash,
        selected: &mut usize,
        options: &[S],
    ) -> bool {
        let h = self.theme.widget_height;
        let row = self.allocate_row(h);
        let id = self.make_id(("segmented", key));
        let mut changed = false;
        self.push_id(id.0);
        let cells = crate::layout::split_even(row.w, options.len(), self.theme.spacing * 0.5);
        for (i, ((x, w), opt)) in cells.into_iter().zip(options).enumerate() {
            let r = Rect::new(row.x + x, row.y, w, h);
            let bid = self.make_id(i);
            let style = ButtonStyle {
                selected: i == *selected,
                ..Default::default()
            };
            if self.button_at(r, bid, opt.as_ref(), style).clicked && i != *selected {
                *selected = i;
                changed = true;
            }
        }
        self.pop_id();
        changed
    }

    /// Tab bar with an underline on the selected tab.
    pub fn tabs<S: AsRef<str>>(
        &mut self,
        key: impl Hash,
        selected: &mut usize,
        labels: &[S],
    ) -> bool {
        let t = self.theme.clone();
        let row = self.allocate_row(t.widget_height);
        let id = self.make_id(("tabs", key));
        let mut changed = false;
        self.push_id(id.0);
        let widths: Vec<f32> = labels
            .iter()
            .map(|l| self.fonts.measure(l.as_ref(), t.text_size) + t.padding * 2.0)
            .collect();
        let ((), _) = self.region(row, Dir::Horizontal, |ui| {
            for (i, l) in labels.iter().enumerate() {
                let r = ui.allocate(Vec2::new(widths[i], t.widget_height));
                let tid = ui.make_id(i);
                let resp = ui.interact(r, tid, Sense::CLICK);
                ui.record(tid, r, || l.as_ref().to_string());
                if resp.clicked && *selected != i {
                    *selected = i;
                    changed = true;
                }
                if resp.highlighted() {
                    ui.painter
                        .rect_rounded(r, t.corner_radius, t.surface_hover.alpha(0.6));
                }
                let color = if i == *selected { t.text } else { t.text_dim };
                ui.draw_text_in(r, l.as_ref(), t.text_size, color, Align::Center);
                if i == *selected {
                    ui.painter.rect_rounded(
                        Rect::new(
                            r.x + t.padding * 0.5,
                            r.bottom() - 4.0,
                            r.w - t.padding,
                            4.0,
                        ),
                        2.0,
                        t.accent,
                    );
                }
                ui.focus_ring(&resp, t.corner_radius);
            }
        });
        self.pop_id();
        let line = Rect::new(row.x, row.bottom() - 1.0, row.w, 1.0);
        self.painter.rect_filled(line, t.border.alpha(0.5));
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn popup_flips_above_when_no_room() {
        let header = Rect::new(10.0, 100.0, 200.0, 50.0);
        assert_eq!(popup_rect(header, 200.0, 1000.0, 5.0).y, 155.0);
        let low = Rect::new(10.0, 900.0, 200.0, 50.0);
        assert_eq!(popup_rect(low, 200.0, 1000.0, 5.0).y, 695.0);
        // Too tall for either side: clamp inside the panel.
        let r = popup_rect(Rect::new(0.0, 400.0, 100.0, 50.0), 900.0, 1000.0, 5.0);
        assert!(r.y >= 0.0 && r.bottom() <= 1000.0);
    }
}
