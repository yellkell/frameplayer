//! Settings > Controller: both Frame controllers face-on, every remappable
//! button glowing, a label beside each saying what it does. Pointing at a
//! button and pulling the trigger opens its choices below.
//!
//! The pictures are Blender renders of Valve's controller models as SteamVR
//! ships them (assets/controllers, credited in packaging/licenses), seen
//! from 35° above face-on so the bumper on top of the head shows. Part
//! centres come from the same renders, in units of the round head's radius
//! from its centre; the tilt squashes the face's round parts vertically by
//! [`SQUASH`].

use super::theme::{self, Weight};
use super::widgets;
use super::{RemapSlot, View};
use crate::bindings::{Axis, AxisAction, Button, ButtonAction, HandBindings, LEFT, RIGHT};
use egui::{Align2, Color32, CornerRadius, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Vec2};
use std::time::Duration;

const LABEL_W: f32 = 170.0;
const GAP: f32 = 14.0;

/// A remappable place on the drawing.
#[derive(Clone, Copy)]
enum Spot {
    /// A round button: centre and radius (head radii).
    Round(f32, f32, f32, Button),
    /// Menu / View: a small pill.
    Pill(f32, f32, Button),
    /// One arm of the D-pad (its centre).
    Arm(f32, f32, Button),
    Stick(f32, f32),
    Bumper,
}

/// cos 35°: how much the tilted view squashes round parts vertically.
const SQUASH: f32 = 0.82;
/// The D-pad's centre and how far each arm's centre is from it.
const DPAD: (f32, f32, f32) = (-0.426, 0.021, 0.235);

/// The right controller has A/B/X/Y, the left a D-pad in their place.
fn spots(hand: usize) -> Vec<Spot> {
    if hand == RIGHT {
        vec![
            Spot::Bumper,
            Spot::Pill(-0.066, -0.174, Button::Menu),
            Spot::Round(0.378, -0.161, 0.132, Button::North),
            Spot::Round(0.180, 0.067, 0.132, Button::West),
            Spot::Round(0.659, 0.011, 0.132, Button::East),
            Spot::Round(0.461, 0.240, 0.132, Button::South),
            Spot::Stick(-0.289, 0.316),
        ]
    } else {
        let (cx, cy, d) = DPAD;
        vec![
            Spot::Bumper,
            Spot::Pill(0.070, -0.212, Button::Menu),
            Spot::Arm(cx, cy - d * SQUASH, Button::North),
            Spot::Arm(cx - d, cy, Button::West),
            Spot::Arm(cx + d, cy, Button::East),
            Spot::Arm(cx, cy + d * SQUASH, Button::South),
            Spot::Stick(0.304, 0.284),
        ]
    }
}

/// Where each render sits around the head, in head radii from its centre:
/// left, top, right, bottom.
fn picture_rect(hand: usize) -> [f32; 4] {
    if hand == RIGHT {
        [-1.023, -1.122, 1.026, 1.338]
    } else {
        [-1.02, -1.139, 1.03, 1.379]
    }
}

/// Where the bumper shows along the head's top edge: left, top, right,
/// bottom in head radii.
fn bumper_rect(hand: usize) -> [f32; 4] {
    if hand == RIGHT {
        [-0.456, -0.866, 0.375, -0.727]
    } else {
        [-0.354, -0.893, 0.411, -0.776]
    }
}

/// A picture of the controller, or (`glow`) the bumper's glow mask over the
/// same area; loaded into `ctx` on first use.
fn picture(ctx: &egui::Context, hand: usize, glow: bool) -> Option<egui::TextureHandle> {
    let id = egui::Id::new(("controller-picture", hand, glow));
    if let Some(t) = ctx.data(|d| d.get_temp::<egui::TextureHandle>(id)) {
        return Some(t);
    }
    let bytes: &[u8] = match (hand == RIGHT, glow) {
        (true, false) => include_bytes!("../../assets/controllers/right.png"),
        (false, false) => include_bytes!("../../assets/controllers/left.png"),
        (true, true) => include_bytes!("../../assets/controllers/right-bumper.png"),
        (false, true) => include_bytes!("../../assets/controllers/left-bumper.png"),
    };
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    let color = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
    let name = format!("controller-{hand}-{glow}");
    let t = ctx.load_texture(name, color, egui::TextureOptions::LINEAR);
    ctx.data_mut(|d| d.insert_temp(id, t.clone()));
    Some(t)
}

