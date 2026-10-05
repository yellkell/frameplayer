//! UI panels placed in the world, and controller pointing at them.

use egui::{Event, Modifiers, PointerButton, Pos2};
use fp_render::{PanelId, QuadDraw, QuadTexture, Renderer};
use fp_xr::Hand;
use glam::{Mat4, Quat, Vec2, Vec3};
use std::time::{Duration, Instant};

/// A flat UI surface rendered with egui.
pub struct Panel {
    pub ctx: egui::Context,
    pub id: Option<PanelId>,
    /// Texture size in pixels.
    pub px: [u32; 2],
    /// Pixels per egui point.
    pub ppp: f32,
    /// Size in metres.
    pub size: Vec2,
    /// Centre and orientation; the panel faces +Z of this transform.
    pub pose: Mat4,
    pub visible: bool,
    pub opacity: f32,
    /// 0 hidden → 1 shown, following `visible` over a short fade.
    fade: f32,
    events: Vec<Event>,
    hovered: bool,
    last_paint: Option<Instant>,
    /// Repaint at least this often while visible (live content).
    pub refresh: Option<Duration>,
    repaint_requested: bool,
}

impl Panel {
    pub fn new(px: [u32; 2], size: Vec2, ppp: f32) -> Panel {
        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(ppp);
        crate::ui::theme::apply(&ctx);
        Panel {
            ctx,
            id: None,
            px,
            ppp,
            size,
            pose: Mat4::IDENTITY,
            visible: false,
            opacity: 1.0,
            fade: 0.0,
            events: Vec::new(),
            hovered: false,
            last_paint: None,
            refresh: None,
            repaint_requested: true,
        }
    }

    /// Size of the panel in egui points.
    pub fn points(&self) -> egui::Vec2 {
        egui::vec2(self.px[0] as f32 / self.ppp, self.px[1] as f32 / self.ppp)
    }

    /// Ray hit: distance along the ray and position in egui points.
    pub fn hit(&self, origin: Vec3, dir: Vec3) -> Option<(f32, Pos2)> {
        let inv = self.pose.inverse();
        let o = inv.transform_point3(origin);
        let d = inv.transform_vector3(dir);
        if d.z.abs() < 1e-6 {
            return None;
        }
        let t = -o.z / d.z;
        if t <= 0.0 {
            return None;
        }
        let p = o + d * t;
        let (hw, hh) = (self.size.x / 2.0, self.size.y / 2.0);
        if p.x.abs() > hw || p.y.abs() > hh {
            return None;
        }
        let u = p.x / self.size.x + 0.5;
        let v = 0.5 - p.y / self.size.y;
        let pts = self.points();
        Some((t, Pos2::new(u * pts.x, v * pts.y)))
    }

    /// World position of a point given in egui points.
    pub fn world_point(&self, p: Pos2) -> Vec3 {
        let pts = self.points();
        let x = (p.x / pts.x - 0.5) * self.size.x;
        let y = (0.5 - p.y / pts.y) * self.size.y;
        self.pose.transform_point3(Vec3::new(x, y, 0.002))
    }

    pub fn push(&mut self, e: Event) {
        self.events.push(e);
        self.repaint_requested = true;
    }

    pub fn request_repaint(&mut self) {
        self.repaint_requested = true;
    }

    /// Whether egui should run this frame.
    pub fn needs_paint(&self) -> bool {
        if !self.visible {
            return false;
        }
        if self.repaint_requested || !self.events.is_empty() || self.last_paint.is_none() {
            return true;
        }
        match (self.refresh, self.last_paint) {
            (Some(r), Some(t)) => t.elapsed() >= r,
            _ => false,
        }
    }

