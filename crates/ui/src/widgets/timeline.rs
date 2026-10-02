//! Video timeline scrubber: played/buffered ranges, chapter marks, A-B loop
//! markers, a funscript heat strip and a thumbnail preview popup.

use crate::geom::{Rect, Vec2};
use crate::painter::Layer;
use crate::text::{Align, TextParams};
use crate::theme::Color;
use crate::ui::{Response, Sense, Ui};
use fp_core::draw::TextureId;
use fp_core::media::Chapter;
use fp_core::MediaTime;
use std::hash::Hash;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TimelineView<'a> {
    pub position: MediaTime,
    pub duration: MediaTime,
    pub buffered: &'a [(MediaTime, MediaTime)],
    pub chapters: &'a [Chapter],
    pub loop_a: Option<MediaTime>,
    pub loop_b: Option<MediaTime>,
    /// Funscript intensity (0..1) in equal time buckets across the duration.
    pub heat: Option<&'a [f32]>,
    /// Preview image for the hovered time (from the app's sprite cache).
    pub preview: Option<(TextureId, [f32; 4])>,
    /// Preview width / height.
    pub preview_aspect: f32,
    /// D-pad seek step.
    pub step: MediaTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TimelineResponse {
    pub response: Response,
    /// Time under the pointer (request a preview frame for it).
    pub hover_time: Option<MediaTime>,
    /// Live position while dragging.
    pub scrub: Option<MediaTime>,
    /// Committed seek (drag released, or D-pad step).
    pub seek: Option<MediaTime>,
    pub preview_rect: Option<Rect>,
}

pub fn time_from_x(x: f32, track: Rect, duration: MediaTime) -> MediaTime {
    let t = if track.w > 0.0 {
        ((x - track.x) / track.w).clamp(0.0, 1.0)
    } else {
        0.0
    };
    MediaTime((duration.0.max(0) as f64 * t as f64).round() as i64)
}

pub fn x_from_time(t: MediaTime, track: Rect, duration: MediaTime) -> f32 {
    if duration.0 <= 0 {
        return track.x;
    }
    track.x + track.w * (t.0 as f64 / duration.0 as f64).clamp(0.0, 1.0) as f32
}

/// Chapter containing `t` (chapters sorted by start).
pub fn chapter_at(chapters: &[Chapter], t: MediaTime) -> Option<&Chapter> {
    chapters.iter().rev().find(|c| c.start <= t)
}

/// Blue → cyan → green → yellow → red ramp for script intensity.
pub fn heat_color(v: f32) -> Color {
    let stops = [0x2b4c9bu32, 0x23a6d5, 0x3ccf6e, 0xf5d142, 0xe5484d];
    let v = v.clamp(0.0, 1.0) * (stops.len() - 1) as f32;
    let i = (v.floor() as usize).min(stops.len() - 2);
    Color::hex(stops[i]).lerp(Color::hex(stops[i + 1]), v - i as f32)
}

/// Preview popup centred over `x`, above the track, clamped to the panel.
pub fn preview_popup_rect(track: Rect, x: f32, size: Vec2, panel_w: f32, gap: f32) -> Rect {
    let px = (x - size.x * 0.5).clamp(0.0, (panel_w - size.x).max(0.0));
    Rect::new(px, (track.y - gap - size.y).max(0.0), size.x, size.y)
}

/// "H:MM:SS" / "M:SS" without milliseconds.
pub fn format_time(t: MediaTime) -> String {
    let neg = t.0 < 0;
    let s = t.0.unsigned_abs() / 1_000_000;
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    let sign = if neg { "-" } else { "" };
    if h > 0 {
        format!("{sign}{h}:{m:02}:{s:02}")
    } else {
        format!("{sign}{m}:{s:02}")
    }
}

