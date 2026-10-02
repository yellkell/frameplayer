//! Simple vector icons built from triangles, quads, lines and arcs, so the UI
//! needs no icon font or image assets.

use crate::geom::{Rect, Vec2};
use crate::painter::Painter;
use crate::theme::Color;
use std::f32::consts::{FRAC_PI_2, PI, TAU};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Icon {
    Play,
    Pause,
    Stop,
    SeekBack,
    SeekForward,
    SkipBack,
    SkipForward,
    Settings,
    Back,
    Close,
    Folder,
    Star,
    StarOutline,
    Search,
    Recenter,
    Subtitles,
    Audio,
    Loop,
    Adjust,
    Check,
    ChevronLeft,
    ChevronRight,
    ChevronUp,
    ChevronDown,
    Plus,
    Minus,
    Menu,
    Grid,
    Network,
    Haptics,
    Eye,
    Keyboard,
    Refresh,
    Download,
    Backspace,
    Shift,
    Enter,
}

impl Icon {
    pub const ALL: [Icon; 37] = [
        Icon::Play,
        Icon::Pause,
        Icon::Stop,
        Icon::SeekBack,
        Icon::SeekForward,
        Icon::SkipBack,
        Icon::SkipForward,
        Icon::Settings,
        Icon::Back,
        Icon::Close,
        Icon::Folder,
        Icon::Star,
        Icon::StarOutline,
        Icon::Search,
        Icon::Recenter,
        Icon::Subtitles,
        Icon::Audio,
        Icon::Loop,
        Icon::Adjust,
        Icon::Check,
        Icon::ChevronLeft,
        Icon::ChevronRight,
        Icon::ChevronUp,
        Icon::ChevronDown,
        Icon::Plus,
        Icon::Minus,
        Icon::Menu,
        Icon::Grid,
        Icon::Network,
        Icon::Haptics,
        Icon::Eye,
        Icon::Keyboard,
        Icon::Refresh,
        Icon::Download,
        Icon::Backspace,
        Icon::Shift,
        Icon::Enter,
    ];
}

fn star_points(c: Vec2, outer: f32, inner: f32) -> Vec<Vec2> {
    (0..10)
        .map(|i| {
            let a = -FRAC_PI_2 + PI * i as f32 / 5.0;
            let r = if i % 2 == 0 { outer } else { inner };
            c + Vec2::new(a.cos(), a.sin()) * r
        })
        .collect()
}

