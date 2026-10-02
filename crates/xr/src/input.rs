//! Controller, eye-gaze and hand-tracking input, sampled once per frame
//! into a plain [`InputState`] the app and UI consume.

use crate::bindings::{self, ActionId, ActionKind, Profile};
use crate::math::{Pose, Ray};
use crate::pinch::{PinchConfig, PinchDetector, PinchState};
use crate::XrError;
use glam::Vec2;
use openxr as xr;

/// Digital button with edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Button {
    pub pressed: bool,
    pub just_pressed: bool,
    pub just_released: bool,
    /// Capacitive touch (false where the hardware has none).
    pub touched: bool,
}

impl Button {
    pub fn update(&mut self, pressed: bool, touched: bool) {
        self.just_pressed = pressed && !self.pressed;
        self.just_released = !pressed && self.pressed;
        self.pressed = pressed;
        self.touched = touched || pressed;
    }
}

/// Analog → digital with hysteresis (press above `on`, release below `off`).
pub fn analog_press(was: bool, value: f32, on: f32, off: f32) -> bool {
    if was {
        value > off
    } else {
        value >= on
    }
}

/// Emulate a D-pad from a stick: `[up, down, left, right]`, only the
/// dominant axis, with `threshold` on its magnitude and hysteresis via `prev`.
pub fn stick_to_dpad(v: Vec2, prev: [bool; 4], threshold: f32) -> [bool; 4] {
    let release = threshold * 0.7;
    let horizontal = v.x.abs() > v.y.abs();
    let held = |i: usize, val: f32| {
        if prev[i] {
            val > release
        } else {
            val >= threshold
        }
    };
    let up = !horizontal && held(0, v.y);
    let down = !horizontal && held(1, -v.y);
    let left = horizontal && held(2, -v.x);
    let right = horizontal && held(3, v.x);
    [up, down, left, right]
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ControllerState {
    /// Bound and delivering input.
    pub active: bool,
    pub aim: Option<Pose>,
    pub grip: Option<Pose>,
    pub trigger: f32,
    /// Trigger click (or "select" on simple controllers); `touched` = trigger touch.
    pub select: Button,
    pub squeeze: f32,
    /// Squeeze as a button (with hysteresis).
    pub grip_button: Button,
    pub bumper: Button,
    pub thumbstick: Vec2,
    pub thumbstick_button: Button,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DPad {
    pub up: Button,
    pub down: Button,
    pub left: Button,
    pub right: Button,
}

/// One hand-tracking joint.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct JointPose {
    pub pose: Pose,
    pub radius: f32,
    pub valid: bool,
}

pub const JOINT_COUNT: usize = 26;
pub const JOINT_PALM: usize = 0;
pub const JOINT_WRIST: usize = 1;
pub const JOINT_THUMB_TIP: usize = 5;
pub const JOINT_INDEX_PROXIMAL: usize = 7;
pub const JOINT_INDEX_TIP: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HandState {
    pub tracked: bool,
    pub joints: [JointPose; JOINT_COUNT],
    pub pinch: PinchState,
    /// Pointing ray for hand-only interaction.
    pub aim: Option<Ray>,
}

impl Default for HandState {
    fn default() -> Self {
        HandState {
            tracked: false,
            joints: [JointPose::default(); JOINT_COUNT],
            pinch: PinchState::default(),
            aim: None,
        }
    }
}

impl HandState {
    /// Thumb/index tip data for the pinch detector.
    pub fn pinch_input(&self) -> Option<(glam::Vec3, f32, glam::Vec3, f32)> {
        let t = self.joints[JOINT_THUMB_TIP];
        let i = self.joints[JOINT_INDEX_TIP];
        (self.tracked && t.valid && i.valid).then_some((
            t.pose.position,
            t.radius,
            i.pose.position,
            i.radius,
        ))
    }

    /// Ray from the index knuckle along the palm's forward axis.
    // [verify] Feels right with Frame hand tracking; alternatives use a
    // shoulder-to-knuckle ray.
    pub fn compute_aim(&self) -> Option<Ray> {
        let palm = self.joints[JOINT_PALM];
        let knuckle = self.joints[JOINT_INDEX_PROXIMAL];
        (self.tracked && palm.valid && knuckle.valid).then(|| Ray {
            origin: knuckle.pose.position,
            dir: palm.pose.forward().normalize(),
        })
    }
}

/// Everything input-related for one frame.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct InputState {
    /// XR time the poses were located at (predicted display time).
    pub time_ns: i64,
    pub left: ControllerState,
    pub right: ControllerState,
    pub a: Button,
    pub b: Button,
    pub x: Button,
    pub y: Button,
    pub menu: Button,
    pub view: Button,
    /// Physical D-pad, or emulated from the left stick when unbound.
    pub dpad: DPad,
    /// Eye gaze pose in the app space (XR_EXT_eye_gaze_interaction).
    pub gaze: Option<Pose>,
    pub hands: [HandState; 2],
    /// Active interaction profile per hand (left, right).
    pub profiles: [Option<Profile>; 2],
}