impl Ui {
    /// Full-width timeline. Seeks are committed on release.
    pub fn timeline(&mut self, key: impl Hash, view: &TimelineView) -> TimelineResponse {
        let t = self.theme.clone();
        let heat_h = if view.heat.is_some() {
            t.track_thickness * 1.5
        } else {
            0.0
        };
        let rect = self.allocate_row(t.widget_height + heat_h);
        let id = self.make_id(("timeline", key));
        let knob_r = t.widget_height * 0.2;
        let track_y = rect.y + (t.widget_height - t.track_thickness) * 0.5;
        let track = Rect::new(
            rect.x + knob_r,
            track_y,
            (rect.w - knob_r * 2.0).max(1.0),
            t.track_thickness,
        );
        let mut out = TimelineResponse {
            response: self.interact(rect, id, Sense::DRAG),
            ..Default::default()
        };
        self.record(id, track, || "timeline".into());
        let resp = out.response;
        let dur = view.duration;
        let scrub_key = id.with("scrub");

        if resp.pressed {
            if let Some(p) = resp.pointer {
                let tm = time_from_x(p.x, track, dur);
                out.scrub = Some(tm);
                self.memory.insert(scrub_key, tm.as_secs_f64() as f32);
            }
        } else if let Some(secs) = self.memory.remove(&scrub_key) {
            out.seek = Some(MediaTime::from_secs_f64(secs as f64));
        }
        let dir = self.take_nav_horizontal(&resp);
        if dir != 0 {
            let step = if view.step.0 > 0 {
                view.step
            } else {
                MediaTime::from_millis(10_000)
            };
            let base = out.seek.unwrap_or(view.position);
            out.seek = Some(MediaTime(base.0 + dir as i64 * step.0).clamp_to(MediaTime::ZERO, dur));
        }
        if resp.hovered && !resp.pressed {
            out.hover_time = resp.pointer.map(|p| time_from_x(p.x, track, dur));
        }
        let shown = out.scrub.unwrap_or(view.position);

        // Track layers: base, buffered, loop region, played.
        let r = track.h * 0.5;
        self.painter.rect_rounded(track, r, t.track);
        for &(a, b) in view.buffered {
            let (xa, xb) = (x_from_time(a, track, dur), x_from_time(b, track, dur));
            self.painter.rect_rounded(
                Rect::new(xa, track.y, (xb - xa).max(0.0), track.h),
                r,
                t.buffered,
            );
        }
        if let (Some(a), Some(b)) = (view.loop_a, view.loop_b) {
            let (xa, xb) = (x_from_time(a, track, dur), x_from_time(b, track, dur));
            self.painter.rect_filled(
                Rect::new(xa, track.y - 4.0, (xb - xa).max(0.0), track.h + 8.0),
                t.warning.alpha(0.25),
            );
        }
        let xp = x_from_time(shown, track, dur);
        self.painter.rect_rounded(
            Rect::new(track.x, track.y, xp - track.x, track.h),
            r,
            t.accent,
        );
        // Chapter marks: small gaps in the track.
        for ch in view.chapters.iter().filter(|c| c.start.0 > 0) {
            let x = x_from_time(ch.start, track, dur);
            self.painter.rect_filled(
                Rect::new(x - 1.5, track.y - 2.0, 3.0, track.h + 4.0),
                t.panel_bg,
            );
        }
        // A/B markers: downward triangles above the track.
        for (m, label) in [(view.loop_a, "A"), (view.loop_b, "B")] {
            if let Some(m) = m {
                let x = x_from_time(m, track, dur);
                let s = t.track_thickness * 1.4;
                let top = track.y - s * 1.6;
                self.painter.triangle(
                    Vec2::new(x - s * 0.6, top),
                    Vec2::new(x + s * 0.6, top),
                    Vec2::new(x, track.y),
                    t.warning,
                );
                let lr = Rect::new(x - s, top - t.small_text_size, s * 2.0, t.small_text_size);
                self.draw_text_in(lr, label, t.small_text_size * 0.8, t.warning, Align::Center);
            }
        }
        // Heat strip under the track.
        if let Some(heat) = view.heat.filter(|h| !h.is_empty()) {
            let strip = Rect::new(track.x, rect.bottom() - heat_h, track.w, heat_h * 0.7);
            let n = heat.len();
            let bw = strip.w / n as f32;
            // Merge equal-colour neighbours cheaply by drawing one quad per bucket.
            for (i, v) in heat.iter().enumerate() {
                self.painter.rect_filled(
                    Rect::new(strip.x + i as f32 * bw, strip.y, bw + 0.5, strip.h),
                    heat_color(*v),
                );
            }
        }
        // Knob.
        let kr = if resp.pressed || resp.highlighted() {
            knob_r * 1.2
        } else {
            knob_r
        };
        if resp.armed {
            self.painter.circle(
                Vec2::new(xp, track.center().y),
                kr * 1.6,
                t.accent.alpha(0.3),
            );
        }
        self.painter
            .circle(Vec2::new(xp, track.center().y), kr, t.text);
        self.focus_ring(&resp, t.corner_radius);

        // Hover / scrub popup: preview image, time and chapter title.
        if let Some(tm) = out.scrub.or(out.hover_time) {
            let x = x_from_time(tm, track, dur);
            let pw = (self.size.x * 0.18).max(160.0);
            let ph = if view.preview.is_some() {
                pw / view.preview_aspect.max(0.1)
            } else {
                0.0
            };
            let label_h = t.small_text_size * 1.6;
            let size = Vec2::new(
                pw,
                ph + label_h
                    * if chapter_at(view.chapters, tm).is_some() {
                        2.0
                    } else {
                        1.0
                    },
            );
            let popup = preview_popup_rect(track, x, size, self.size.x, t.spacing + knob_r);
            let prev = self.painter.set_layer(Layer::Tooltip);
            self.painter.push_clip_absolute(self.screen_rect());
            self.painter
                .rect_rounded(popup.expand(4.0), t.corner_radius, t.surface_active);
            if let Some((tex, uv)) = view.preview {
                self.painter
                    .image(Rect::new(popup.x, popup.y, pw, ph), tex, uv, Color::WHITE);
            }
            let mut y = popup.y + ph;
            if let Some(ch) = chapter_at(view.chapters, tm) {
                let l = self.fonts.layout(
                    &ch.title,
                    TextParams::new(t.small_text_size)
                        .width(pw)
                        .ellipsis()
                        .align(Align::Center),
                );
                self.draw_text_at(
                    Vec2::new(popup.x, y + (label_h - l.size.y) * 0.5),
                    &l,
                    t.text_dim,
                );
                y += label_h;
            }
            self.draw_text_in(
                Rect::new(popup.x, y, pw, label_h),
                &format_time(tm),
                t.small_text_size,
                t.text,
                Align::Center,
            );
            self.painter.pop_clip();
            self.painter.set_layer(prev);
            out.preview_rect = Some(popup);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_mapping_roundtrip() {
        let track = Rect::new(50.0, 0.0, 1000.0, 8.0);
        let d = MediaTime::from_millis(100_000);
        assert_eq!(time_from_x(550.0, track, d), MediaTime::from_millis(50_000));
        assert_eq!(time_from_x(0.0, track, d), MediaTime::ZERO);
        assert_eq!(time_from_x(5000.0, track, d), d);
        assert_eq!(x_from_time(MediaTime::from_millis(25_000), track, d), 300.0);
        assert_eq!(
            x_from_time(MediaTime::from_millis(1), track, MediaTime::ZERO),
            50.0
        );
    }

    #[test]
    fn chapters_and_formatting() {
        let ch = vec![
            Chapter {
                start: MediaTime::ZERO,
                title: "Intro".into(),
            },
            Chapter {
                start: MediaTime::from_millis(60_000),
                title: "Main".into(),
            },
        ];
        assert_eq!(
            chapter_at(&ch, MediaTime::from_millis(30_000))
                .unwrap()
                .title,
            "Intro"
        );
        assert_eq!(
            chapter_at(&ch, MediaTime::from_millis(90_000))
                .unwrap()
                .title,
            "Main"
        );
        assert!(chapter_at(&ch[1..], MediaTime::ZERO).is_none());
        assert_eq!(format_time(MediaTime::from_millis(3_723_900)), "1:02:03");
        assert_eq!(format_time(MediaTime::from_millis(65_000)), "1:05");
    }

    #[test]
    fn heat_ramp_and_popup() {
        assert_eq!(heat_color(0.0), Color::hex(0x2b4c9b));
        let close = |a: Color, b: Color| a.0.iter().zip(b.0).all(|(x, y)| (x - y).abs() < 1e-5);
        assert!(close(heat_color(1.0), Color::hex(0xe5484d)));
        assert_eq!(heat_color(7.0), heat_color(1.0));
        let track = Rect::new(0.0, 500.0, 1000.0, 8.0);
        let r = preview_popup_rect(track, 10.0, Vec2::new(200.0, 120.0), 1000.0, 10.0);
        assert_eq!(r, Rect::new(0.0, 370.0, 200.0, 120.0));
        let r = preview_popup_rect(track, 990.0, Vec2::new(200.0, 120.0), 1000.0, 10.0);
        assert_eq!(r.x, 800.0);
    }
}
