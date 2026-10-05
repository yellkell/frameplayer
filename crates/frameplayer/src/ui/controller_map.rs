//! Settings > Controller: both Frame controllers drawn face-on, every
//! remappable button glowing, a label beside each saying what it does.
//! Pointing at a button and pulling the trigger opens its choices below.
//!
//! The drawing is our own, laid out from the Frame controller models
//! (part centres measured face-on, in units of the round head's radius).

use super::theme::{self, Weight};
use super::widgets;
use super::{RemapSlot, View};
use crate::bindings::{Axis, AxisAction, Button, ButtonAction, HandBindings, LEFT, RIGHT};
use egui::{Align2, Color32, CornerRadius, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Vec2};
use std::time::Duration;

const BODY: Color32 = Color32::from_rgb(52, 60, 72);
const BODY_EDGE: Color32 = Color32::from_rgb(74, 84, 99);
const KEY: Color32 = Color32::from_rgb(30, 35, 43);
const LABEL_W: f32 = 220.0;
const GAP: f32 = 18.0;

/// A remappable place on the drawing.
#[derive(Clone, Copy)]
enum Spot {
    /// A round button: centre and radius (head radii), its letter.
    Round(f32, f32, f32, Button, &'static str),
    /// Menu / View: a small pill.
    Pill(f32, f32, Button),
    /// One arm of the D-pad (its centre).
    Arm(f32, f32, Button),
    Stick(f32, f32),
    Bumper,
}

/// Right controller; the left is its mirror image with a D-pad for A/B/X/Y.
fn spots(hand: usize) -> Vec<Spot> {
    if hand == RIGHT {
        vec![
            Spot::Bumper,
            Spot::Pill(-0.05, -0.32, Button::Menu),
            Spot::Round(0.39, -0.25, 0.13, Button::North, "Y"),
            Spot::Round(0.17, 0.01, 0.13, Button::West, "X"),
            Spot::Round(0.66, -0.03, 0.13, Button::East, "B"),
            Spot::Round(0.44, 0.23, 0.13, Button::South, "A"),
            Spot::Stick(-0.33, 0.26),
        ]
    } else {
        let (cx, cy, d) = (-0.42, -0.01, 0.235);
        vec![
            Spot::Bumper,
            Spot::Pill(0.05, -0.32, Button::Menu),
            Spot::Arm(cx, cy - d, Button::North),
            Spot::Arm(cx - d, cy, Button::West),
            Spot::Arm(cx + d, cy, Button::East),
            Spot::Arm(cx, cy + d, Button::South),
            Spot::Stick(0.33, 0.26),
        ]
    }
}

impl Spot {
    /// Which binding the spot opens.
    fn slot(self, hand: usize) -> RemapSlot {
        match self {
            Spot::Round(.., b, _) | Spot::Pill(_, _, b) | Spot::Arm(_, _, b) => {
                RemapSlot::Button(hand, b)
            }
            Spot::Stick(..) => RemapSlot::Axis(hand, Axis::StickX),
            Spot::Bumper => RemapSlot::Button(hand, Button::Shoulder),
        }
    }

    /// Whether `open` is (part of) this spot.
    fn owns(self, hand: usize, open: RemapSlot) -> bool {
        match (self, open) {
            (Spot::Stick(..), RemapSlot::Axis(h, _)) => h == hand,
            (Spot::Stick(..), RemapSlot::Button(h, Button::StickClick)) => h == hand,
            _ => self.slot(hand) == open,
        }
    }

    fn centre(self, hand: usize) -> (f32, f32) {
        match self {
            Spot::Round(x, y, ..)
            | Spot::Pill(x, y, _)
            | Spot::Arm(x, y, _)
            | Spot::Stick(x, y) => (x, y),
            Spot::Bumper => (if hand == RIGHT { 0.05 } else { -0.05 }, -1.12),
        }
    }

