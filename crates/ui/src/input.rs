//! Input model: pointers from lasers, eye gaze, and hands; focus navigation
//! from thumbsticks/D-pad; text events; and the 3D → panel mapping.
//!
//! The XR layer turns poses into rays and calls [`ray_quad`] / [`ray_cylinder`]
//! to get panel-pixel hits, then fills a [`FrameInput`] for [`crate::Ui::begin_frame`].

use crate::geom::Vec2;
use glam::{Quat, Vec3};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Hand {
    Left,
    Right,
}

/// Where a pointer comes from. Each source is tracked independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PointerSource {
    /// Controller laser; `pressed` = trigger, `touch_hint` = finger resting on trigger.
    Laser(Hand),
    /// Eye-gaze ray; `pressed` = pinch (either hand).
    Gaze,
    /// Hand-tracking ray (aim pose); `pressed` = index–thumb pinch.
    HandPinch(Hand),
    /// Index fingertip touching the panel; `pressed` derived with [`PokeDetector`].
    HandPoke(Hand),
    /// Desktop mouse (debug / simulator).
    Mouse,
}

impl PointerSource {
    /// Sources that can deliver controller haptic ticks.
    pub fn has_haptics(self) -> bool {
        matches!(self, PointerSource::Laser(_))
    }
}

/// One pointer's state for this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointerInput {
    pub source: PointerSource,
    /// Hit position in panel pixels; `None` when the ray misses the panel.
    pub pos: Option<Vec2>,
    pub pressed: bool,
    /// Capacitive "finger on trigger" hint: highlight without clicking.
    pub touch_hint: bool,
    /// Thumbstick / scroll wheel delta for this pointer (x right, y up), -1..1.
    pub scroll: Vec2,
}

impl PointerInput {
    pub fn new(source: PointerSource, pos: Option<Vec2>, pressed: bool) -> PointerInput {
        PointerInput {
            source,
            pos,
            pressed,
            touch_hint: false,
            scroll: Vec2::ZERO,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NavDir {
    Up,
    Down,
    Left,
    Right,
}

/// Button-style navigation for this frame (edge-triggered).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct NavInput {
    /// D-pad or thumbstick flick (already debounced by [`StickNavigator`]).
    pub dir: Option<NavDir>,
    /// A button.
    pub activate: bool,
    /// B button.
    pub back: bool,
}

/// Text editing events from a hardware/remote keyboard or the virtual keyboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextEvent {
    Text(String),
    Backspace,
    Delete,
    Left,
    Right,
    Home,
    End,
    Enter,
}

/// Everything the UI needs for one frame.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FrameInput {
    /// Seconds since the previous frame.
    pub dt: f32,
    pub pointers: Vec<PointerInput>,
    pub nav: NavInput,
    pub text: Vec<TextEvent>,
    /// Whether the eye-gaze ray currently hits this panel (`None` = no eye tracking).
    pub gaze_on_panel: Option<bool>,
}

/// Panel hit returned by the ray tests.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanelHit {
    /// Normalized panel coordinates; 0..1 inside, may exceed for off-panel hits.
    pub uv: Vec2,
    /// Panel pixels (origin top-left).
    pub px: Vec2,
    /// Distance along the (normalized) ray.
    pub distance: f32,
    pub point: Vec3,
}

impl PanelHit {
    pub fn inside(&self) -> bool {
        (0.0..=1.0).contains(&self.uv.x) && (0.0..=1.0).contains(&self.uv.y)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ray {
    pub origin: Vec3,
    pub dir: Vec3,
}

impl Ray {
    pub fn new(origin: Vec3, dir: Vec3) -> Ray {
        Ray {
            origin,
            dir: dir.normalize_or_zero(),
        }
    }

    /// Ray along a pose's -Z (OpenXR aim-pose convention).
    pub fn from_pose(position: Vec3, orientation: Quat) -> Ray {
        Ray::new(position, orientation * Vec3::NEG_Z)
    }
}

/// A flat panel matching `XrCompositionLayerQuad`: centred on `position`,
/// lying in its local XY plane and facing local +Z.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuadPanel {
    pub position: Vec3,
    pub orientation: Quat,
    pub size_m: Vec2,
    pub size_px: Vec2,
}

/// Ray vs front face of a quad panel. Returns hits outside the panel rectangle
/// too (check [`PanelHit::inside`]) so drags can continue past the edge.
pub fn ray_quad(ray: &Ray, q: &QuadPanel) -> Option<PanelHit> {
    let inv = q.orientation.inverse();
    let o = inv * (ray.origin - q.position);
    let d = inv * ray.dir;
    // Must travel towards the front face (local +Z side, heading -Z).
    if d.z >= -1e-6 {
        return None;
    }
    let t = -o.z / d.z;
    if t < 0.0 {
        return None;
    }
    let p = o + d * t;
    let uv = Vec2::new(p.x / q.size_m.x + 0.5, 0.5 - p.y / q.size_m.y);
    Some(PanelHit {
        uv,
        px: uv * q.size_px,
        distance: t,
        point: ray.origin + ray.dir * t,
    })
}

/// A curved panel matching `XrCompositionLayerCylinderKHR`: axis along local
/// +Y through `position`, arc of `central_angle` centred on local -Z, viewed
/// from inside.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CylinderPanel {
    pub position: Vec3,
    pub orientation: Quat,
    pub radius: f32,
    /// Radians.
    pub central_angle: f32,
    /// Arc length / height, as in the OpenXR struct.
    pub aspect_ratio: f32,
    pub size_px: Vec2,
}