    /// Runs egui with the queued input and paints into the renderer.
    pub fn paint(
        &mut self,
        renderer: &mut Renderer,
        time: f64,
        ui: impl FnMut(&egui::Context),
    ) -> fp_render::Result<()> {
        if self.id.is_none() {
            self.id = Some(renderer.create_panel(self.px[0], self.px[1])?);
        }
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(Pos2::ZERO, self.points())),
            time: Some(time),
            events: std::mem::take(&mut self.events),
            focused: true,
            ..Default::default()
        };
        let hovered = self.hovered;
        let mut ui = ui;
        let out = self.ctx.run(input, |ctx| {
            ui(ctx);
            // The pointer's dot, drawn with the panel so it sits exactly on
            // it (a quad layer would cover one drawn in the scene).
            if hovered && let Some(p) = ctx.input(|i| i.pointer.hover_pos()) {
                let painter = ctx.layer_painter(egui::LayerId::new(
                    egui::Order::Debug,
                    egui::Id::new("pointer-dot"),
                ));
                painter.circle_filled(p, 7.0, egui::Color32::from_black_alpha(110));
                painter.circle_filled(p, 5.0, egui::Color32::WHITE);
            }
        });
        let repaint = out
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|v| v.repaint_delay < Duration::from_millis(100))
            .unwrap_or(false);
        let prims = self.ctx.tessellate(out.shapes, out.pixels_per_point);
        if let Some(id) = self.id {
            renderer.paint_panel(id, &prims, &out.textures_delta, out.pixels_per_point)?;
        }
        self.last_paint = Some(Instant::now());
        self.repaint_requested = repaint;
        Ok(())
    }

    /// Moves the fade towards `visible`: in over 0.15 s, out over 0.25 s.
    pub fn update_fade(&mut self, dt: f32) {
        self.fade = if self.visible {
            (self.fade + dt / 0.15).min(1.0)
        } else {
            (self.fade - dt / 0.25).max(0.0)
        };
    }

    /// Visible and done fading in: nothing about its look is animating.
    pub fn fully_shown(&self) -> bool {
        self.visible && self.fade >= 1.0 && self.opacity >= 0.999
    }

    /// Drawn at all: visible, or still fading out.
    pub fn shown(&self) -> bool {
        self.fade > 0.01 || self.visible
    }

    pub fn quad(&self) -> Option<QuadDraw> {
        let id = self.id?;
        // Smoothstep, so the fade starts and ends gently.
        let f = self.fade * self.fade * (3.0 - 2.0 * self.fade);
        let opacity = self.opacity * f;
        (self.shown() && opacity > 0.01).then(|| {
            QuadDraw::panel(
                QuadTexture::Panel(id),
                self.pose,
                self.size.x,
                self.size.y,
                opacity,
            )
        })
    }

    pub fn wants_keyboard(&self) -> bool {
        self.ctx.wants_keyboard_input()
    }
}

/// Places a panel `distance` metres in front of `anchor` (a yaw-only pose at
/// eye height), `offset_y` metres up, rotated `yaw_deg` around the viewer
/// (positive = to the left). Positive `tilt_deg` turns the panel's face
/// upwards, for panels below eye level.
pub fn place(anchor: Mat4, yaw_deg: f32, distance: f32, offset_y: f32, tilt_deg: f32) -> Mat4 {
    anchor
        * Mat4::from_rotation_y(yaw_deg.to_radians())
        * Mat4::from_translation(Vec3::new(0.0, offset_y, -distance))
        * Mat4::from_rotation_x(-tilt_deg.to_radians())
}

/// `anchor` turned about the viewer the way the renderer turns a 180°/360°
/// picture for its yaw and pitch (degrees, left and up positive), so a panel
/// placed from it moves with the picture. Roll is left out: it levels a
/// crooked camera, and a panel rolled with it would no longer be level.
pub fn turned_with_picture(anchor: Mat4, yaw_deg: f32, pitch_deg: f32) -> Mat4 {
    anchor
        * Mat4::from_euler(
            glam::EulerRot::YXZ,
            yaw_deg.to_radians(),
            pitch_deg.to_radians(),
            0.0,
        )
}

/// Yaw-only anchor at the head position.
pub fn anchor_from_head(position: Vec3, orientation: Quat) -> Mat4 {
    let (yaw, _, _) = orientation.to_euler(glam::EulerRot::YXZ);
    Mat4::from_rotation_translation(Quat::from_rotation_y(yaw), position)
}

