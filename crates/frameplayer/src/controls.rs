//! Controller input to commands. What each button and thumbstick axis does
//! comes from [`Bindings`] (remappable in Settings > Controller); the
//! defaults:
//!
//! | Input | Right hand | Left hand |
//! |---|---|---|
//! | Thumbstick left / right | Seek back / forward (repeats while held) | the same |
//! | Thumbstick up / down | Volume | Tilt the picture |
//! | Thumbstick press | Mute | Reset the picture |
//! | A / D-pad down | Play / pause | Play / pause |
//! | B / D-pad right | Back | Passthrough on / off |
//! | X / D-pad left | Previous video | Adjust panel |
//! | Y / D-pad up | Next video | Show or hide the controls |
//! | Menu / View | Library | Recenter |
//! | Bumper | Forward 1 minute | Back 1 minute |
//! | Grip + thumbstick | (nothing) | Left / right turns the picture, up / down zooms |
//!
//! Fixed: the trigger clicks (on empty space it shows or hides the
//! controls), grip + trigger drags the picture, both grips recenter, and a
//! thumbstick pointed at a list pages it left / right.

use crate::bindings::{Axis, AxisAction, Bindings, Button, ButtonAction};
use fp_xr::Hand;
use glam::{Quat, Vec3};

/// What the bindings asked for this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Cmd {
    TogglePause,
    Back,
    Menu,
    /// -1 back, +1 forward, by the seek step.
    Seek(i32),
    /// -1 back, +1 forward, by a minute.
    Skip(i32),
    /// Picture pitch change (degrees this frame; up positive).
    Pitch(f32),
    /// Picture yaw change (degrees this frame; left positive).
    Yaw(f32),
    /// Zoom change (factor delta this frame).
    Zoom(f32),
    /// Volume change (this frame; 1.0 is full).
    Volume(f32),
    ToggleMute,
    Next,
    Previous,
    ResetImage,
    DragBegin,
    /// Degrees moved since the drag began (yaw left-positive, pitch up-positive).
    DragTo {
        yaw: f32,
        pitch: f32,
    },
    DragEnd,
    Recenter,
    ToggleUi,
    ToggleAdjust,
    TogglePassthrough,
    /// Page the pointed-at list: -1 back, +1 forward.
    Page(i32),
}

/// What the app knows that changes how input is read.
#[derive(Clone, Copy, Debug, Default)]
pub struct Context {
    /// The active hand points at a panel.
    pub over_ui: bool,
    /// The active hand pulled the trigger while pointing at nothing.
    pub click_outside: bool,
    /// A video is open.
    pub playing: bool,
    pub dt: f32,
}

/// How fast a thumbstick tilts or turns the picture, in degrees a second.
const PITCH_SPEED: f32 = 30.0;
const YAW_SPEED: f32 = 45.0;
/// Volume change a second at full stick.
const VOLUME_SPEED: f32 = 0.6;
pub(crate) const GRIP: f32 = 0.7;
const TRIGGER: f32 = 0.7;
const STICK: f32 = 0.75;
const STICK_RELEASE: f32 = 0.35;
/// Stick deflection before a continuous action (tilt, volume...) starts.
const STICK_HOLD: f32 = 0.5;
const REPEAT_DELAY: f32 = 0.5;
const REPEAT_EVERY: f32 = 0.35;

#[derive(Clone, Copy, Debug)]
struct Drag {
    hand: usize,
    start: (f32, f32),
}

#[derive(Default)]
pub struct Controls {
    prev: [Hand; 2],
    /// Per hand and axis: the direction held (seeking or paging) and for how long.
    held: [[(i32, f32); 4]; 2],
    drag: Option<Drag>,
}