impl InputState {
    pub fn controller(&self, hand: usize) -> &ControllerState {
        if hand == 0 {
            &self.left
        } else {
            &self.right
        }
    }
}

/// Typed OpenXR actions.
struct Actions {
    trigger: xr::Action<f32>,
    squeeze: xr::Action<f32>,
    thumbstick: xr::Action<xr::Vector2f>,
    aim: xr::Action<xr::Posef>,
    grip: xr::Action<xr::Posef>,
    haptic: xr::Action<xr::Haptic>,
    /// Bool actions indexed by `ActionId::index()`.
    bools: Vec<Option<xr::Action<bool>>>,
    gaze: Option<xr::Action<xr::Posef>>,
}

impl Actions {
    fn bool(&self, id: ActionId) -> &xr::Action<bool> {
        self.bools[id.index()].as_ref().expect("bool action")
    }

    fn binding(&self, id: ActionId, path: xr::Path) -> xr::Binding<'_> {
        match id.kind() {
            ActionKind::Bool => xr::Binding::new(self.bool(id), path),
            ActionKind::Float if id == ActionId::Trigger => xr::Binding::new(&self.trigger, path),
            ActionKind::Float => xr::Binding::new(&self.squeeze, path),
            ActionKind::Vec2 => xr::Binding::new(&self.thumbstick, path),
            ActionKind::Pose if id == ActionId::AimPose => xr::Binding::new(&self.aim, path),
            ActionKind::Pose => xr::Binding::new(&self.grip, path),
            ActionKind::Haptic => xr::Binding::new(&self.haptic, path),
        }
    }
}

/// Owns the action set, action spaces and (optional) hand trackers.
pub struct InputSystem {
    set: xr::ActionSet,
    actions: Actions,
    hand_paths: [xr::Path; 2],
    aim_spaces: Option<[xr::Space; 2]>,
    grip_spaces: Option<[xr::Space; 2]>,
    gaze_space: Option<xr::Space>,
    hand_trackers: Option<[xr::HandTracker; 2]>,
    pinch: [PinchDetector; 2],
    dpad_emul: [bool; 4],
}