impl CylinderPanel {
    pub fn height(&self) -> f32 {
        self.radius * self.central_angle / self.aspect_ratio
    }
}

/// Ray vs the inner surface of a cylinder panel. Like [`ray_quad`], returns
/// out-of-range uvs for hits on the cylinder outside the arc/height.
///
/// [verify] that SteamVR's `XR_KHR_composition_layer_cylinder` on the Frame
/// centres the arc on local -Z with u increasing to the right, as the spec says.
pub fn ray_cylinder(ray: &Ray, c: &CylinderPanel) -> Option<PanelHit> {
    let inv = c.orientation.inverse();
    let o = inv * (ray.origin - c.position);
    let d = inv * ray.dir;
    let (ox, oz, dx, dz) = (o.x, o.z, d.x, d.z);
    let a = dx * dx + dz * dz;
    if a < 1e-9 {
        return None;
    }
    let b = 2.0 * (ox * dx + oz * dz);
    let cc = ox * ox + oz * oz - c.radius * c.radius;
    let disc = b * b - 4.0 * a * cc;
    if disc < 0.0 {
        return None;
    }
    let sq = disc.sqrt();
    let (t0, t1) = ((-b - sq) / (2.0 * a), (-b + sq) / (2.0 * a));
    let h = c.height();
    let mut best: Option<PanelHit> = None;
    for t in [t0, t1] {
        if t < 0.0 {
            continue;
        }
        let p = o + d * t;
        // Only the inner face counts: the ray must be heading outwards.
        if p.x * dx + p.z * dz <= 0.0 {
            continue;
        }
        let theta = p.x.atan2(-p.z);
        let uv = Vec2::new(theta / c.central_angle + 0.5, 0.5 - p.y / h);
        let hit = PanelHit {
            uv,
            px: uv * c.size_px,
            distance: t,
            point: ray.origin + ray.dir * t,
        };
        if hit.inside() {
            return Some(hit);
        }
        best.get_or_insert(hit);
    }
    best
}

/// Turns a fingertip's signed distance to the panel into press/release with
/// hysteresis, so tremor at the surface doesn't chatter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PokeDetector {
    /// Press when the tip is closer than this (m); negative = behind the surface.
    pub press_at: f32,
    /// Release when farther than this.
    pub release_at: f32,
    /// Show hover when within this distance.
    pub hover_at: f32,
    pub pressed: bool,
}

impl Default for PokeDetector {
    fn default() -> Self {
        // [verify] tune against Frame hand-tracking jitter.
        PokeDetector {
            press_at: 0.005,
            release_at: 0.015,
            hover_at: 0.08,
            pressed: false,
        }
    }
}

impl PokeDetector {
    /// Returns `(pressed, hovering)`.
    pub fn update(&mut self, distance_m: f32) -> (bool, bool) {
        if self.pressed {
            if distance_m > self.release_at {
                self.pressed = false;
            }
        } else if distance_m < self.press_at {
            self.pressed = true;
        }
        (self.pressed, distance_m < self.hover_at)
    }
}

/// Converts an analogue thumbstick into discrete navigation steps with
/// initial delay and auto-repeat.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StickNavigator {
    pub threshold: f32,
    pub repeat_delay: f32,
    pub repeat_interval: f32,
    held: Option<NavDir>,
    timer: f32,
}

impl Default for StickNavigator {
    fn default() -> Self {
        StickNavigator {
            threshold: 0.6,
            repeat_delay: 0.45,
            repeat_interval: 0.12,
            held: None,
            timer: 0.0,
        }
    }
}