    /// What the label beside the spot says.
    fn lines(self, map: &HandBindings) -> Vec<String> {
        match self {
            Spot::Round(.., b, _) | Spot::Pill(_, _, b) | Spot::Arm(_, _, b) => {
                vec![map.button(b).label().to_string()]
            }
            Spot::Bumper => vec![map.button(Button::Shoulder).label().to_string()],
            Spot::Stick(..) => {
                let mut l = vec![
                    format!("←→  {}", map.axis(Axis::StickX).label()),
                    format!("↑↓  {}", map.axis(Axis::StickY).label()),
                    format!("Press  {}", map.button(Button::StickClick).label()),
                ];
                for (axis, arrows) in [(Axis::GripX, "←→"), (Axis::GripY, "↑↓")] {
                    let a = map.axis(axis);
                    if a != AxisAction::Nothing {
                        l.push(format!("Grip {arrows}  {}", short(a)));
                    }
                }
                l
            }
        }
    }
}

/// Width of a label holding `lines`.
fn label_width(ui: &egui::Ui, lines: &[String]) -> f32 {
    let font = theme::font(Weight::Medium, 15.0);
    let widest = lines
        .iter()
        .map(|s| {
            ui.painter()
                .layout_no_wrap(s.clone(), font.clone(), theme::TEXT)
                .size()
                .x
        })
        .fold(0.0, f32::max);
    widest.min(LABEL_W - 20.0) + 20.0
}

fn short(a: AxisAction) -> &'static str {
    match a {
        AxisAction::Tilt => "Tilt",
        AxisAction::Turn => "Turn",
        other => other.label(),
    }
}

/// Both controllers, then the chosen input's choices.
pub fn controller_map(ui: &mut egui::Ui, v: &mut View) {
    let avail = ui.available_width();
    let each = ((avail - 24.0) / 2.0).max(300.0);
    // The label columns' real widths decide how big the controllers can be.
    let cols = [LEFT, RIGHT].map(|hand| {
        let map = v.settings.controls.hand(hand);
        let mut w = [0.0_f32; 2];
        for spot in spots(hand) {
            let col = (spot.centre(hand).0 >= 0.0) as usize;
            w[col] = w[col].max(label_width(ui, &spot.lines(map)));
        }
        w
    });
    let r = cols
        .iter()
        .map(|w| (each - w[0] - w[1] - 2.0 * GAP) / 2.0)
        .fold(f32::MAX, f32::min)
        .clamp(54.0, 125.0);
    let h = r * 3.95 + 64.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(avail, h), Sense::hover());
    let t = ui.input(|i| i.time) as f32;
    for (i, hand) in [LEFT, RIGHT].into_iter().enumerate() {
        let x0 = rect.left() + i as f32 * (each + 24.0);
        // Centred in its half, then nudged so both label columns fit.
        let [lw, rw] = cols[i];
        let cx = (x0 + each / 2.0).clamp(x0 + lw + GAP + r, x0 + each - rw - GAP - r);
        let centre = Pos2::new(cx, rect.top() + 30.0 + r * 1.45);
        draw_hand(ui, v, hand, centre, r, t);
        ui.painter().text(
            Pos2::new(
                centre.x + if hand == RIGHT { 0.27 } else { -0.27 } * r,
                rect.bottom(),
            ),
            Align2::CENTER_BOTTOM,
            if hand == RIGHT { "Right" } else { "Left" },
            theme::font(Weight::SemiBold, 16.0),
            theme::TEXT_2,
        );
    }
    // The glow breathes; a slow repaint is enough.
    ui.ctx().request_repaint_after(Duration::from_millis(50));

    ui.add_space(8.0);
    chooser(ui, v);
}