impl InputSystem {
    /// Create actions and suggest bindings (must happen before the session
    /// attaches action sets). `eye_gaze` requires XR_EXT_eye_gaze_interaction.
    pub fn new(instance: &xr::Instance, eye_gaze: bool) -> Result<InputSystem, XrError> {
        let set = instance.create_action_set("frameplayer", "FramePlayer", 0)?;
        let hand_paths = [
            instance.string_to_path(bindings::LEFT)?,
            instance.string_to_path(bindings::RIGHT)?,
        ];
        let mk_bool = |id: ActionId| -> Result<xr::Action<bool>, XrError> {
            let (n, l) = id.names();
            let sub: &[xr::Path] = if id.per_hand() { &hand_paths } else { &[] };
            Ok(set.create_action::<bool>(n, l, sub)?)
        };
        let mut bools: Vec<Option<xr::Action<bool>>> = Vec::with_capacity(ActionId::ALL.len());
        for id in ActionId::ALL {
            bools.push(if id.kind() == ActionKind::Bool {
                Some(mk_bool(id)?)
            } else {
                None
            });
        }
        let named = |id: ActionId| id.names();
        let actions = Actions {
            trigger: set.create_action(
                named(ActionId::Trigger).0,
                named(ActionId::Trigger).1,
                &hand_paths,
            )?,
            squeeze: set.create_action(
                named(ActionId::Squeeze).0,
                named(ActionId::Squeeze).1,
                &hand_paths,
            )?,
            thumbstick: set.create_action(
                named(ActionId::Thumbstick).0,
                named(ActionId::Thumbstick).1,
                &hand_paths,
            )?,
            aim: set.create_action(
                named(ActionId::AimPose).0,
                named(ActionId::AimPose).1,
                &hand_paths,
            )?,
            grip: set.create_action(
                named(ActionId::GripPose).0,
                named(ActionId::GripPose).1,
                &hand_paths,
            )?,
            haptic: set.create_action(
                named(ActionId::Haptic).0,
                named(ActionId::Haptic).1,
                &hand_paths,
            )?,
            bools,
            gaze: if eye_gaze {
                Some(set.create_action("gaze_pose", "Eye gaze", &[])?)
            } else {
                None
            },
        };

        for profile in Profile::ALL {
            let profile_path = instance.string_to_path(profile.path())?;
            let mut done = false;
            for full in [true, false] {
                if done || (!full && profile != Profile::Frame) {
                    continue;
                }
                let list = bindings::bindings(profile, full);
                let mut paths = Vec::with_capacity(list.len());
                for (id, p) in &list {
                    paths.push((*id, instance.string_to_path(p)?));
                }
                let b: Vec<xr::Binding<'_>> = paths
                    .iter()
                    .map(|&(id, p)| actions.binding(id, p))
                    .collect();
                match instance.suggest_interaction_profile_bindings(profile_path, &b) {
                    Ok(()) => {
                        tracing::info!(
                            "suggested {} bindings for {} ({})",
                            b.len(),
                            profile.path(),
                            if full { "full" } else { "core" }
                        );
                        done = true;
                    }
                    Err(e) => tracing::warn!(
                        "bindings for {} ({}) rejected: {e}",
                        profile.path(),
                        if full { "full" } else { "core" }
                    ),
                }
            }
        }
        if let Some(g) = &actions.gaze {
            let profile = instance.string_to_path(bindings::EYE_GAZE_PROFILE)?;
            let path = instance.string_to_path(bindings::EYE_GAZE_POSE)?;
            if let Err(e) =
                instance.suggest_interaction_profile_bindings(profile, &[xr::Binding::new(g, path)])
            {
                tracing::warn!("eye gaze binding rejected: {e}");
            }
        }
        Ok(InputSystem {
            set,
            actions,
            hand_paths,
            aim_spaces: None,
            grip_spaces: None,
            gaze_space: None,
            hand_trackers: None,
            pinch: [PinchDetector::new(PinchConfig::default()); 2],
            dpad_emul: [false; 4],
        })
    }

    /// Attach to the session and create action spaces / hand trackers.
    pub fn attach<G>(
        &mut self,
        session: &xr::Session<G>,
        hand_tracking: bool,
    ) -> Result<(), XrError> {
        session.attach_action_sets(&[&self.set])?;
        let mk = |a: &xr::Action<xr::Posef>, p: xr::Path| {
            a.create_space(session.clone(), p, xr::Posef::IDENTITY)
        };
        self.aim_spaces = Some([
            mk(&self.actions.aim, self.hand_paths[0])?,
            mk(&self.actions.aim, self.hand_paths[1])?,
        ]);
        self.grip_spaces = Some([
            mk(&self.actions.grip, self.hand_paths[0])?,
            mk(&self.actions.grip, self.hand_paths[1])?,
        ]);
        if let Some(g) = &self.actions.gaze {
            self.gaze_space = Some(mk(g, xr::Path::NULL)?);
        }
        if hand_tracking {
            match (
                session.create_hand_tracker(xr::Hand::LEFT),
                session.create_hand_tracker(xr::Hand::RIGHT),
            ) {
                (Ok(l), Ok(r)) => self.hand_trackers = Some([l, r]),
                (Err(e), _) | (_, Err(e)) => tracing::warn!("hand tracking unavailable: {e}"),
            }
        }
        Ok(())
    }

    pub fn set_pinch_config(&mut self, c: PinchConfig) {
        for p in &mut self.pinch {
            p.config = c;
        }
    }

    /// Query the active interaction profile of each hand (after an
    /// `InteractionProfileChanged` event).
    pub fn refresh_profiles<G>(&self, session: &xr::Session<G>, state: &mut InputState) {
        for (h, path) in self.hand_paths.iter().enumerate() {
            state.profiles[h] = session
                .current_interaction_profile(*path)
                .ok()
                .filter(|p| *p != xr::Path::NULL)
                .and_then(|p| session.instance().path_to_string(p).ok())
                .and_then(|s| Profile::from_path(&s));
        }
    }