impl StickNavigator {
    /// `stick` is x right, y up in -1..1.
    pub fn update(&mut self, stick: Vec2, dt: f32) -> Option<NavDir> {
        let dir = if stick.length() < self.threshold {
            None
        } else if stick.x.abs() > stick.y.abs() {
            Some(if stick.x > 0.0 {
                NavDir::Right
            } else {
                NavDir::Left
            })
        } else {
            Some(if stick.y > 0.0 {
                NavDir::Up
            } else {
                NavDir::Down
            })
        };
        if dir != self.held {
            self.held = dir;
            self.timer = self.repeat_delay;
            return dir;
        }
        dir?;
        self.timer -= dt;
        if self.timer <= 0.0 {
            self.timer += self.repeat_interval;
            return dir;
        }
        None
    }
}

/// Fades the UI down while the user is watching video and back up when they
/// look at it or interact.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GazeDimmer {
    pub enabled: bool,
    /// Opacity when dimmed.
    pub dim_opacity: f32,
    /// Seconds without gaze or interaction before dimming starts.
    pub idle_delay: f32,
    /// Exponential approach rate (1/s) when fading in / out.
    pub fade_in_rate: f32,
    pub fade_out_rate: f32,
    opacity: f32,
    idle: f32,
}

impl Default for GazeDimmer {
    fn default() -> Self {
        GazeDimmer {
            enabled: true,
            dim_opacity: 0.15,
            idle_delay: 2.5,
            fade_in_rate: 14.0,
            fade_out_rate: 2.5,
            opacity: 1.0,
            idle: 0.0,
        }
    }
}

impl GazeDimmer {
    pub fn opacity(&self) -> f32 {
        if self.enabled {
            self.opacity
        } else {
            1.0
        }
    }

    /// Seconds since the last gaze-on-UI or interaction.
    pub fn idle_time(&self) -> f32 {
        self.idle
    }