fn draw_hand(ui: &mut egui::Ui, v: &mut View, hand: usize, c: Pos2, r: f32, t: f32) {
    let side = if hand == RIGHT { 1.0 } else { -1.0 };
    let at = |x: f32, y: f32| Pos2::new(c.x + x * r, c.y + y * r);
    let p = ui.painter().clone();

    // Body: the round head and the handle under it, outlined as one shape.
    let handle = Rect::from_min_max(at(side * 0.27 - 0.45, 0.4), at(side * 0.27 + 0.45, 2.5));
    let round = CornerRadius::same((0.45 * r) as u8);
    p.circle_filled(c, r + 2.0, BODY_EDGE);
    p.rect_filled(handle.expand(2.0), round, BODY_EDGE);
    p.rect_filled(handle, round, BODY);
    p.circle_filled(c, r, BODY);
    // A soft highlight on the head.
    let mut glow = egui::Mesh::default();
    glow.colored_vertex(at(-0.2, -0.3), Color32::from_white_alpha(18));
    for k in 0..=40 {
        let a = k as f32 / 40.0 * std::f32::consts::TAU;
        glow.colored_vertex(c + Vec2::angled(a) * r, Color32::TRANSPARENT);
    }
    for k in 1..=40 {
        glow.add_triangle(0, k, k + 1);
    }
    p.add(Shape::mesh(glow));
    // Grip button on the handle's inner side, and the system button: not
    // remappable, drawn quietly.
    let grip = Rect::from_center_size(
        at(side * 0.27 - side * 0.5, 1.15),
        Vec2::new(0.12 * r, 0.34 * r),
    );
    p.rect_filled(grip, CornerRadius::same(4), BODY_EDGE);
    p.circle_stroke(
        at(side * 0.16, 0.58),
        0.12 * r,
        Stroke::new(1.5_f32, BODY_EDGE),
    );
    if hand == LEFT {
        // The D-pad's cross, under its arm hotspots.
        let (cx, cy, d, w) = (-0.42, -0.01, 0.235, 0.21);
        p.rect_filled(
            Rect::from_center_size(at(cx, cy), Vec2::new((2.0 * d + w) * r, w * r)),
            CornerRadius::same(6),
            KEY,
        );
        p.rect_filled(
            Rect::from_center_size(at(cx, cy), Vec2::new(w * r, (2.0 * d + w) * r)),
            CornerRadius::same(6),
            KEY,
        );
    }

    let map = v.settings.controls.hand(hand).clone();
    let open = v.state.remap_open;
    let mut labels: Vec<(f32, Pos2, Vec<String>, f32)> = Vec::new();
    for (k, spot) in spots(hand).into_iter().enumerate() {
        let (sx, sy) = spot.centre(hand);
        let pos = at(sx, sy);
        let hit = match spot {
            Spot::Round(_, _, rad, ..) => {
                Rect::from_center_size(pos, Vec2::splat(2.0 * rad * r + 10.0))
            }
            Spot::Pill(..) => Rect::from_center_size(pos, Vec2::new(0.34 * r, 0.2 * r + 8.0)),
            Spot::Arm(..) => Rect::from_center_size(pos, Vec2::splat(0.25 * r)),
            Spot::Stick(..) => Rect::from_center_size(pos, Vec2::splat(0.66 * r)),
            Spot::Bumper => Rect::from_center_size(pos, Vec2::new(0.9 * r, 0.3 * r)),
        };
        let resp = ui.interact(hit, ui.id().with(("remap", hand, k)), Sense::click());
        let hover = ui
            .ctx()
            .animate_bool_with_time(resp.id, resp.hovered(), 0.12);
        let selected = open.is_some_and(|o| spot.owns(hand, o));
        let sel = ui
            .ctx()
            .animate_bool_with_time(resp.id.with("sel"), selected, 0.15);
        let pulse = 0.55 + 0.45 * (t * 2.2 + k as f32 * 0.9).sin();
        let strength = (0.5 + 0.3 * pulse + 0.4 * hover).min(1.0).max(sel);
        draw_spot(&p, spot, pos, r, strength, sel, hand);
        if resp.clicked() {
            v.state.remap_open = if selected {
                None
            } else {
                Some(spot.slot(hand))
            };
        }
        labels.push((sx, pos, spot.lines(&map), hover.max(sel)));
    }

    // Labels in a column on each side, nudged apart, with leader lines.
    for col in [-1.0_f32, 1.0] {
        let mut mine: Vec<_> = labels
            .iter()
            .filter(|l| (l.0 >= 0.0) == (col > 0.0))
            .collect();
        mine.sort_by(|a, b| a.1.y.total_cmp(&b.1.y));
        let mut next_top = f32::MIN;
        for (_, pos, lines, lit) in mine {
            let font = theme::font(Weight::Medium, 15.0);
            let galleys: Vec<_> = lines
                .iter()
                .map(|s| p.layout_no_wrap(s.clone(), font.clone(), theme::TEXT))
                .collect();
            let w = label_width(ui, lines);
            let lh = galleys.first().map(|g| g.size().y).unwrap_or(18.0);
            let hgt = lh * galleys.len() as f32 + 12.0;
            let top = (pos.y - hgt / 2.0).max(next_top);
            next_top = top + hgt + 6.0;
            let x = if col > 0.0 {
                c.x + r + GAP
            } else {
                c.x - r - GAP - w
            };
            let label = Rect::from_min_size(Pos2::new(x, top), Vec2::new(w, hgt));
            let anchor = Pos2::new(
                if col > 0.0 {
                    label.left()
                } else {
                    label.right()
                },
                label.center().y,
            );
            let line = Color32::from_white_alpha(40).lerp_to_gamma(theme::ACCENT_HOVER, *lit);
            p.line_segment([*pos, anchor], Stroke::new(1.2_f32, line));
            p.circle_filled(*pos, 2.5, line);
            p.rect_filled(label, CornerRadius::same(10), theme::SURFACE_2);
            p.rect_stroke(
                label,
                CornerRadius::same(10),
                Stroke::new(
                    1.0_f32,
                    Color32::from_white_alpha(14).lerp_to_gamma(theme::ACCENT, *lit),
                ),
                StrokeKind::Inside,
            );
            let mut y = label.top() + 6.0;
            for g in galleys {
                p.with_clip_rect(label)
                    .galley(Pos2::new(label.left() + 10.0, y), g, theme::TEXT);
                y += lh;
            }
        }
    }
}