impl Spot {
    /// Which binding the spot opens.
    fn slot(self, hand: usize) -> RemapSlot {
        match self {
            Spot::Round(.., b) | Spot::Pill(_, _, b) | Spot::Arm(_, _, b) => {
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
            Spot::Bumper => {
                let [l, t, r, b] = bumper_rect(hand);
                ((l + r) / 2.0, (t + b) / 2.0)
            }
        }
    }

    /// Which label column the spot's label goes in: negative is left. The
    /// bumper's goes on the controller's outer side.
    fn side(self, hand: usize) -> f32 {
        match self {
            Spot::Bumper if hand == RIGHT => 1.0,
            Spot::Bumper => -1.0,
            _ => self.centre(hand).0,
        }
    }

    /// What the label beside the spot says.
    fn lines(self, map: &HandBindings) -> Vec<String> {
        match self {
            Spot::Round(.., b) | Spot::Pill(_, _, b) | Spot::Arm(_, _, b) => {
                vec![map.button(b).short_label().to_string()]
            }
            Spot::Bumper => vec![map.button(Button::Shoulder).short_label().to_string()],
            Spot::Stick(..) => {
                let mut l = vec![
                    format!("←→ {}", map.axis(Axis::StickX).short_label()),
                    format!("↑↓ {}", map.axis(Axis::StickY).short_label()),
                    format!("Press {}", map.button(Button::StickClick).short_label()),
                ];
                for (axis, arrows) in [(Axis::GripX, "←→"), (Axis::GripY, "↑↓")] {
                    let a = map.axis(axis);
                    if a != AxisAction::Nothing {
                        l.push(format!("Grip {arrows} {}", a.short_label()));
                    }
                }
                l
            }
        }
    }
}

/// Width of a label holding `lines`.
fn label_width(ui: &egui::Ui, lines: &[String]) -> f32 {
    let font = theme::font(Weight::Medium, 14.0);
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

/// Both controllers, then the chosen input's choices.
pub fn controller_map(ui: &mut egui::Ui, v: &mut View) {
    let avail = ui.available_width();
    let each = ((avail - 24.0) / 2.0).max(300.0);
    // The label columns' real widths decide how big the controllers can be.
    let cols = [LEFT, RIGHT].map(|hand| {
        let map = v.settings.controls.hand(hand);
        let mut w = [0.0_f32; 2];
        for spot in spots(hand) {
            let col = (spot.side(hand) >= 0.0) as usize;
            w[col] = w[col].max(label_width(ui, &spot.lines(map)));
        }
        w
    });
    let r = cols
        .iter()
        .map(|w| (each - w[0] - w[1] - 2.0 * GAP) / 2.0)
        .fold(f32::MAX, f32::min)
        .clamp(54.0, 140.0);
    let h = r * 2.55 + 64.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(avail, h), Sense::hover());
    let t = ui.input(|i| i.time) as f32;
    for (i, hand) in [LEFT, RIGHT].into_iter().enumerate() {
        let x0 = rect.left() + i as f32 * (each + 24.0);
        // Centred in its half, then nudged so both label columns fit.
        let [lw, rw] = cols[i];
        let (lo, hi) = (x0 + lw + GAP + r, x0 + each - rw - GAP - r);
        let cx = if lo <= hi {
            (x0 + each / 2.0).clamp(lo, hi)
        } else {
            (lo + hi) / 2.0
        };
        let centre = Pos2::new(cx, rect.top() + 20.0 + r * 1.14);
        draw_hand(ui, v, hand, centre, r, t);
        ui.painter().text(
            Pos2::new(centre.x, rect.bottom()),
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
    let at = |x: f32, y: f32| Pos2::new(c.x + x * r, c.y + y * r);
    let p = ui.painter().clone();

    if let Some(tex) = picture(ui.ctx(), hand, false) {
        let [l, t_, rr, b] = picture_rect(hand);
        p.image(
            tex.id(),
            Rect::from_min_max(at(l, t_), at(rr, b)),
            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
            Color32::WHITE,
        );
    }

    let map = v.settings.controls.hand(hand).clone();
    let open = v.state.remap_open;
    let mut labels: Vec<(f32, Pos2, Vec<String>, f32)> = Vec::new();
    for (k, spot) in spots(hand).into_iter().enumerate() {
        let (sx, sy) = spot.centre(hand);
        let pos = at(sx, sy);
        let hit = match spot {
            Spot::Round(_, _, rad, ..) => Rect::from_center_size(
                pos,
                Vec2::new(2.0, 2.0 * SQUASH) * rad * r + Vec2::splat(10.0),
            ),
            Spot::Pill(..) => Rect::from_center_size(pos, Vec2::new(0.34 * r, 0.2 * r + 8.0)),
            Spot::Arm(..) => Rect::from_center_size(pos, Vec2::splat(0.25 * r)),
            Spot::Stick(..) => Rect::from_center_size(pos, Vec2::splat(0.66 * r)),
            Spot::Bumper => {
                let [l, t, rr, b] = bumper_rect(hand);
                Rect::from_min_max(at(l, t), at(rr, b)).expand(6.0)
            }
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
        draw_spot(
            &p,
            spot,
            pos,
            r,
            strength,
            sel,
            hand,
            picture_at(ui.ctx(), hand, c, r),
        );
        if resp.clicked() {
            v.state.remap_open = if selected {
                None
            } else {
                Some(spot.slot(hand))
            };
        }
        labels.push((spot.side(hand), pos, spot.lines(&map), hover.max(sel)));
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
            let font = theme::font(Weight::Medium, 14.0);
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

/// The bumper's glow texture and the rect the picture fills, for
/// [`draw_spot`].
fn picture_at(
    ctx: &egui::Context,
    hand: usize,
    c: Pos2,
    r: f32,
) -> Option<(egui::TextureHandle, Rect)> {
    let [l, t, rr, b] = picture_rect(hand);
    let rect = Rect::from_min_max(
        Pos2::new(c.x + l * r, c.y + t * r),
        Pos2::new(c.x + rr * r, c.y + b * r),
    );
    picture(ctx, hand, true).map(|t| (t, rect))
}

/// One hotspot: a halo round the real button in the picture, filled with
/// the accent while its choices are open; the bumper glows through its
/// own mask.
#[allow(clippy::too_many_arguments)]
fn draw_spot(
    p: &egui::Painter,
    spot: Spot,
    pos: Pos2,
    r: f32,
    strength: f32,
    sel: f32,
    _hand: usize,
    bumper_glow: Option<(egui::TextureHandle, Rect)>,
) {
    let glow = |a: f32| theme::ACCENT.gamma_multiply((a * strength).clamp(0.0, 1.0));
    let fill = theme::ACCENT.gamma_multiply(0.45 * sel);
    let ring = Stroke::new(
        2.0_f32,
        theme::ACCENT_HOVER.gamma_multiply(strength.max(sel)),
    );
    let oval = |rad: f32| Vec2::new(rad, rad * SQUASH);
    match spot {
        Spot::Round(..) | Spot::Stick(..) => {
            let rr = match spot {
                Spot::Round(_, _, rad, ..) => rad,
                _ => 0.28,
            } * r;
            for (k, a) in [0.1, 0.2, 0.35].into_iter().enumerate() {
                let e = 2.0 + (3 - k) as f32 * 2.5;
                p.add(Shape::ellipse_stroke(
                    pos,
                    oval(rr) + Vec2::splat(e),
                    Stroke::new(3.0_f32, glow(a)),
                ));
            }
            p.add(Shape::ellipse_filled(pos, oval(rr), fill));
            p.add(Shape::ellipse_stroke(
                pos,
                oval(rr) + Vec2::splat(1.5),
                ring,
            ));
        }
        Spot::Pill(..) | Spot::Arm(..) => {
            let (size, round) = match spot {
                Spot::Pill(..) => (Vec2::new(0.3 * r, 0.15 * r * SQUASH), 0.06 * r),
                _ => (Vec2::new(0.2 * r, 0.2 * r * SQUASH), 0.04 * r),
            };
            let rect = Rect::from_center_size(pos, size);
            for (k, a) in [0.1, 0.2, 0.35].into_iter().enumerate() {
                let e = 2.0 + (3 - k) as f32 * 2.5;
                p.rect_stroke(
                    rect.expand(e),
                    CornerRadius::same((round + e) as u8),
                    Stroke::new(3.0_f32, glow(a)),
                    StrokeKind::Middle,
                );
            }
            p.rect_filled(rect, CornerRadius::same(round as u8), fill);
            p.rect_stroke(
                rect.expand(1.5),
                CornerRadius::same((round + 1.5) as u8),
                ring,
                StrokeKind::Middle,
            );
        }
        Spot::Bumper => {
            if let Some((tex, rect)) = bumper_glow {
                let tint = theme::ACCENT.gamma_multiply((0.75 * strength).max(0.95 * sel));
                p.image(
                    tex.id(),
                    rect,
                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                    tint,
                );
            }
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