    /// `gaze_on_ui`: `None` without eye tracking. A pointer resting on the
    /// panel or any interaction also counts as engagement.
    pub fn update(
        &mut self,
        dt: f32,
        gaze_on_ui: Option<bool>,
        pointer_on_ui: bool,
        interacted: bool,
    ) -> f32 {
        let engaged = interacted || pointer_on_ui || gaze_on_ui == Some(true);
        if engaged {
            self.idle = 0.0;
        } else {
            self.idle += dt;
        }
        let target = if self.idle < self.idle_delay {
            1.0
        } else {
            self.dim_opacity
        };
        let rate = if target > self.opacity {
            self.fade_in_rate
        } else {
            self.fade_out_rate
        };
        let k = 1.0 - (-rate * dt.max(0.0)).exp();
        self.opacity += (target - self.opacity) * k;
        if (self.opacity - target).abs() < 1e-3 {
            self.opacity = target;
        }
        self.opacity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::{FRAC_PI_2, FRAC_PI_4};

    fn quad() -> QuadPanel {
        QuadPanel {
            position: Vec3::new(0.0, 1.5, -1.5),
            orientation: Quat::IDENTITY,
            size_m: Vec2::new(1.0, 0.5),
            size_px: Vec2::new(1000.0, 500.0),
        }
    }

    #[test]
    fn ray_hits_quad_centre_and_corner() {
        let q = quad();
        let h = ray_quad(&Ray::new(Vec3::new(0.0, 1.5, 0.0), Vec3::NEG_Z), &q).unwrap();
        assert!((h.px - Vec2::new(500.0, 250.0)).length() < 1e-3);
        assert!((h.distance - 1.5).abs() < 1e-5);
        // Aim at the top-left corner region.
        let target = Vec3::new(-0.4, 1.7, -1.5);
        let o = Vec3::new(0.2, 1.4, 0.0);
        let h = ray_quad(&Ray::new(o, target - o), &q).unwrap();
        assert!(
            (h.px - Vec2::new(100.0, 50.0)).length() < 1e-2,
            "{:?}",
            h.px
        );
        assert!(h.inside());
    }

    #[test]
    fn ray_misses_quad_from_behind_or_away() {
        let q = quad();
        assert!(ray_quad(&Ray::new(Vec3::new(0.0, 1.5, 0.0), Vec3::Z), &q).is_none());
        assert!(ray_quad(&Ray::new(Vec3::new(0.0, 1.5, -3.0), Vec3::Z), &q).is_none());
        let off = ray_quad(&Ray::new(Vec3::new(2.0, 1.5, 0.0), Vec3::NEG_Z), &q).unwrap();
        assert!(!off.inside());
    }

    #[test]
    fn ray_hits_rotated_quad() {
        // Panel to the user's right, facing them (normal rotated to -X).
        let q = QuadPanel {
            position: Vec3::new(1.5, 1.5, 0.0),
            orientation: Quat::from_rotation_y(-FRAC_PI_2),
            ..quad()
        };
        let h = ray_quad(&Ray::new(Vec3::new(0.0, 1.5, 0.0), Vec3::X), &q).unwrap();
        assert!(
            (h.px - Vec2::new(500.0, 250.0)).length() < 1e-2,
            "{:?}",
            h.px
        );
    }

    fn cyl() -> CylinderPanel {
        CylinderPanel {
            position: Vec3::new(0.0, 1.5, 0.0),
            orientation: Quat::IDENTITY,
            radius: 2.0,
            central_angle: FRAC_PI_2,
            aspect_ratio: 2.0,
            size_px: Vec2::new(2000.0, 1000.0),
        }
    }

    #[test]
    fn ray_hits_cylinder_from_centre() {
        let c = cyl();
        let h = ray_cylinder(&Ray::new(Vec3::new(0.0, 1.5, 0.0), Vec3::NEG_Z), &c).unwrap();
        assert!((h.px - Vec2::new(1000.0, 500.0)).length() < 1e-2);
        assert!((h.distance - 2.0).abs() < 1e-4);
        // 22.5° to the right = 3/4 across the 90° arc.
        let dir = Quat::from_rotation_y(-FRAC_PI_4 / 2.0) * Vec3::NEG_Z;
        let h = ray_cylinder(&Ray::new(Vec3::new(0.0, 1.5, 0.0), dir), &c).unwrap();
        assert!((h.uv.x - 0.75).abs() < 1e-4, "{:?}", h.uv);
        // Upwards: height = 2 * (pi/2) / 2 = pi/2; aim at y = +h/4 → v = 0.25.
        let hgt = c.height();
        let target = Vec3::new(0.0, 1.5 + hgt / 4.0, -2.0);
        let o = Vec3::new(0.0, 1.5, 0.0);
        let h = ray_cylinder(&Ray::new(o, target - o), &c).unwrap();
        assert!((h.uv.y - 0.25).abs() < 1e-4, "{:?}", h.uv);
    }

    #[test]
    fn cylinder_outside_arc_and_from_outside() {
        let c = cyl();
        // Looking backwards hits the cylinder outside the arc.
        let h = ray_cylinder(&Ray::new(Vec3::new(0.0, 1.5, 0.0), Vec3::Z), &c).unwrap();
        assert!(!h.inside());
        // From outside in front, ray towards the user passes the outer face first
        // (ignored) and hits the inner face on the far side (behind), out of arc.
        let h = ray_cylinder(&Ray::new(Vec3::new(0.0, 1.5, -3.0), Vec3::Z), &c);
        assert!(!matches!(h, Some(h) if h.inside()));
        // Straight up never hits.
        assert!(ray_cylinder(&Ray::new(Vec3::new(0.0, 1.5, 0.0), Vec3::Y), &c).is_none());
    }

    #[test]
    fn poke_hysteresis() {
        let mut p = PokeDetector::default();
        assert_eq!(p.update(0.05), (false, true));
        assert_eq!(p.update(0.004), (true, true));
        assert_eq!(
            p.update(0.010),
            (true, true),
            "stays pressed inside hysteresis band"
        );
        assert_eq!(p.update(0.02), (false, true));
        assert_eq!(p.update(0.2), (false, false));
    }

    #[test]
    fn stick_navigation_repeats() {
        let mut s = StickNavigator::default();
        assert_eq!(s.update(Vec2::new(0.0, -0.9), 0.016), Some(NavDir::Down));
        assert_eq!(s.update(Vec2::new(0.0, -0.9), 0.1), None);
        assert_eq!(s.update(Vec2::new(0.0, -0.9), 0.4), Some(NavDir::Down));
        assert_eq!(s.update(Vec2::new(0.0, -0.9), 0.13), Some(NavDir::Down));
        assert_eq!(s.update(Vec2::new(0.1, 0.0), 0.016), None);
        assert_eq!(s.update(Vec2::new(0.9, 0.2), 0.016), Some(NavDir::Right));
    }

    #[test]
    fn gaze_dimming() {
        let mut g = GazeDimmer::default();
        for _ in 0..60 {
            g.update(1.0 / 60.0, Some(false), false, false);
        }
        assert_eq!(g.opacity(), 1.0, "still within idle delay");
        for _ in 0..600 {
            g.update(1.0 / 60.0, Some(false), false, false);
        }
        assert!((g.opacity() - g.dim_opacity).abs() < 0.01);
        // Looking back brings it up quickly.
        for _ in 0..30 {
            g.update(1.0 / 60.0, Some(true), false, false);
        }
        assert!(g.opacity() > 0.95);
        // Without eye tracking, pointer presence counts as engagement.
        let mut g2 = GazeDimmer::default();
        for _ in 0..600 {
            g2.update(1.0 / 60.0, None, true, false);
        }
        assert_eq!(g2.opacity(), 1.0);
        g2.enabled = false;
        assert_eq!(g2.opacity(), 1.0);
    }
}