/// Draws `icon` centred in `rect` (uses the shorter side).
pub fn draw_icon(p: &mut Painter, icon: Icon, rect: Rect, color: Color) {
    let s = rect.w.min(rect.h);
    let c = rect.center();
    // Unit helper: coordinates in -1..1 icon space.
    let u = |x: f32, y: f32| c + Vec2::new(x, y) * (s * 0.5);
    let stroke = (s * 0.1).max(1.5);
    match icon {
        Icon::Play => p.triangle(u(-0.55, -0.7), u(0.75, 0.0), u(-0.55, 0.7), color),
        Icon::Pause => {
            p.rect_filled(Rect::from_min_max(u(-0.6, -0.7), u(-0.15, 0.7)), color);
            p.rect_filled(Rect::from_min_max(u(0.15, -0.7), u(0.6, 0.7)), color);
        }
        Icon::Stop => p.rect_filled(Rect::from_min_max(u(-0.6, -0.6), u(0.6, 0.6)), color),
        Icon::SeekBack | Icon::SeekForward => {
            let f = if icon == Icon::SeekForward { 1.0 } else { -1.0 };
            p.triangle(u(-0.8 * f, -0.6), u(0.0, 0.0), u(-0.8 * f, 0.6), color);
            p.triangle(u(0.0, -0.6), u(0.8 * f, 0.0), u(0.0, 0.6), color);
        }
        Icon::SkipBack | Icon::SkipForward => {
            let f = if icon == Icon::SkipForward { 1.0 } else { -1.0 };
            p.triangle(
                u(-0.6 * f, -0.65),
                u(0.4 * f, 0.0),
                u(-0.6 * f, 0.65),
                color,
            );
            let bar = [u(0.45 * f, -0.65), u(0.7 * f, 0.65)];
            p.rect_filled(
                Rect::from_min_max(bar[0].min(bar[1]), bar[0].max(bar[1])),
                color,
            );
        }
        Icon::Settings => {
            // Gear: ring plus 8 teeth.
            for i in 0..8 {
                let a = TAU * i as f32 / 8.0;
                let d = Vec2::new(a.cos(), a.sin());
                p.line(c + d * s * 0.22, c + d * s * 0.46, s * 0.16, color);
            }
            p.arc(c, s * 0.27, s * 0.17, 0.0, TAU, color);
        }
        Icon::Back | Icon::ChevronLeft => {
            p.polyline(
                &[u(0.3, -0.6), u(-0.3, 0.0), u(0.3, 0.6)],
                stroke * 1.2,
                color,
            );
            if icon == Icon::Back {
                p.line(u(-0.3, 0.0), u(0.7, 0.0), stroke * 1.2, color);
            }
        }
        Icon::ChevronRight => p.polyline(
            &[u(-0.3, -0.6), u(0.3, 0.0), u(-0.3, 0.6)],
            stroke * 1.2,
            color,
        ),
        Icon::ChevronUp => p.polyline(
            &[u(-0.6, 0.3), u(0.0, -0.3), u(0.6, 0.3)],
            stroke * 1.2,
            color,
        ),
        Icon::ChevronDown => p.polyline(
            &[u(-0.6, -0.3), u(0.0, 0.3), u(0.6, -0.3)],
            stroke * 1.2,
            color,
        ),
        Icon::Close => {
            p.line(u(-0.55, -0.55), u(0.55, 0.55), stroke * 1.2, color);
            p.line(u(0.55, -0.55), u(-0.55, 0.55), stroke * 1.2, color);
        }
        Icon::Folder => {
            p.convex_polygon(
                &[
                    u(-0.8, -0.6),
                    u(-0.25, -0.6),
                    u(-0.1, -0.42),
                    u(-0.1, -0.3),
                    u(-0.8, -0.3),
                ],
                color,
            );
            p.rect_rounded(
                Rect::from_min_max(u(-0.8, -0.4), u(0.8, 0.6)),
                s * 0.05,
                color,
            );
        }
        Icon::Star => {
            let pts = star_points(c, s * 0.48, s * 0.2);
            // Concave star: fan from centre.
            for i in 0..10 {
                p.triangle(c, pts[i], pts[(i + 1) % 10], color);
            }
        }
        Icon::StarOutline => {
            let mut pts = star_points(c, s * 0.48, s * 0.2);
            pts.push(pts[0]);
            p.polyline(&pts, stroke * 0.8, color);
        }
        Icon::Search => {
            p.arc(u(-0.15, -0.15), s * 0.27, stroke, 0.0, TAU, color);
            p.line(u(0.2, 0.2), u(0.65, 0.65), stroke * 1.4, color);
        }
        Icon::Recenter => {
            p.arc(c, s * 0.3, stroke, 0.0, TAU, color);
            p.circle(c, s * 0.1, color);
            for (a, b) in [
                ((0.0, -0.85), (0.0, -0.5)),
                ((0.0, 0.5), (0.0, 0.85)),
                ((-0.85, 0.0), (-0.5, 0.0)),
                ((0.5, 0.0), (0.85, 0.0)),
            ] {
                p.line(u(a.0, a.1), u(b.0, b.1), stroke, color);
            }
        }
        Icon::Subtitles => {
            p.rect_stroke(
                Rect::from_min_max(u(-0.8, -0.55), u(0.8, 0.55)),
                s * 0.08,
                stroke * 0.8,
                color,
            );
            p.line(u(-0.5, 0.15), u(0.1, 0.15), stroke, color);
            p.line(u(0.25, 0.15), u(0.5, 0.15), stroke, color);
            p.line(u(-0.5, -0.15), u(-0.2, -0.15), stroke, color);
            p.line(u(-0.05, -0.15), u(0.5, -0.15), stroke, color);
        }
        Icon::Audio => {
            p.rect_filled(Rect::from_min_max(u(-0.75, -0.25), u(-0.4, 0.25)), color);
            p.convex_polygon(
                &[u(-0.4, -0.25), u(0.0, -0.6), u(0.0, 0.6), u(-0.4, 0.25)],
                color,
            );
            p.arc(u(0.0, 0.0), s * 0.25, stroke * 0.8, -0.8, 0.8, color);
            p.arc(u(0.0, 0.0), s * 0.42, stroke * 0.8, -0.8, 0.8, color);
        }
        Icon::Loop => {
            p.arc(c, s * 0.35, stroke, 0.3, TAU - 0.6, color);
            let tip = c + Vec2::new((0.3f32).cos(), (0.3f32).sin()) * s * 0.35;
            p.triangle(
                tip + Vec2::new(-s * 0.16, 0.0),
                tip + Vec2::new(s * 0.16, 0.0),
                tip + Vec2::new(0.0, -s * 0.18),
                color,
            );
        }
        Icon::Adjust => {
            for (i, x) in [-0.5f32, 0.0, 0.5].iter().enumerate() {
                p.line(u(*x, -0.7), u(*x, 0.7), stroke * 0.7, color);
                let ky = [0.3, -0.35, 0.1][i];
                p.rect_rounded(
                    Rect::from_center(u(*x, ky), Vec2::splat(s * 0.22)),
                    s * 0.04,
                    color,
                );
            }
        }
        Icon::Check => p.polyline(
            &[u(-0.6, 0.0), u(-0.15, 0.45), u(0.65, -0.45)],
            stroke * 1.3,
            color,
        ),
        Icon::Plus | Icon::Minus => {
            p.line(u(-0.6, 0.0), u(0.6, 0.0), stroke * 1.2, color);
            if icon == Icon::Plus {
                p.line(u(0.0, -0.6), u(0.0, 0.6), stroke * 1.2, color);
            }
        }
        Icon::Menu => {
            for y in [-0.45f32, 0.0, 0.45] {
                p.line(u(-0.65, y), u(0.65, y), stroke * 1.1, color);
            }
        }
        Icon::Grid => {
            for (x, y) in [(-1.0f32, -1.0f32), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                p.rect_rounded(
                    Rect::from_center(u(x * 0.35, y * 0.35), Vec2::splat(s * 0.3)),
                    s * 0.04,
                    color,
                );
            }
        }
        Icon::Network => {
            p.rect_filled(
                Rect::from_center(u(0.0, -0.5), Vec2::new(s * 0.35, s * 0.25)),
                color,
            );
            p.line(u(0.0, -0.4), u(0.0, 0.1), stroke, color);
            p.line(u(-0.6, 0.1), u(0.6, 0.1), stroke, color);
            for x in [-0.6f32, 0.6] {
                p.line(u(x, 0.1), u(x, 0.35), stroke, color);
                p.rect_filled(
                    Rect::from_center(u(x, 0.55), Vec2::new(s * 0.3, s * 0.22)),
                    color,
                );
            }
        }
        Icon::Haptics => {
            let pts: Vec<Vec2> = (0..=16)
                .map(|i| {
                    let t = i as f32 / 16.0;
                    u(-0.8 + 1.6 * t, (t * TAU * 2.0).sin() * 0.4)
                })
                .collect();
            p.polyline(&pts, stroke, color);
        }
        Icon::Eye => {
            p.arc(u(0.0, 0.55), s * 0.6, stroke, -PI * 0.78, -PI * 0.22, color);
            p.arc(u(0.0, -0.55), s * 0.6, stroke, PI * 0.22, PI * 0.78, color);
            p.circle(c, s * 0.16, color);
        }
        Icon::Keyboard => {
            p.rect_stroke(
                Rect::from_min_max(u(-0.85, -0.5), u(0.85, 0.5)),
                s * 0.06,
                stroke * 0.7,
                color,
            );
            for row in 0..2 {
                for col in 0..5 {
                    let x = -0.6 + col as f32 * 0.3;
                    let y = -0.22 + row as f32 * 0.25;
                    p.rect_filled(Rect::from_center(u(x, y), Vec2::splat(s * 0.09)), color);
                }
            }
            p.rect_filled(Rect::from_min_max(u(-0.4, 0.22), u(0.4, 0.32)), color);
        }
        Icon::Refresh => {
            p.arc(c, s * 0.35, stroke, -PI * 0.35, PI * 1.45, color);
            let a = -PI * 0.35;
            let tip = c + Vec2::new(a.cos(), a.sin()) * s * 0.35;
            p.triangle(
                tip + Vec2::new(-s * 0.05, -s * 0.2),
                tip + Vec2::new(s * 0.2, s * 0.05),
                tip + Vec2::new(-s * 0.15, s * 0.12),
                color,
            );
        }
        Icon::Download => {
            p.line(u(0.0, -0.75), u(0.0, 0.2), stroke * 1.2, color);
            p.triangle(u(-0.4, 0.0), u(0.4, 0.0), u(0.0, 0.45), color);
            p.line(u(-0.7, 0.7), u(0.7, 0.7), stroke * 1.2, color);
        }
        Icon::Backspace => {
            p.convex_polygon(
                &[
                    u(-0.85, 0.0),
                    u(-0.4, -0.55),
                    u(0.85, -0.55),
                    u(0.85, 0.55),
                    u(-0.4, 0.55),
                ],
                color,
            );
            let bg = Color::BLACK.alpha(0.8);
            p.line(u(-0.1, -0.25), u(0.45, 0.25), stroke, bg);
            p.line(u(0.45, -0.25), u(-0.1, 0.25), stroke, bg);
        }
        Icon::Shift => {
            p.convex_polygon(&[u(0.0, -0.75), u(0.7, 0.05), u(-0.7, 0.05)], color);
            p.rect_filled(Rect::from_min_max(u(-0.3, 0.0), u(0.3, 0.65)), color);
        }
        Icon::Enter => {
            p.polyline(
                &[u(0.6, -0.6), u(0.6, 0.2), u(-0.4, 0.2)],
                stroke * 1.2,
                color,
            );
            p.triangle(u(-0.75, 0.2), u(-0.3, -0.15), u(-0.3, 0.55), color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fp_core::draw::DrawList;

    #[test]
    fn every_icon_draws_valid_geometry_inside_its_rect() {
        let rect = Rect::new(10.0, 10.0, 40.0, 40.0);
        for icon in Icon::ALL {
            let mut p = Painter::new(Vec2::new(100.0, 100.0));
            draw_icon(&mut p, icon, rect, Color::WHITE);
            let mut out = DrawList::default();
            p.finish_into(&mut out, 1.0);
            crate::painter::validate(&out).unwrap();
            assert!(!out.indices.is_empty(), "{icon:?} drew nothing");
            let bounds = rect.expand(4.0);
            for v in &out.vertices {
                assert!(
                    bounds.contains(Vec2::from(v.pos)),
                    "{icon:?} vertex {:?} escapes",
                    v.pos
                );
            }
        }
    }
}