/// One hotspot: its glow, then the key itself.
fn draw_spot(
    p: &egui::Painter,
    spot: Spot,
    pos: Pos2,
    r: f32,
    strength: f32,
    sel: f32,
    hand: usize,
) {
    let glow = |a: f32| theme::ACCENT.gamma_multiply((a * strength).clamp(0.0, 1.0));
    let key = KEY.lerp_to_gamma(theme::ACCENT, sel);
    let ink = theme::TEXT_2.lerp_to_gamma(Color32::WHITE, strength.max(sel));
    match spot {
        Spot::Round(_, _, rad, _, letter) => {
            let rr = rad * r;
            for (k, a) in [0.12, 0.22, 0.4].into_iter().enumerate() {
                p.circle_filled(pos, rr + (3 - k) as f32 * 4.5, glow(a));
            }
            p.circle_filled(pos, rr, key);
            p.text(
                pos,
                Align2::CENTER_CENTER,
                letter,
                theme::font(Weight::Bold, rr),
                ink,
            );
        }
        Spot::Pill(..) => {
            let rect = Rect::from_center_size(pos, Vec2::new(0.28 * r, 0.14 * r));
            for (k, a) in [0.12, 0.22, 0.4].into_iter().enumerate() {
                let e = (3 - k) as f32 * 4.5;
                p.rect_filled(
                    rect.expand(e),
                    CornerRadius::same((0.07 * r + e) as u8),
                    glow(a),
                );
            }
            p.rect_filled(rect, CornerRadius::same((0.07 * r) as u8), key);
            let s = 0.035 * r;
            let line = Stroke::new(1.5_f32, ink);
            if hand == RIGHT {
                // Menu: three lines.
                for k in [-1.0, 0.0, 1.0] {
                    let y = pos.y + k * s * 0.8;
                    p.line_segment(
                        [Pos2::new(pos.x - s * 1.3, y), Pos2::new(pos.x + s * 1.3, y)],
                        line,
                    );
                }
            } else {
                // View: two overlapping windows.
                for (dx, dy) in [(-0.35, -0.35), (0.35, 0.35)] {
                    let w = Rect::from_center_size(
                        pos + Vec2::new(dx * s, dy * s),
                        Vec2::new(1.8 * s, 1.4 * s),
                    );
                    p.rect_stroke(w, CornerRadius::same(1), line, StrokeKind::Inside);
                }
            }
        }
        Spot::Arm(..) => {
            let rect = Rect::from_center_size(pos, Vec2::splat(0.21 * r));
            for (k, a) in [0.15, 0.3, 0.5].into_iter().enumerate() {
                let e = (3 - k) as f32 * 3.0;
                p.rect_filled(rect.expand(e), CornerRadius::same((4.0 + e) as u8), glow(a));
            }
            p.rect_filled(rect, CornerRadius::same(4), key);
        }
        Spot::Stick(..) => {
            let rr = 0.3 * r;
            for (k, a) in [0.1, 0.2, 0.36].into_iter().enumerate() {
                p.circle_filled(pos, rr + (3 - k) as f32 * 6.0, glow(a));
            }
            p.circle_filled(pos, rr, Color32::from_rgb(24, 28, 35));
            p.circle_filled(pos, rr * 0.78, key);
            p.circle_stroke(
                pos,
                rr * 0.55,
                Stroke::new(1.5_f32, Color32::from_white_alpha(22)),
            );
        }
        Spot::Bumper => {
            // The bumper sits under the head's top edge: drawn as a band
            // just outside it.
            let c = pos + Vec2::new(0.0, 1.12 * r);
            let arc: Vec<Pos2> = (0..=24)
                .map(|k| {
                    let a = (-125.0 + 70.0 * k as f32 / 24.0).to_radians();
                    c + Vec2::angled(a) * (r + 0.1 * r)
                })
                .collect();
            p.add(Shape::line(
                arc.clone(),
                Stroke::new(0.16 * r + 8.0 * strength, glow(0.3)),
            ));
            p.add(Shape::line(arc, Stroke::new(0.12 * r, key)));
        }
    }
}