/// Per-hand trigger state with hysteresis.
#[derive(Default, Clone, Copy)]
struct Trigger {
    down: bool,
}

impl Trigger {
    /// Returns Some(pressed) on a transition.
    fn update(&mut self, value: f32) -> Option<bool> {
        if !self.down && value > 0.7 {
            self.down = true;
            Some(true)
        } else if self.down && value < 0.35 {
            self.down = false;
            Some(false)
        } else {
            None
        }
    }
}

/// Where each hand points this frame.
#[derive(Clone, Copy, Debug, Default)]
pub struct Aim {
    pub origin: Vec3,
    pub dir: Vec3,
    /// Index into the panel list and the hit position.
    pub hit: Option<(usize, f32, Pos2)>,
}

/// How far (in panel points) the ray may move during a press before it
/// counts as a drag. Pulling a trigger tilts the controller, and on the main
/// panel (about 7 points per cm) that moves a laser pointer tens of points,
/// while egui only reports a click when press and release are within 6
/// points. Until the ray leaves this radius the pointer stays at the press
/// point, so a trigger pull is a click; beyond it, a drag (scrolling).
const CLICK_SLOP: f32 = 60.0;

#[derive(Default)]
pub struct Pointer {
    triggers: [Trigger; 2],
    /// The hand driving the UI.
    pub active: usize,
    /// Panel that received the last press, so the release goes there too.
    pressed_panel: Option<usize>,
    /// Where that press landed, and whether the ray has since left
    /// [`CLICK_SLOP`] of it.
    press_pos: Option<Pos2>,
    dragging: bool,
    hovered_panel: Option<usize>,
    pub aims: [Option<Aim>; 2],
}

/// Result of routing controller input to panels.
#[derive(Default, Debug, Clone, Copy)]
pub struct Routed {
    /// The active hand points at some panel.
    pub over_ui: bool,
    /// A trigger was pressed while not pointing at any panel.
    pub click_outside: bool,
}

impl Pointer {
    /// The position to report for a ray hit on `panel`: the press point
    /// while a press on that panel hasn't moved beyond [`CLICK_SLOP`].
    fn pinned(&mut self, panel: usize, pos: Pos2) -> Pos2 {
        match (self.pressed_panel, self.press_pos) {
            (Some(pressed), Some(start)) if pressed == panel && !self.dragging => {
                if pos.distance(start) <= CLICK_SLOP {
                    start
                } else {
                    self.dragging = true;
                    pos
                }
            }
            _ => pos,
        }
    }

