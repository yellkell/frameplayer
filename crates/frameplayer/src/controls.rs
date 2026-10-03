//! Controller bindings, modelled on DeoVR's Quest defaults so its users feel
//! at home:
//!
//! | Input | Action |
//! |---|---|
//! | A / X | Play / pause |
//! | B / Y | Back (close a panel, leave the library, show the library) |
//! | Thumbstick left / right | Seek back / forward (repeats while held); pages the library when pointing at it |
//! | Right thumbstick up / down | Tilt the picture up / down (pitch) |
//! | Left thumbstick up / down | Volume |
//! | Thumbstick press | Reset the image (zoom and dome drag) |
//! | Hold grip + trigger, move | Drag the dome (rotate / move the picture) |
//! | Grip + thumbstick right / left | Next / previous video |
//! | Grip + thumbstick down / up | Zoom in / out |
//! | Trigger | Click; on empty space, show or hide the controls |
//! | Menu | Library |
//! | Both grips (no trigger) | Recenter |

use fp_xr::Hand;
use glam::{Quat, Vec3};

/// What the bindings asked for this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Cmd {
    TogglePause,
    Back,
    Menu,
    /// -1 back, +1 forward.
    Seek(i32),
    /// Volume change (fraction per frame).
    Volume(f32),
    /// Picture pitch change (degrees per frame; up positive).
    Pitch(f32),
    /// Zoom change (factor delta per frame).
    Zoom(f32),
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

/// How fast the right thumbstick tilts the picture, in degrees a second.
const PITCH_SPEED: f32 = 30.0;
const GRIP: f32 = 0.7;
const TRIGGER: f32 = 0.7;
const STICK: f32 = 0.75;
const STICK_RELEASE: f32 = 0.35;
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
    /// Grip + thumbstick x already fired until the stick returns.
    grip_latch: [bool; 2],
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
            }
            None => {}
        }
        let any_grip = hands.iter().any(gripping);

        for (i, hand) in hands.iter().enumerate() {
            if pressed(i, |h| h.primary) && ctx.playing {
                out.push(Cmd::TogglePause);
            }
            if pressed(i, |h| h.secondary) {
                out.push(Cmd::Back);
            }
            if pressed(i, |h| h.menu) {
                out.push(Cmd::Menu);
            }
            if pressed(i, |h| h.stick_click) && ctx.playing {
                out.push(Cmd::ResetImage);
            }

            let s = hand.stick;
            if gripping(hand) {
                // Grip + stick: next/previous video, zoom.
                self.held[i] = (0, 0.0);
                if ctx.playing {
                    if s.x.abs() > STICK && !self.grip_latch[i] {
                        self.grip_latch[i] = true;
                        out.push(if s.x > 0.0 { Cmd::Next } else { Cmd::Previous });
                    } else if s.x.abs() < STICK_RELEASE {
                        self.grip_latch[i] = false;
                    }
                    if s.y.abs() > 0.3 && s.x.abs() < STICK_RELEASE {
                        // Down zooms in, as in DeoVR.
                        out.push(Cmd::Zoom(-s.y * ctx.dt * 0.8));
                    }
                }
                continue;
            }
            self.grip_latch[i] = false;

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
                out.push(if ctx.over_ui {
                    Cmd::Page(dir)
                } else if ctx.playing {
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
                if fires(t2) > fires(t) && ctx.playing && !ctx.over_ui {
                    out.push(Cmd::Seek(dir));
                }
                self.held[i] = (dir, t2);
            }
            // Stick up/down: the right hand tilts the picture, the left one
            // sets the volume (menus scroll through the pointer instead).
            if ctx.playing && !ctx.over_ui && s.y.abs() > 0.5 && s.x.abs() < STICK_RELEASE {
                out.push(if i == 1 {
                    Cmd::Pitch(s.y * ctx.dt * PITCH_SPEED)
                } else {
                    Cmd::Volume(s.y * ctx.dt * 0.6)
                });
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

    #[test]
    fn buttons() {
        let mut c = Controls::default();
        let mut h = hands();
        h[1].primary = true;
        assert_eq!(run(&mut c, &h, ctx(true)), vec![Cmd::TogglePause]);
        assert_eq!(run(&mut c, &h, ctx(true)), vec![], "edge only");
        h[1].primary = false;
        h[0].secondary = true;
        assert_eq!(run(&mut c, &h, ctx(true)), vec![Cmd::Back]);
        h[0].secondary = false;
        h[1].stick_click = true;
        assert_eq!(run(&mut c, &h, ctx(true)), vec![Cmd::ResetImage]);
        h[1].stick_click = false;
        h[0].menu = true;
        assert_eq!(run(&mut c, &h, ctx(false)), vec![Cmd::Menu]);
    }

    #[test]
    fn seek_repeats_while_held_and_pages_menus() {
        let mut c = Controls::default();
        let mut h = hands();
        h[1].stick = Vec2::new(0.9, 0.0);
        let mut seeks = 0;
        // 1.5 s held at 10 Hz: the first press, then repeats from 0.5 s every 0.35 s.
        for _ in 0..15 {
            seeks += run(&mut c, &h, ctx(true))
                .iter()
                .filter(|c| **c == Cmd::Seek(1))
                .count();
        }
        assert_eq!(seeks, 1 + 3);
        h[1].stick = Vec2::ZERO;
        run(&mut c, &h, ctx(true));
        h[1].stick = Vec2::new(-0.9, 0.0);
        let over = Context {
            over_ui: true,
            ..ctx(true)
        };
        assert_eq!(run(&mut c, &h, over), vec![Cmd::Page(-1)]);
    }

    #[test]
    fn right_stick_tilts_left_stick_sets_volume() {
        let mut c = Controls::default();
        let mut h = hands();
        h[1].stick = Vec2::new(0.0, 1.0);
        let up = run(&mut c, &h, ctx(true));
        assert!(
            matches!(up[..], [Cmd::Pitch(d)] if d > 0.0),
            "right up tilts up: {up:?}"
        );
        h[1].stick = Vec2::ZERO;
        h[0].stick = Vec2::new(0.0, -1.0);
        let down = run(&mut c, &h, ctx(true));
        assert!(
            matches!(down[..], [Cmd::Volume(d)] if d < 0.0),
            "left down lowers the volume: {down:?}"
        );
    }

    #[test]
    fn grip_and_stick_change_video_and_zoom() {
        let mut c = Controls::default();
        let mut h = hands();
        h[0].squeeze = 1.0;
        h[0].stick = Vec2::new(0.9, 0.0);
        assert_eq!(run(&mut c, &h, ctx(true)), vec![Cmd::Next]);
        assert_eq!(run(&mut c, &h, ctx(true)), vec![], "once per push");
        h[0].stick = Vec2::new(0.0, -1.0);
        let z = run(&mut c, &h, ctx(true));
        assert!(
            matches!(z[..], [Cmd::Zoom(d)] if d > 0.0),
            "down zooms in: {z:?}"
        );
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
