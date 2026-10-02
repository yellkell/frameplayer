//! Value slider with a floating value tooltip while dragging or hovering.

use crate::geom::{Rect, Vec2};
use crate::text::Align;
use crate::ui::{Response, Sense, Ui};
use std::ops::RangeInclusive;

#[derive(Debug, Clone, PartialEq)]
pub struct SliderOpts {
    /// Snap increment; also the D-pad step. `None` → 1% of the range.
    pub step: Option<f32>,
    pub decimals: usize,
    pub suffix: String,
    /// Show the numeric value at the right end of the row.
    pub show_value: bool,
}

impl Default for SliderOpts {
    fn default() -> Self {
        SliderOpts {
            step: None,
            decimals: 2,
            suffix: String::new(),
            show_value: true,
        }
    }
}

impl SliderOpts {
    pub fn step(mut self, s: f32) -> Self {
        self.step = Some(s);
        self
    }
    pub fn decimals(mut self, d: usize) -> Self {
        self.decimals = d;
        self
    }
    pub fn suffix(mut self, s: &str) -> Self {
        self.suffix = s.to_string();
        self
    }
    pub fn format(&self, v: f32) -> String {
        format!("{:.*}{}", self.decimals, v, self.suffix)
    }
}

/// Maps a pointer x within `track` to a value in `range`, snapped to `step`.
pub fn value_from_x(x: f32, track: Rect, range: &RangeInclusive<f32>, step: Option<f32>) -> f32 {
    let (lo, hi) = (*range.start(), *range.end());
    let t = if track.w > 0.0 {
        ((x - track.x) / track.w).clamp(0.0, 1.0)
    } else {
        0.0
    };
    snap(lo + t * (hi - lo), range, step)
}

pub fn snap(v: f32, range: &RangeInclusive<f32>, step: Option<f32>) -> f32 {
    let (lo, hi) = (*range.start(), *range.end());
    let v = match step {
        Some(s) if s > 0.0 => lo + ((v - lo) / s).round() * s,
        _ => v,
    };
    v.clamp(lo.min(hi), hi.max(lo))
}

/// Normalized 0..1 position of `v` in `range`.
pub fn fraction(v: f32, range: &RangeInclusive<f32>) -> f32 {
    let (lo, hi) = (*range.start(), *range.end());
    if hi == lo {
        0.0
    } else {
        ((v - lo) / (hi - lo)).clamp(0.0, 1.0)
    }
}

impl Ui {
    /// Labelled slider row. Press anywhere on the track jumps there; drag
    /// adjusts; Left/Right step when focused.
    pub fn slider(
        &mut self,
        label: &str,
        value: &mut f32,
        range: RangeInclusive<f32>,
        opts: &SliderOpts,
    ) -> Response {
        let t = self.theme.clone();
        let rect = self.allocate_row(t.widget_height);
        let id = self.make_id(("slider", label));
        let label_w = if label.is_empty() {
            0.0
        } else {
            (rect.w * 0.32).min(self.fonts.measure(label, t.text_size) + t.padding)
        };
        let value_w = if opts.show_value {
            self.fonts
                .measure(&opts.format(*range.end()), t.small_text_size)
                .max(
                    self.fonts
                        .measure(&opts.format(*range.start()), t.small_text_size),
                )
                + t.padding
        } else {
            0.0
        };
        let (label_rect, rest) = rect.split_left(label_w);
        let (track_area, value_rect) = rest.split_right(value_w);
        let knob_r = t.widget_height * 0.22;
        let track = Rect::new(
            track_area.x + knob_r,
            track_area.center().y - t.track_thickness * 0.5,
            (track_area.w - knob_r * 2.0).max(1.0),
            t.track_thickness,
        );
        // Hit area: the full track row, not just the thin bar.
        let hit = Rect::new(track_area.x, rect.y, track_area.w, rect.h);
        let mut resp = self.interact(hit, id, Sense::DRAG);
        self.record(id, hit, || label.to_string());
        resp.rect = rect;

        let old = *value;
        if resp.pressed {
            if let Some(p) = resp.pointer {
                *value = value_from_x(p.x, track, &range, opts.step);
            }
        }
        let dir = self.take_nav_horizontal(&resp);
        if dir != 0 {
            let step = opts
                .step
                .unwrap_or((range.end() - range.start()).abs() / 100.0);
            *value = snap(*value + dir as f32 * step, &range, opts.step);
        }
        resp.changed = *value != old;

        // Drawing.
        if !label.is_empty() {
            self.draw_text_in(label_rect, label, t.text_size, t.text, Align::Left);
        }
        let f = fraction(*value, &range);
        self.painter.rect_rounded(track, track.h * 0.5, t.track);
        self.painter.rect_rounded(
            Rect::new(track.x, track.y, track.w * f, track.h),
            track.h * 0.5,
            t.accent,
        );
        let knob = Vec2::new(track.x + track.w * f, track.center().y);
        let kr = if resp.pressed || resp.highlighted() {
            knob_r * 1.15
        } else {
            knob_r
        };
        if resp.armed {
            self.painter.circle(knob, kr * 1.6, t.accent.alpha(0.3));
        }
        self.painter.circle(knob, kr, t.text);
        if opts.show_value {
            self.draw_text_in(
                value_rect,
                &opts.format(*value),
                t.small_text_size,
                t.text_dim,
                Align::Right,
            );
        }
        if resp.pressed || resp.hovered {
            let anchor = Rect::from_center(knob, Vec2::splat(kr * 2.0));
            self.tooltip(anchor, &opts.format(*value));
        }
        self.focus_ring(&resp, t.corner_radius);
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_mapping() {
        let track = Rect::new(100.0, 0.0, 200.0, 10.0);
        let r = 0.0..=10.0;
        assert_eq!(value_from_x(100.0, track, &r, None), 0.0);
        assert_eq!(value_from_x(200.0, track, &r, None), 5.0);
        assert_eq!(value_from_x(500.0, track, &r, None), 10.0);
        assert_eq!(value_from_x(-50.0, track, &r, None), 0.0);
        assert_eq!(value_from_x(163.0, track, &r, Some(0.5)), 3.0);
        assert_eq!(snap(7.3, &(-1.0..=1.0), None), 1.0);
        assert_eq!(fraction(2.5, &(0.0..=10.0)), 0.25);
        assert_eq!(fraction(1.0, &(1.0..=1.0)), 0.0);
        assert_eq!(
            SliderOpts::default().decimals(1).suffix(" EV").format(1.26),
            "1.3 EV"
        );
    }
}