/// Yaw and pitch, degrees, of a controller's pointing direction in the
/// anchor's frame (-Z forward).
pub(crate) fn angles(aim: Quat, anchor: Quat) -> (f32, f32) {
    let d = (anchor.inverse() * aim * Vec3::NEG_Z).normalize_or_zero();
    (
        (-d.x).atan2(-d.z).to_degrees(),
        d.y.clamp(-1.0, 1.0).asin().to_degrees(),
    )
}

pub(crate) fn wrap(deg: f32) -> f32 {
    (deg + 180.0).rem_euclid(360.0) - 180.0
}

impl Controls {
    pub fn update(
        &mut self,
        hands: &[Hand; 2],
        anchor: Quat,
        ctx: Context,
        bindings: &Bindings,
    ) -> Vec<Cmd> {
        let mut out = Vec::new();
        let prev = self.prev;
        let gripping = |h: &Hand| h.squeeze > GRIP;

        // Dome drag: grip held, then trigger, on a hand not pointing at a panel.
        match self.drag {
            Some(d) => {
                let h = &hands[d.hand];
                if gripping(h) && h.trigger > TRIGGER * 0.5 {
                    if let Some((_, rot)) = h.aim {
                        let (yaw, pitch) = angles(rot, anchor);
                        out.push(Cmd::DragTo {
                            yaw: wrap(yaw - d.start.0),
                            pitch: pitch - d.start.1,
                        });
                    }
                } else {
                    self.drag = None;
                    out.push(Cmd::DragEnd);
                }
            }
            None if ctx.playing && !ctx.over_ui => {
                for i in 0..2 {
                    let h = &hands[i];
                    if gripping(h)
                        && h.trigger > TRIGGER
                        && prev[i].trigger <= TRIGGER
                        && let Some((_, rot)) = h.aim
                    {
                        self.drag = Some(Drag {
                            hand: i,
                            start: angles(rot, anchor),
                        });
                        out.push(Cmd::DragBegin);
                        break;
                    }
                }
            }
            None => {}
        }
        let any_grip = hands.iter().any(gripping);

        let playing = ctx.playing;
        for (i, hand) in hands.iter().enumerate() {
            let map = bindings.hand(i);
            for b in Button::ALL {
                if b.pressed(hand)
                    && !b.pressed(&prev[i])
                    && let Some(cmd) = button_cmd(map.button(b), playing)
                {
                    out.push(cmd);
                }
            }

            let s = hand.stick;
            let grip = gripping(hand);
            if !grip && (!playing || ctx.over_ui) {
                // Menus: left / right pages the list pointed at (up / down
                // scrolls it, through the pointer).
                if let Some(dir) = self.step(i, Axis::StickX, s.x, ctx.dt, false) {
                    out.push(Cmd::Page(dir));
                }
                continue;
            }
            let (ax, ay, other) = if grip {
                (Axis::GripX, Axis::GripY, [Axis::StickX, Axis::StickY])
            } else {
                (Axis::StickX, Axis::StickY, [Axis::GripX, Axis::GripY])
            };
            for a in other {
                self.held[i][a.index()] = (0, 0.0);
            }
            for (axis, v, cross) in [(ax, s.x, s.y), (ay, s.y, s.x)] {
                match map.axis(axis) {
                    AxisAction::Nothing => {}
                    AxisAction::Seek => {
                        if let Some(dir) = self.step(i, axis, v, ctx.dt, true) {
                            out.push(Cmd::Seek(dir));
                        }
                    }
                    // Continuous: only along the axis the stick mostly points.
                    action if v.abs() > STICK_HOLD && cross.abs() < v.abs() => {
                        let d = v * ctx.dt;
                        out.push(match action {
                            AxisAction::Volume => Cmd::Volume(d * VOLUME_SPEED),
                            AxisAction::Tilt => Cmd::Pitch(d * PITCH_SPEED),
                            AxisAction::Turn => Cmd::Yaw(-d * YAW_SPEED),
                            // Down zooms in, as in DeoVR.
                            _ => Cmd::Zoom(-d * 0.8),
                        });
                    }
                    _ => {}
                }
            }
        }

        let both = |h: &[Hand; 2]| h.iter().all(|h| h.squeeze > 0.8 && h.trigger < 0.3);
        if both(hands) && !both(&prev) && self.drag.is_none() {
            out.push(Cmd::Recenter);
        }
        if ctx.click_outside && !any_grip && ctx.playing {
            out.push(Cmd::ToggleUi);
        }
        self.prev = *hands;
        out
    }

