//! Controller bindings. Each button does one thing; the right hand runs
//! playback and the left hand the picture. Play/pause is on both (A and the
//! D-pad's down), and the sticks seek on both, so the commonest actions are
//! under either thumb.
//!
//! | Input | Right hand | Left hand |
//! |---|---|---|
//! | Thumbstick left / right | Seek back / forward (repeats while held) | the same |
//! | Thumbstick up / down | Volume | Tilt the picture |
//! | Thumbstick press | Mute | Reset the picture |
//! | A / D-pad down | Play / pause | Play / pause |
//! | B / D-pad right | Back | (spare) |
//! | X / D-pad left | Previous video | Adjust panel |
//! | Y / D-pad up | Next video | Show or hide the controls |
//! | Menu / View | Library | Recenter |
//! | Bumper | Forward 1 minute | Back 1 minute |
//! | Grip + thumbstick | (nothing) | Left / right turns the picture, up / down zooms |
//! | Hold grip + trigger, move | Drag the picture | Drag the picture |
//! | Trigger | Click; on empty space, show or hide the controls | the same |
//!
//! Pointing at a list, thumbstick left / right pages it; both grips together
//! recenter too.

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

const LEFT: usize = 0;
const RIGHT: usize = 1;

/// How fast a thumbstick tilts or turns the picture, in degrees a second.
const PITCH_SPEED: f32 = 30.0;
const YAW_SPEED: f32 = 45.0;
/// Volume change a second at full stick.
const VOLUME_SPEED: f32 = 0.6;
const GRIP: f32 = 0.7;
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
    /// Direction held on the thumbstick (x axis) and for how long.
    held: [(i32, f32); 2],
    drag: Option<Drag>,
}

/// Yaw and pitch, degrees, of a controller's pointing direction in the
/// anchor's frame (-Z forward).
fn angles(aim: Quat, anchor: Quat) -> (f32, f32) {
    let d = (anchor.inverse() * aim * Vec3::NEG_Z).normalize_or_zero();
    (
        (-d.x).atan2(-d.z).to_degrees(),
        d.y.clamp(-1.0, 1.0).asin().to_degrees(),
    )
}

fn wrap(deg: f32) -> f32 {
    (deg + 180.0).rem_euclid(360.0) - 180.0
}

impl Controls {
    pub fn update(&mut self, hands: &[Hand; 2], anchor: Quat, ctx: Context) -> Vec<Cmd> {
        let mut out = Vec::new();
        let prev = self.prev;
        let pressed = |i: usize, f: fn(&Hand) -> bool| f(&hands[i]) && !f(&prev[i]);
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

        // Buttons: one action each, by hand.
        let playing = ctx.playing;
        if pressed(RIGHT, |h| h.south) && playing {
            out.push(Cmd::TogglePause);
        }
        if pressed(LEFT, |h| h.south) && playing {
            out.push(Cmd::TogglePause);
        }
        if pressed(RIGHT, |h| h.east) {
            out.push(Cmd::Back);
        }
        if pressed(RIGHT, |h| h.west) && playing {
            out.push(Cmd::Previous);
        }
        if pressed(RIGHT, |h| h.north) && playing {
            out.push(Cmd::Next);
        }
        if pressed(RIGHT, |h| h.menu) {
            out.push(Cmd::Menu);
        }
        if pressed(RIGHT, |h| h.shoulder) && playing {
            out.push(Cmd::Skip(1));
        }
        if pressed(RIGHT, |h| h.stick_click) && playing {
            out.push(Cmd::ToggleMute);
        }
        if pressed(LEFT, |h| h.north) && playing {
            out.push(Cmd::ToggleUi);
        }
        if pressed(LEFT, |h| h.west) && playing {
            out.push(Cmd::ToggleAdjust);
        }
        if pressed(LEFT, |h| h.menu) {
            out.push(Cmd::Recenter);
        }
        if pressed(LEFT, |h| h.shoulder) && playing {
            out.push(Cmd::Skip(-1));
        }
        if pressed(LEFT, |h| h.stick_click) && playing {
            out.push(Cmd::ResetImage);
        }

        for (i, hand) in hands.iter().enumerate() {
            let s = hand.stick;
            if gripping(hand) {
                // Left grip + stick: turn and zoom the picture.
                self.held[i] = (0, 0.0);
                if i == LEFT && playing && !ctx.over_ui {
                    if s.x.abs() > STICK_HOLD {
                        out.push(Cmd::Yaw(-s.x * ctx.dt * YAW_SPEED));
                    }
                    if s.y.abs() > STICK_HOLD {
                        // Down zooms in, as in DeoVR.
                        out.push(Cmd::Zoom(-s.y * ctx.dt * 0.8));
                    }
                }
                continue;
            }

            // Stick left/right: seek (or page a list), repeating while held.
            let dir = if s.x > STICK {
                1
            } else if s.x < -STICK {
                -1
            } else if s.x.abs() < STICK_RELEASE {
                0
            } else {
                self.held[i].0
            };
            let (was, t) = self.held[i];
            if dir == 0 {
                self.held[i] = (0, 0.0);
            } else if dir != was {
                self.held[i] = (dir, 0.0);
                out.push(if playing && !ctx.over_ui {
                    Cmd::Seek(dir)
                } else {
                    Cmd::Page(dir)
                });
            } else {
                let t2 = t + ctx.dt;
                // Fire each time the hold crosses delay + k * interval.
                let fires = |t: f32| {
                    if t < REPEAT_DELAY {
                        -1.0
                    } else {
                        ((t - REPEAT_DELAY) / REPEAT_EVERY).floor()
                    }
                };
                if fires(t2) > fires(t) && playing && !ctx.over_ui {
                    out.push(Cmd::Seek(dir));
                }
                self.held[i] = (dir, t2);
            }
            // Stick up/down: volume (right) or tilt (left). Menus pointed at
            // scroll through the pointer instead.
            if playing && !ctx.over_ui && s.y.abs() > STICK_HOLD && s.x.abs() < STICK_RELEASE {
                out.push(if i == RIGHT {
                    Cmd::Volume(s.y * ctx.dt * VOLUME_SPEED)
                } else {
                    Cmd::Pitch(s.y * ctx.dt * PITCH_SPEED)
                });
            }
        }

        let both = |h: &[Hand; 2]| h.iter().all(|h| h.squeeze > 0.8 && h.trigger < 0.3);
        if both(hands) && !both(&prev) && self.drag.is_none() {
            out.push(Cmd::Recenter);
        }
        if ctx.click_outside && !any_grip && playing {
            out.push(Cmd::ToggleUi);
        }
        self.prev = *hands;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        c.update(h, Quat::IDENTITY, cx)
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
        let cases: [(usize, Set, Cmd); 13] = [
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
            (LEFT, |h, v| h.menu = v, Cmd::Recenter),
            (LEFT, |h, v| h.shoulder = v, Cmd::Skip(-1)),
            (LEFT, |h, v| h.stick_click = v, Cmd::ResetImage),
        ];
        for (hand, set, want) in cases {
            assert_eq!(press(hand, set, true), vec![want], "{want:?}");
        }
        assert_eq!(press(LEFT, |h, v| h.east = v, true), vec![], "spare");
        // In the library only navigation works.
        assert_eq!(press(RIGHT, |h, v| h.east = v, false), vec![Cmd::Back]);
        assert_eq!(press(RIGHT, |h, v| h.menu = v, false), vec![Cmd::Menu]);
        assert_eq!(press(RIGHT, |h, v| h.south = v, false), vec![]);
        assert_eq!(press(LEFT, |h, v| h.shoulder = v, false), vec![]);
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