    /// Sample all input for `time` into `state`. Actions are only synced
    /// while focused; otherwise buttons read as released. No allocation.
    pub fn update<G>(
        &mut self,
        session: &xr::Session<G>,
        space: &xr::Space,
        time: xr::Time,
        focused: bool,
        state: &mut InputState,
    ) -> Result<(), XrError> {
        state.time_ns = time.as_nanos();
        let synced = focused
            && session
                .sync_actions(&[xr::ActiveActionSet::new(&self.set)])
                .is_ok();
        let a = &self.actions;
        let read_bool = |id: ActionId, sub: xr::Path| -> (bool, bool) {
            if !synced {
                return (false, false);
            }
            match a.bool(id).state(session, sub) {
                Ok(s) => (s.current_state, s.is_active),
                Err(_) => (false, false),
            }
        };
        let read_f32 = |act: &xr::Action<f32>, sub| {
            if synced {
                act.state(session, sub)
                    .map(|s| s.current_state)
                    .unwrap_or(0.0)
            } else {
                0.0
            }
        };

        for h in 0..2 {
            let sub = self.hand_paths[h];
            let c = if h == 0 {
                &mut state.left
            } else {
                &mut state.right
            };
            c.trigger = read_f32(&a.trigger, sub);
            c.squeeze = read_f32(&a.squeeze, sub);
            c.thumbstick = if synced {
                a.thumbstick
                    .state(session, sub)
                    .map(|s| Vec2::new(s.current_state.x, s.current_state.y))
                    .unwrap_or(Vec2::ZERO)
            } else {
                Vec2::ZERO
            };
            let (sel, sel_active) = read_bool(ActionId::TriggerClick, sub);
            // Fall back to an analog threshold where there is no click component.
            let sel = sel || (!sel_active && analog_press(c.select.pressed, c.trigger, 0.75, 0.6));
            c.select
                .update(sel, read_bool(ActionId::TriggerTouch, sub).0);
            let grip = analog_press(c.grip_button.pressed, c.squeeze, 0.7, 0.5);
            c.grip_button.update(grip, c.squeeze > 0.05);
            c.bumper.update(read_bool(ActionId::Bumper, sub).0, false);
            c.thumbstick_button.update(
                read_bool(ActionId::ThumbstickClick, sub).0,
                read_bool(ActionId::ThumbstickTouch, sub).0,
            );
            c.active = synced && a.aim.is_active(session, sub).unwrap_or(false);
            c.aim = locate(self.aim_spaces.as_ref().map(|s| &s[h]), space, time);
            c.grip = locate(self.grip_spaces.as_ref().map(|s| &s[h]), space, time);
        }

        let n = xr::Path::NULL;
        let pairs = [
            (&mut state.a, ActionId::ButtonA, ActionId::TouchA),
            (&mut state.b, ActionId::ButtonB, ActionId::TouchB),
            (&mut state.x, ActionId::ButtonX, ActionId::TouchX),
            (&mut state.y, ActionId::ButtonY, ActionId::TouchY),
        ];
        for (btn, press, touch) in pairs {
            btn.update(read_bool(press, n).0, read_bool(touch, n).0);
        }
        state.menu.update(read_bool(ActionId::Menu, n).0, false);
        state.view.update(read_bool(ActionId::View, n).0, false);

        let (up, up_active) = read_bool(ActionId::DpadUp, n);
        let dirs = if up_active {
            [
                up,
                read_bool(ActionId::DpadDown, n).0,
                read_bool(ActionId::DpadLeft, n).0,
                read_bool(ActionId::DpadRight, n).0,
            ]
        } else {
            self.dpad_emul = stick_to_dpad(state.left.thumbstick, self.dpad_emul, 0.7);
            self.dpad_emul
        };
        state.dpad.up.update(dirs[0], false);
        state.dpad.down.update(dirs[1], false);
        state.dpad.left.update(dirs[2], false);
        state.dpad.right.update(dirs[3], false);

        state.gaze = if synced {
            locate(self.gaze_space.as_ref(), space, time)
        } else {
            None
        };

        for h in 0..2 {
            let hs = &mut state.hands[h];
            hs.tracked = false;
            if let Some(trackers) = &self.hand_trackers {
                if let Ok(Some(joints)) = space.locate_hand_joints(&trackers[h], time) {
                    hs.tracked = true;
                    for (dst, j) in hs.joints.iter_mut().zip(joints.iter()) {
                        let f = j.location_flags;
                        dst.valid = f.contains(
                            xr::SpaceLocationFlags::POSITION_VALID
                                | xr::SpaceLocationFlags::ORIENTATION_VALID,
                        );
                        dst.pose = Pose::from_xr(&j.pose);
                        dst.radius = j.radius;
                    }
                }
            }
            hs.pinch = self.pinch[h].update(hs.pinch_input());
            hs.aim = hs.compute_aim();
        }
        Ok(())
    }