    /// Edge-triggered stick direction on `axis` of hand `i`: a push fires
    /// once, and with `repeat` again while held (after a delay).
    fn step(&mut self, i: usize, axis: Axis, v: f32, dt: f32, repeat: bool) -> Option<i32> {
        let held = &mut self.held[i][axis.index()];
        let dir = if v > STICK {
            1
        } else if v < -STICK {
            -1
        } else if v.abs() < STICK_RELEASE {
            0
        } else {
            held.0
        };
        let (was, t) = *held;
        if dir == 0 {
            *held = (0, 0.0);
            return None;
        }
        if dir != was {
            *held = (dir, 0.0);
            return Some(dir);
        }
        let t2 = t + dt;
        *held = (dir, t2);
        // Fire each time the hold crosses delay + k * interval.
        let fires = |t: f32| {
            if t < REPEAT_DELAY {
                -1.0
            } else {
                ((t - REPEAT_DELAY) / REPEAT_EVERY).floor()
            }
        };
        (repeat && fires(t2) > fires(t)).then_some(dir)
    }
}

/// The command for a pressed button, if it does anything now.
fn button_cmd(action: ButtonAction, playing: bool) -> Option<Cmd> {
    use ButtonAction as B;
    let cmd = match action {
        B::Nothing => return None,
        B::Back => Cmd::Back,
        B::Library => Cmd::Menu,
        B::Passthrough => Cmd::TogglePassthrough,
        B::Recenter => Cmd::Recenter,
        _ if !playing => return None,
        B::PlayPause => Cmd::TogglePause,
        B::PreviousVideo => Cmd::Previous,
        B::NextVideo => Cmd::Next,
        B::SeekBack => Cmd::Seek(-1),
        B::SeekForward => Cmd::Seek(1),
        B::BackOneMinute => Cmd::Skip(-1),
        B::ForwardOneMinute => Cmd::Skip(1),
        B::Mute => Cmd::ToggleMute,
        B::ResetPicture => Cmd::ResetImage,
        B::ShowHideControls => Cmd::ToggleUi,
        B::AdjustPanel => Cmd::ToggleAdjust,
    };
    Some(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bindings::{LEFT, RIGHT};
    use glam::Vec2;

    fn ctx(playing: bool) -> Context {
        Context {
            playing,
            dt: 0.1,
            ..Default::default()
        }
    }

    fn hands() -> [Hand; 2] {
        let mut h = [Hand::default(), Hand::default()];
        for x in &mut h {
            x.active = true;
            x.aim = Some((Vec3::ZERO, Quat::IDENTITY));
        }
        h
    }

    fn run(c: &mut Controls, h: &[Hand; 2], cx: Context) -> Vec<Cmd> {
        c.update(h, Quat::IDENTITY, cx, &Bindings::default())
    }

    /// Presses and releases one button, returning what the press did.
    fn press(hand: usize, set: fn(&mut Hand, bool), playing: bool) -> Vec<Cmd> {
        let mut c = Controls::default();
        let mut h = hands();
        set(&mut h[hand], true);
        let out = run(&mut c, &h, ctx(playing));
        assert_eq!(run(&mut c, &h, ctx(playing)), vec![], "edge only");
        set(&mut h[hand], false);
        run(&mut c, &h, ctx(playing));
        out
    }

    #[test]
    fn every_button_has_one_job() {
        type Set = fn(&mut Hand, bool);
        let cases: [(usize, Set, Cmd); 14] = [
            (RIGHT, |h, v| h.south = v, Cmd::TogglePause),
            (LEFT, |h, v| h.south = v, Cmd::TogglePause),
            (RIGHT, |h, v| h.east = v, Cmd::Back),
            (RIGHT, |h, v| h.west = v, Cmd::Previous),
            (RIGHT, |h, v| h.north = v, Cmd::Next),
            (RIGHT, |h, v| h.menu = v, Cmd::Menu),
            (RIGHT, |h, v| h.shoulder = v, Cmd::Skip(1)),
            (RIGHT, |h, v| h.stick_click = v, Cmd::ToggleMute),
            (LEFT, |h, v| h.north = v, Cmd::ToggleUi),
            (LEFT, |h, v| h.west = v, Cmd::ToggleAdjust),
            (LEFT, |h, v| h.east = v, Cmd::TogglePassthrough),
            (LEFT, |h, v| h.menu = v, Cmd::Recenter),
            (LEFT, |h, v| h.shoulder = v, Cmd::Skip(-1)),
            (LEFT, |h, v| h.stick_click = v, Cmd::ResetImage),
        ];
        for (hand, set, want) in cases {
            assert_eq!(press(hand, set, true), vec![want], "{want:?}");
        }
        // In the library only navigation works.
        assert_eq!(press(RIGHT, |h, v| h.east = v, false), vec![Cmd::Back]);
        assert_eq!(press(RIGHT, |h, v| h.menu = v, false), vec![Cmd::Menu]);
        assert_eq!(press(RIGHT, |h, v| h.south = v, false), vec![]);
        assert_eq!(press(LEFT, |h, v| h.shoulder = v, false), vec![]);
        assert_eq!(
            press(LEFT, |h, v| h.east = v, false),
            vec![Cmd::TogglePassthrough],
            "passthrough works in the library too"
        );
    }

    #[test]
    fn either_stick_seeks_and_repeats_while_held() {
        for hand in [LEFT, RIGHT] {
            let mut c = Controls::default();
            let mut h = hands();
            h[hand].stick = Vec2::new(0.9, 0.0);
            let mut seeks = 0;
            // 1.5 s held at 10 Hz: the first press, then repeats from 0.5 s every 0.35 s.
            for _ in 0..15 {
                seeks += run(&mut c, &h, ctx(true))
                    .iter()
                    .filter(|c| **c == Cmd::Seek(1))
                    .count();
            }
            assert_eq!(seeks, 1 + 3, "hand {hand}");
            h[hand].stick = Vec2::ZERO;
            run(&mut c, &h, ctx(true));
            h[hand].stick = Vec2::new(-0.9, 0.0);
            let over = Context {
                over_ui: true,
                ..ctx(true)
            };
            assert_eq!(run(&mut c, &h, over), vec![Cmd::Page(-1)]);
        }
    }

    #[test]
    fn right_stick_up_is_volume_left_is_tilt() {
        let mut c = Controls::default();
        let mut h = hands();
        h[RIGHT].stick = Vec2::new(0.0, 1.0);
        let up = run(&mut c, &h, ctx(true));
        assert!(matches!(up[..], [Cmd::Volume(d)] if d > 0.0), "{up:?}");
        h[RIGHT].stick = Vec2::ZERO;
        h[LEFT].stick = Vec2::new(0.0, -1.0);
        let down = run(&mut c, &h, ctx(true));
        assert!(matches!(down[..], [Cmd::Pitch(d)] if d < 0.0), "{down:?}");
        // Pointing at a menu, up/down scrolls it instead (through the pointer).
        let over = Context {
            over_ui: true,
            ..ctx(true)
        };
        assert_eq!(run(&mut c, &h, over), vec![]);
    }

    #[test]
    fn left_grip_and_stick_turn_and_zoom() {
        let mut c = Controls::default();
        let mut h = hands();
        h[LEFT].squeeze = 1.0;
        h[LEFT].stick = Vec2::new(0.9, 0.0);
        let turn = run(&mut c, &h, ctx(true));
        assert!(
            matches!(turn[..], [Cmd::Yaw(d)] if d < 0.0),
            "right turns it right: {turn:?}"
        );
        h[LEFT].stick = Vec2::new(0.0, -1.0);
        let z = run(&mut c, &h, ctx(true));
        assert!(
            matches!(z[..], [Cmd::Zoom(d)] if d > 0.0),
            "down zooms in: {z:?}"
        );
        // The right grip + stick does nothing (and does not seek).
        let mut h = hands();
        h[RIGHT].squeeze = 1.0;
        h[RIGHT].stick = Vec2::new(0.9, 0.0);
        assert_eq!(run(&mut Controls::default(), &h, ctx(true)), vec![]);
    }

    #[test]
    fn remapped_buttons_and_axes_follow_the_bindings() {
        let mut b = Bindings::default();
        b.right.south = ButtonAction::NextVideo;
        b.left.stick_x = AxisAction::Turn;
        b.right.stick_y = AxisAction::Nothing;
        let mut c = Controls::default();
        let mut h = hands();
        h[RIGHT].south = true;
        assert_eq!(c.update(&h, Quat::IDENTITY, ctx(true), &b), vec![Cmd::Next]);
        let mut h = hands();
        h[LEFT].stick = Vec2::new(-0.9, 0.0);
        h[RIGHT].stick = Vec2::new(0.0, 0.9);
        let out = c.update(&h, Quat::IDENTITY, ctx(true), &b);
        assert!(
            matches!(out[..], [Cmd::Yaw(d)] if d > 0.0),
            "left turns left: {out:?}"
        );
        // Pointed at a menu, the stick pages whatever it is bound to.
        let over = Context {
            over_ui: true,
            ..ctx(true)
        };
        let mut c = Controls::default();
        assert_eq!(c.update(&h, Quat::IDENTITY, over, &b), vec![Cmd::Page(-1)]);
    }

    #[test]
    fn grip_trigger_drags_the_dome() {
        let mut c = Controls::default();
        let mut h = hands();
        h[1].squeeze = 1.0;
        assert_eq!(run(&mut c, &h, ctx(true)), vec![]);
        h[1].trigger = 1.0;
        let click = Context {
            click_outside: true,
            ..ctx(true)
        };
        assert_eq!(
            run(&mut c, &h, click),
            vec![Cmd::DragBegin],
            "no UI toggle while gripping"
        );
        // Point 20 degrees to the right and 10 up.
        h[1].aim = Some((
            Vec3::ZERO,
            Quat::from_euler(
                glam::EulerRot::YXZ,
                (-20f32).to_radians(),
                10f32.to_radians(),
                0.0,
            ),
        ));
        match run(&mut c, &h, ctx(true))[..] {
            [Cmd::DragTo { yaw, pitch }] => {
                assert!(
                    (yaw + 20.0).abs() < 0.01 && (pitch - 10.0).abs() < 0.01,
                    "{yaw} {pitch}"
                );
            }
            ref o => panic!("{o:?}"),
        }
        h[1].trigger = 0.0;
        assert_eq!(run(&mut c, &h, ctx(true)), vec![Cmd::DragEnd]);
    }

    #[test]
    fn both_grips_recenter_and_empty_clicks_toggle() {
        let mut c = Controls::default();
        let mut h = hands();
        h[0].squeeze = 1.0;
        h[1].squeeze = 1.0;
        assert_eq!(run(&mut c, &h, ctx(false)), vec![Cmd::Recenter]);
        let mut h = hands();
        h[1].trigger = 1.0;
        let click = Context {
            click_outside: true,
            ..ctx(true)
        };
        assert_eq!(run(&mut c, &h, click), vec![Cmd::ToggleUi]);
    }
}