/// The open input's choices, or a hint.
fn chooser(ui: &mut egui::Ui, v: &mut View) {
    let Some(open) = v.state.remap_open else {
        ui.label(
            egui::RichText::new(
                "Point at a glowing button and pull the trigger to change what it does.",
            )
            .font(theme::font(Weight::Regular, 15.0))
            .color(theme::TEXT_3),
        );
        return;
    };
    let hand = match open {
        RemapSlot::Button(h, _) | RemapSlot::Axis(h, _) => h,
    };
    let hand_name = if hand == RIGHT { "Right" } else { "Left" };
    widgets::card(ui, |ui| {
        ui.set_width(ui.available_width());
        egui::Frame::new()
            .inner_margin(egui::Margin::same(16))
            .show(ui, |ui| {
                let stick = matches!(
                    open,
                    RemapSlot::Axis(..) | RemapSlot::Button(_, Button::StickClick)
                );
                let title = match open {
                    RemapSlot::Button(_, b) if !stick => format!("{hand_name} · {}", b.label(hand)),
                    _ => format!("{hand_name} · Thumbstick"),
                };
                ui.label(
                    egui::RichText::new(title)
                        .font(theme::font(Weight::Bold, 20.0))
                        .color(theme::TEXT),
                );
                ui.add_space(6.0);
                if stick {
                    // Which part of the stick.
                    let parts = [
                        (RemapSlot::Axis(hand, Axis::StickX), "←→  Left / right"),
                        (RemapSlot::Axis(hand, Axis::StickY), "↑↓  Up / down"),
                        (RemapSlot::Button(hand, Button::StickClick), "Press"),
                        (RemapSlot::Axis(hand, Axis::GripX), "Grip + ←→"),
                        (RemapSlot::Axis(hand, Axis::GripY), "Grip + ↑↓"),
                    ];
                    ui.horizontal_wrapped(|ui| {
                        for (slot, label) in parts {
                            if widgets::chip(ui, label, slot == open).clicked() {
                                v.state.remap_open = Some(slot);
                            }
                        }
                    });
                    ui.add_space(4.0);
                    ui.separator();
                    ui.add_space(4.0);
                }
                let map = v.settings.controls.hand_mut(hand);
                ui.horizontal_wrapped(|ui| match open {
                    RemapSlot::Button(_, b) => {
                        let current = map.button(b);
                        for a in ButtonAction::ALL {
                            if widgets::chip(ui, a.label(), a == current).clicked() {
                                *map.button_mut(b) = a;
                            }
                        }
                    }
                    RemapSlot::Axis(_, x) => {
                        let current = map.axis(x);
                        for a in AxisAction::ALL {
                            if widgets::chip(ui, a.label(), a == current).clicked() {
                                *map.axis_mut(x) = a;
                            }
                        }
                    }
                });
            });
    });
}