    /// Vibrate a controller (`hand` 0 = left). `duration_s ≤ 0` = minimum pulse.
    pub fn vibrate<G>(
        &self,
        session: &xr::Session<G>,
        hand: usize,
        amplitude: f32,
        duration_s: f32,
        frequency_hz: f32,
    ) -> Result<(), XrError> {
        let duration = if duration_s > 0.0 {
            xr::Duration::from_nanos((duration_s * 1e9) as i64)
        } else {
            xr::Duration::MIN_HAPTIC
        };
        let freq = frequency_hz.max(0.0); // 0 = XR_FREQUENCY_UNSPECIFIED
        let v = xr::HapticVibration::new()
            .amplitude(amplitude.clamp(0.0, 1.0))
            .duration(duration)
            .frequency(freq);
        self.actions
            .haptic
            .apply_feedback(session, self.hand_paths[hand.min(1)], &v)?;
        Ok(())
    }

    pub fn stop_vibration<G>(&self, session: &xr::Session<G>, hand: usize) -> Result<(), XrError> {
        self.actions
            .haptic
            .stop_feedback(session, self.hand_paths[hand.min(1)])?;
        Ok(())
    }

    pub fn has_eye_gaze(&self) -> bool {
        self.gaze_space.is_some()
    }

    pub fn has_hand_tracking(&self) -> bool {
        self.hand_trackers.is_some()
    }
}

/// Locate an action space; `None` unless both position and orientation are valid.
fn locate(space: Option<&xr::Space>, base: &xr::Space, time: xr::Time) -> Option<Pose> {
    let loc = space?.locate(base, time).ok()?;
    loc.location_flags
        .contains(
            xr::SpaceLocationFlags::POSITION_VALID | xr::SpaceLocationFlags::ORIENTATION_VALID,
        )
        .then(|| Pose::from_xr(&loc.pose))
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::{Quat, Vec3};

    #[test]
    fn button_edges() {
        let mut b = Button::default();
        b.update(true, false);
        assert!(b.pressed && b.just_pressed && b.touched);
        b.update(true, true);
        assert!(!b.just_pressed);
        b.update(false, true);
        assert!(b.just_released && b.touched && !b.pressed);
        b.update(false, false);
        assert!(!b.just_released && !b.touched);
    }

    #[test]
    fn analog_hysteresis() {
        assert!(!analog_press(false, 0.65, 0.7, 0.5));
        assert!(analog_press(false, 0.7, 0.7, 0.5));
        assert!(analog_press(true, 0.6, 0.7, 0.5));
        assert!(!analog_press(true, 0.5, 0.7, 0.5));
    }

    #[test]
    fn dpad_emulation() {
        let none = [false; 4];
        assert_eq!(
            stick_to_dpad(Vec2::new(0.0, 0.9), none, 0.7),
            [true, false, false, false]
        );
        assert_eq!(
            stick_to_dpad(Vec2::new(-0.9, 0.3), none, 0.7),
            [false, false, true, false]
        );
        assert_eq!(stick_to_dpad(Vec2::new(0.5, 0.2), none, 0.7), none);
        // Hysteresis: held right stays pressed at 0.6, released at 0.4.
        let right = [false, false, false, true];
        assert_eq!(stick_to_dpad(Vec2::new(0.6, 0.0), right, 0.7), right);
        assert_eq!(stick_to_dpad(Vec2::new(0.4, 0.0), right, 0.7), none);
        // Diagonal picks the dominant axis only.
        assert_eq!(
            stick_to_dpad(Vec2::new(0.75, -0.8), none, 0.7),
            [false, true, false, false]
        );
    }

    #[test]
    fn hand_pinch_and_aim() {
        let mut h = HandState {
            tracked: true,
            ..Default::default()
        };
        let j = |p: Vec3| JointPose {
            pose: Pose::new(p, Quat::IDENTITY),
            radius: 0.005,
            valid: true,
        };
        h.joints[JOINT_THUMB_TIP] = j(Vec3::new(0.0, 0.0, 0.0));
        h.joints[JOINT_INDEX_TIP] = j(Vec3::new(0.012, 0.0, 0.0));
        h.joints[JOINT_PALM] = j(Vec3::new(0.0, 0.0, 0.05));
        h.joints[JOINT_INDEX_PROXIMAL] = j(Vec3::new(0.02, 0.0, 0.0));
        let mut d = PinchDetector::default();
        assert!(d.update(h.pinch_input()).pinching, "2 mm gap pinches");
        let aim = h.compute_aim().unwrap();
        assert_eq!(aim.origin, Vec3::new(0.02, 0.0, 0.0));
        assert!((aim.dir - Vec3::NEG_Z).length() < 1e-6);
        h.tracked = false;
        assert!(h.pinch_input().is_none() && h.compute_aim().is_none());
    }
}