    /// Routes the hands' rays and triggers into panel events.
    pub fn route(&mut self, panels: &mut [&mut Panel], hands: &[Hand; 2], dt: f32) -> Routed {
        let mut routed = Routed::default();
        for (i, h) in hands.iter().enumerate() {
            self.aims[i] = h.aim.map(|(pos, rot)| {
                let dir = rot * Vec3::NEG_Z;
                let mut best: Option<(usize, f32, Pos2)> = None;
                for (pi, p) in panels.iter().enumerate() {
                    if !p.visible {
                        continue;
                    }
                    if let Some((t, pt)) = p.hit(pos, dir)
                        && best.is_none_or(|b| t < b.1)
                    {
                        best = Some((pi, t, pt));
                    }
                }
                Aim {
                    origin: pos,
                    dir,
                    hit: best,
                }
            });
        }
        // A trigger press makes that hand the active one.
        let mut transitions = [None, None];
        for (i, h) in hands.iter().enumerate() {
            transitions[i] = self.triggers[i].update(h.trigger);
            if transitions[i] == Some(true) {
                self.active = i;
            }
        }
        if self.aims[self.active].is_none()
            && let Some(other) = (0..2).find(|&i| self.aims[i].is_some())
        {
            self.active = other;
        }
        let aim = self.aims[self.active];
        let hit = aim.and_then(|a| a.hit);
        // Hover changes.
        let target = hit.map(|h| h.0);
        if self.hovered_panel != target {
            if let Some(old) = self.hovered_panel
                && let Some(p) = panels.get_mut(old)
            {
                p.push(Event::PointerGone);
                p.hovered = false;
            }
            self.hovered_panel = target;
        }
        if let Some((pi, _, pos)) = hit {
            routed.over_ui = true;
            let pos = self.pinned(pi, pos);
            let p = &mut panels[pi];
            p.hovered = true;
            p.push(Event::PointerMoved(pos));
            let stick = hands[self.active].stick;
            if stick.y.abs() > 0.2 {
                p.push(Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, stick.y * 900.0 * dt),
                    modifiers: Modifiers::NONE,
                });
            }
        }
        match transitions[self.active] {
            Some(true) => {
                if let Some((pi, _, pos)) = hit {
                    panels[pi].push(Event::PointerButton {
                        pos,
                        button: PointerButton::Primary,
                        pressed: true,
                        modifiers: Modifiers::NONE,
                    });
                    self.pressed_panel = Some(pi);
                    self.press_pos = Some(pos);
                    self.dragging = false;
                } else {
                    routed.click_outside = true;
                }
            }
            Some(false) => {
                if let Some(pi) = self.pressed_panel.take() {
                    let start = self.press_pos.take();
                    let pos = hit
                        .filter(|h| h.0 == pi)
                        .map(|h| match start {
                            Some(start) if !self.dragging => start,
                            _ => h.2,
                        })
                        .unwrap_or(Pos2::new(-1.0, -1.0));
                    self.dragging = false;
                    panels[pi].push(Event::PointerButton {
                        pos,
                        button: PointerButton::Primary,
                        pressed: false,
                        modifiers: Modifiers::NONE,
                    });
                }
            }
            None => {}
        }
        routed
    }

    /// The panel the active hand points at.
    pub fn hovered(&self) -> Option<usize> {
        self.hovered_panel
    }

    /// Pointer rays and cursor dots to draw.
    pub fn quads(&self, eye: Vec3, show: bool) -> Vec<QuadDraw> {
        let mut out = Vec::new();
        if !show {
            return out;
        }
        for (i, aim) in self.aims.iter().enumerate() {
            let Some(a) = aim else { continue };
            let active = i == self.active;
            let len = a.hit.map(|h| h.1).unwrap_or(2.0).min(5.0);
            let end = a.origin + a.dir * len;
            let alpha = if active { 0.85 } else { 0.35 };
            let color = [0.55 * alpha, 0.75 * alpha, 1.0 * alpha, alpha];
            out.push(QuadDraw::line(
                a.origin + a.dir * 0.02,
                end,
                if active { 0.004 } else { 0.0025 },
                eye,
                color,
            ));
        }
        out
    }

    /// The rays as quad layers: each a thin strip from the hand to what it
    /// points at, turned to face the eye. (The dot is drawn on the panel.)
    pub fn ray_layers(&self, eye: Vec3, show: bool) -> Vec<crate::app::LayerDraw> {
        let mut out = Vec::new();
        if !show {
            return out;
        }
        for (i, aim) in self.aims.iter().enumerate() {
            let Some(a) = aim else { continue };
            let active = i == self.active;
            let len = a.hit.map(|h| h.1).unwrap_or(2.0).min(5.0) - 0.02;
            if len <= 0.01 {
                continue;
            }
            let start = a.origin + a.dir * 0.02;
            let centre = start + a.dir * (len / 2.0);
            let x = a.dir.normalize_or_zero();
            let to_eye = eye - centre;
            let z = (to_eye - x * to_eye.dot(x)).normalize_or_zero();
            if z == Vec3::ZERO {
                continue;
            }
            let y = z.cross(x);
            let rot = Quat::from_mat3(&glam::Mat3::from_cols(x, y, z));
            out.push(crate::app::LayerDraw::Ray {
                active,
                pose: (centre, rot),
                size: Vec2::new(len, if active { 0.006 } else { 0.004 }),
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panel_in_front() -> Panel {
        let mut p = Panel::new([800, 400], Vec2::new(1.6, 0.8), 1.0);
        p.pose = Mat4::from_translation(Vec3::new(0.0, 0.0, -2.0));
        p.visible = true;
        p
    }

    #[test]
    fn ray_hits_map_to_points() {
        let p = panel_in_front();
        let (t, pt) = p.hit(Vec3::ZERO, Vec3::NEG_Z).unwrap();
        assert!((t - 2.0).abs() < 1e-5);
        assert!((pt.x - 400.0).abs() < 0.5 && (pt.y - 200.0).abs() < 0.5);
        // Upper-left corner region.
        let dir = (Vec3::new(-0.7, 0.35, -2.0)).normalize();
        let (_, pt) = p.hit(Vec3::ZERO, dir).unwrap();
        assert!(pt.x < 60.0 && pt.y < 30.0, "{pt:?}");
        assert!(p.hit(Vec3::ZERO, Vec3::Z).is_none(), "behind the viewer");
        assert!(
            p.hit(Vec3::ZERO, Vec3::new(1.0, 0.0, -0.1).normalize())
                .is_none(),
            "off to the side"
        );
        let back = p.world_point(pt);
        assert!((back.z + 2.0).abs() < 0.01);
    }

    fn button_positions(p: &Panel) -> Vec<Pos2> {
        p.events
            .iter()
            .filter_map(|e| match e {
                Event::PointerButton { pos, .. } => Some(*pos),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_wobbling_trigger_pull_still_clicks() {
        let mut p = panel_in_front();
        let mut ptr = Pointer::default();
        let mut hands = [Hand::default(), Hand::default()];
        hands[1].active = true;
        hands[1].aim = Some((Vec3::ZERO, Quat::IDENTITY));
        ptr.route(&mut [&mut p], &hands, 0.016);
        hands[1].trigger = 0.9;
        ptr.route(&mut [&mut p], &hands, 0.016);
        // Pulling the trigger tips the controller about a degree: ~17 points
        // on this panel, far beyond egui's 6-point click distance.
        hands[1].aim = Some((Vec3::ZERO, Quat::from_rotation_x(-0.0175)));
        ptr.route(&mut [&mut p], &hands, 0.016);
        hands[1].trigger = 0.1;
        ptr.route(&mut [&mut p], &hands, 0.016);
        let b = button_positions(&p);
        assert_eq!(b.len(), 2);
        assert_eq!(b[0], b[1], "release reported at the press point");

        // A long sweep is a drag: the release lands where the ray is.
        p.events.clear();
        hands[1].aim = Some((Vec3::ZERO, Quat::IDENTITY));
        hands[1].trigger = 0.9;
        ptr.route(&mut [&mut p], &hands, 0.016);
        hands[1].aim = Some((Vec3::ZERO, Quat::from_rotation_x(-0.1)));
        ptr.route(&mut [&mut p], &hands, 0.016);
        hands[1].trigger = 0.1;
        ptr.route(&mut [&mut p], &hands, 0.016);
        let b = button_positions(&p);
        assert_eq!(b.len(), 2);
        assert!(b[0].distance(b[1]) > CLICK_SLOP, "{b:?}");
    }

    #[test]
    fn trigger_clicks_go_to_the_panel() {
        let mut p = panel_in_front();
        let mut ptr = Pointer::default();
        let mut hands = [Hand::default(), Hand::default()];
        hands[1].aim = Some((Vec3::ZERO, Quat::IDENTITY));
        hands[1].active = true;
        let r = ptr.route(&mut [&mut p], &hands, 0.016);
        assert!(r.over_ui);
        hands[1].trigger = 0.9;
        ptr.route(&mut [&mut p], &hands, 0.016);
        hands[1].trigger = 0.5; // hysteresis: still down
        ptr.route(&mut [&mut p], &hands, 0.016);
        hands[1].trigger = 0.1;
        ptr.route(&mut [&mut p], &hands, 0.016);
        let presses: Vec<bool> = p
            .events
            .iter()
            .filter_map(|e| match e {
                Event::PointerButton { pressed, .. } => Some(*pressed),
                _ => None,
            })
            .collect();
        assert_eq!(presses, vec![true, false]);
        assert_eq!(ptr.active, 1);
    }

    #[test]
    fn clicking_empty_space_is_reported() {
        let mut p = panel_in_front();
        let mut ptr = Pointer::default();
        let mut hands = [Hand::default(), Hand::default()];
        hands[0].aim = Some((Vec3::ZERO, Quat::from_rotation_y(std::f32::consts::PI)));
        hands[0].trigger = 1.0;
        let r = ptr.route(&mut [&mut p], &hands, 0.016);
        assert!(r.click_outside && !r.over_ui);
        assert_eq!(ptr.active, 0);
    }

    #[test]
    fn turned_panels_follow_the_picture() {
        let head = Vec3::new(0.0, 1.6, 0.0);
        let anchor = anchor_from_head(head, Quat::from_rotation_y(0.4));
        let (anchor_yaw, _, _) = anchor
            .to_scale_rotation_translation()
            .1
            .to_euler(glam::EulerRot::YXZ);
        let (yaw, pitch) = (35.0_f32, -20.0_f32);
        // Where the renderer shows the picture's centre: its correction is
        // the inverse of this rotation (fp-render params.rs), with the
        // anchor's yaw added to the picture's (app.rs).
        let picture = Mat4::from_euler(
            glam::EulerRot::YXZ,
            anchor_yaw + yaw.to_radians(),
            pitch.to_radians(),
            0.0,
        )
        .transform_vector3(Vec3::NEG_Z);
        let turned = turned_with_picture(anchor, yaw, pitch);
        let ahead = (place(turned, 0.0, 1.0, 0.0, 0.0).transform_point3(Vec3::ZERO) - head)
            .normalize();
        assert!(ahead.distance(picture) < 1e-4, "{ahead} vs {picture}");
        assert!(ahead.y < -0.3, "negative pitch moves it down");
        // The bar's spot below the view stays below it, at the same distance.
        let bar = place(turned, 0.0, 1.05, -0.42, 28.0).transform_point3(Vec3::ZERO);
        let rest = place(anchor, 0.0, 1.05, -0.42, 28.0).transform_point3(Vec3::ZERO);
        assert!(((bar - head).length() - (rest - head).length()).abs() < 1e-4);
        assert!((bar - head).dot(picture) > 0.9 * (bar - head).length());
        // No roll: the panel's right edge stays level.
        let right = turned.transform_vector3(Vec3::X);
        assert!(right.y.abs() < 1e-5, "{right}");
        assert_eq!(turned_with_picture(anchor, 0.0, 0.0), anchor);
    }

    #[test]
    fn placement_faces_the_viewer() {
        let anchor = anchor_from_head(
            Vec3::new(0.0, 1.6, 0.0),
            Quat::from_rotation_y(0.3) * Quat::from_rotation_x(0.4),
        );
        let m = place(anchor, 0.0, 2.0, 0.0, 0.0);
        let centre = m.transform_point3(Vec3::ZERO);
        assert!((centre.y - 1.6).abs() < 1e-4, "pitch ignored for placement");
        assert!(((centre - Vec3::new(0.0, 1.6, 0.0)).length() - 2.0).abs() < 1e-4);
        let normal = m.transform_vector3(Vec3::Z);
        assert!(
            normal.dot(Vec3::new(0.0, 1.6, 0.0) - centre) > 0.0,
            "faces the viewer"
        );
        let low = place(anchor, 0.0, 1.0, -0.5, 30.0);
        assert!(
            low.transform_vector3(Vec3::Z).y > 0.3,
            "tilted panels face up"
        );
        let left = place(anchor, 30.0, 1.0, 0.0, 0.0).transform_point3(Vec3::ZERO);
        let fwd = anchor.transform_vector3(Vec3::NEG_Z);
        let right = anchor.transform_vector3(Vec3::X);
        assert!(
            left.dot(fwd) > 0.0 && (left - Vec3::new(0.0, 1.6, 0.0)).dot(right) < 0.0,
            "positive yaw is to the left"
        );
    }
}
