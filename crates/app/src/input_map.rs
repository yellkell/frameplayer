//! Frame controller mapping: raw per-frame [`InputState`] → app actions and
//! UI focus navigation. Pure (no XR calls) and table-driven so it can be
//! unit-tested and documented in one place.
//!
//! | Input                          | Over the UI / UI focused          | Watching (not pointing at UI)          |
//! |--------------------------------|-----------------------------------|----------------------------------------|
//! | Trigger (either hand)          | select (laser pointer press)      | show/hide controls                     |
//! | A                              | activate focused widget           | play / pause                           |
//! | B                              | back                              | back (hide controls, then leave)       |
//! | Right stick ←/→                | focus navigation (or scroll)      | seek ∓/± seek step (repeats when held) |
//! | Right stick ↑/↓                | focus navigation (or scroll)      | speed up / down one step               |
//! | Grip + right stick             | —                                 | ↑/↓ screen distance, ←/→ screen size   |
//! | Menu (short)                   | toggle controls / settings        | toggle controls                        |
//! | Menu or stick click (long)     | recenter                          | recenter                               |
//! | D-pad (left)                   | focus navigation                  | ←/→ chapter (frame step when paused)   |
//! | X                              | —                                 | picture adjust                         |
//! | Y / View                       | settings                          | settings                               |
//!
//! Hand-tracking pinch and eye gaze are pointer sources handled by the
//! frame loop (they only ever target the UI).

use fp_ui::{NavDir, NavInput, StickNavigator};
use fp_xr::{Button, InputState};
use glam::Vec2;

/// Seconds a button must be held to count as a long press.
pub const LONG_PRESS_S: f32 = 0.6;
/// Stick deflection that counts as a direction.
pub const STICK_THRESHOLD: f32 = 0.6;
/// Interval between repeated seeks while the stick is held.
pub const SEEK_REPEAT_S: f32 = 0.45;
/// Grip squeeze that switches the stick to screen adjustment.
pub const GRIP_THRESHOLD: f32 = 0.6;
/// Screen adjustment speed at full deflection (m/s).
pub const SCREEN_DISTANCE_RATE: f32 = 1.5;
pub const SCREEN_SIZE_RATE: f32 = 2.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputAction {
    /// Any button: resets the controls auto-hide timer.
    Activity,
    TogglePlay,
    /// Seek by ± one configured seek step.
    SeekStep(i32),
    SpeedStep(i32),
    Back,
    ToggleControls,
    Recenter,
    /// Metres to add to the flat screen's distance / width this frame.
    AdjustScreen {
        distance: f32,
        size: f32,
    },
    OpenSettings,
    OpenPictureAdjust,
    FrameStep(i32),
    Chapter(i32),
}

/// What the app is showing, as far as input routing cares.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct InputContext {
    /// A laser / hand ray currently hits the UI panel.
    pub pointing_at_ui: bool,
    /// The UI panel is shown (library, settings, or player controls).
    pub ui_visible: bool,
    /// The player screen is active (controls may be hidden).
    pub in_player: bool,
    /// Playback is paused (D-pad steps frames instead of chapters).
    pub paused: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MappedInput {
    pub actions: Vec<InputAction>,
    /// Focus navigation for the UI (`Ui::begin_frame`).
    pub nav: NavInput,
}

#[derive(Debug, Clone, Copy, Default)]
struct LongPress {
    held: f32,
    fired: bool,
}

impl LongPress {
    /// Returns `(short_release, long_fire)` for this frame.
    fn update(&mut self, b: &Button, dt: f32) -> (bool, bool) {
        if b.just_pressed {
            *self = LongPress::default();
        }
        if b.pressed {
            self.held += dt;
            if !self.fired && self.held >= LONG_PRESS_S {
                self.fired = true;
                return (false, true);
            }
            return (false, false);
        }
        let short = b.just_released && !self.fired;
        if b.just_released {
            *self = LongPress::default();
        }
        (short, false)
    }
}

/// Stateful mapper (long presses, auto-repeat, stick edges).
#[derive(Debug, Clone, Default)]
pub struct InputMapper {
    menu: LongPress,
    stick_click: LongPress,
    navigator: StickNavigator,
    seek_dir: i32,
    seek_timer: f32,
    speed_latched: bool,
}

impl InputMapper {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, input: &InputState, ctx: &InputContext, dt: f32) -> MappedInput {
        let mut out = MappedInput::default();
        let r = &input.right;
        let any_button = [
            &input.a,
            &input.b,
            &input.x,
            &input.y,
            &input.menu,
            &input.view,
            &input.left.select,
            &input.right.select,
            &input.dpad.up,
            &input.dpad.down,
            &input.dpad.left,
            &input.dpad.right,
        ]
        .iter()
        .any(|b| b.just_pressed);
        if any_button || r.thumbstick.length() > STICK_THRESHOLD {
            out.actions.push(InputAction::Activity);
        }

        // Long presses first so a long press never also counts as short.
        let (menu_short, menu_long) = self.menu.update(&input.menu, dt);
        let (_, click_long) = self.stick_click.update(&r.thumbstick_button, dt);
        if menu_long || click_long {
            out.actions.push(InputAction::Recenter);
        }
        if menu_short {
            out.actions.push(if ctx.in_player {
                InputAction::ToggleControls
            } else {
                InputAction::OpenSettings
            });
        }

        // B: the UI consumes Back itself when it is visible.
        if input.b.just_pressed {
            if ctx.ui_visible {
                out.nav.back = true;
            } else {
                out.actions.push(InputAction::Back);
            }
        }

        // A: activate focus in menus, play/pause while watching.
        if input.a.just_pressed && !ctx.pointing_at_ui {
            if ctx.in_player {
                out.actions.push(InputAction::TogglePlay);
            } else if ctx.ui_visible {
                out.nav.activate = true;
            }
        }

        // Trigger away from the UI while watching toggles the controls.
        if ctx.in_player
            && !ctx.pointing_at_ui
            && (input.right.select.just_pressed || input.left.select.just_pressed)
        {
            out.actions.push(InputAction::ToggleControls);
        }

        if input.x.just_pressed && ctx.in_player {
            out.actions.push(InputAction::OpenPictureAdjust);
        }
        if input.y.just_pressed || input.view.just_pressed {
            out.actions.push(InputAction::OpenSettings);
        }

        // Right stick.
        let stick = r.thumbstick;
        let grip = r.squeeze >= GRIP_THRESHOLD || r.grip_button.pressed;
        if ctx.pointing_at_ui {
            // The frame loop routes the stick to scrolling.
            self.reset_stick();
        } else if grip && ctx.in_player {
            let dz = |v: f32| if v.abs() < 0.15 { 0.0 } else { v };
            let (x, y) = (dz(stick.x), dz(stick.y));
            if x != 0.0 || y != 0.0 {
                out.actions.push(InputAction::AdjustScreen {
                    distance: y * SCREEN_DISTANCE_RATE * dt,
                    size: x * SCREEN_SIZE_RATE * dt,
                });
            }
            self.reset_stick();
        } else if ctx.in_player && !ctx.ui_visible {
            self.player_stick(stick, dt, &mut out);
        } else if ctx.ui_visible {
            out.nav.dir = self.navigator.update(stick, dt);
        } else {
            self.reset_stick();
        }

        // D-pad.
        let d = &input.dpad;
        let pressed_dir = if d.up.just_pressed {
            Some(NavDir::Up)
        } else if d.down.just_pressed {
            Some(NavDir::Down)
        } else if d.left.just_pressed {
            Some(NavDir::Left)
        } else if d.right.just_pressed {
            Some(NavDir::Right)
        } else {
            None
        };
        if let Some(dir) = pressed_dir {
            if ctx.ui_visible {
                out.nav.dir = out.nav.dir.or(Some(dir));
            } else if ctx.in_player {
                let sign = match dir {
                    NavDir::Left => -1,
                    NavDir::Right => 1,
                    _ => 0,
                };
                if sign != 0 {
                    out.actions.push(if ctx.paused {
                        InputAction::FrameStep(sign)
                    } else {
                        InputAction::Chapter(sign)
                    });
                }
            }
        }
        out
    }

    fn reset_stick(&mut self) {
        self.seek_dir = 0;
        self.seek_timer = 0.0;
        self.speed_latched = false;
    }

    fn player_stick(&mut self, stick: Vec2, dt: f32, out: &mut MappedInput) {
        let horizontal = stick.x.abs() >= stick.y.abs();
        let dir_x = if horizontal && stick.x.abs() >= STICK_THRESHOLD {
            stick.x.signum() as i32
        } else {
            0
        };
        if dir_x != 0 {
            if dir_x != self.seek_dir {
                self.seek_dir = dir_x;
                self.seek_timer = SEEK_REPEAT_S;
                out.actions.push(InputAction::SeekStep(dir_x));
            } else {
                self.seek_timer -= dt;
                if self.seek_timer <= 0.0 {
                    self.seek_timer += SEEK_REPEAT_S;
                    out.actions.push(InputAction::SeekStep(dir_x));
                }
            }
        } else {
            self.seek_dir = 0;
        }
        let vertical = !horizontal && stick.y.abs() >= STICK_THRESHOLD;
        if vertical && !self.speed_latched {
            self.speed_latched = true;
            out.actions
                .push(InputAction::SpeedStep(stick.y.signum() as i32));
        } else if stick.y.abs() < STICK_THRESHOLD * 0.7 {
            self.speed_latched = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f32 = 1.0 / 72.0;

    fn press(b: &mut Button) {
        b.update(true, true);
    }
    fn release(b: &mut Button) {
        b.update(false, false);
    }
    fn hold(b: &mut Button) {
        b.update(true, true);
    }

    fn watching() -> InputContext {
        InputContext {
            in_player: true,
            ..Default::default()
        }
    }
    fn menu_ui() -> InputContext {
        InputContext {
            ui_visible: true,
            ..Default::default()
        }
    }

    fn actions(m: &mut InputMapper, s: &InputState, ctx: InputContext) -> Vec<InputAction> {
        m.update(s, &ctx, DT)
            .actions
            .into_iter()
            .filter(|a| *a != InputAction::Activity)
            .collect()
    }

    #[test]
    fn a_toggles_play_unless_pointing_at_ui() {
        let mut m = InputMapper::new();
        let mut s = InputState::default();
        press(&mut s.a);
        assert_eq!(
            actions(&mut m, &s, watching()),
            vec![InputAction::TogglePlay]
        );
        let ctx = InputContext {
            pointing_at_ui: true,
            ui_visible: true,
            in_player: true,
            paused: false,
        };
        assert!(actions(&mut m, &s, ctx).is_empty());
        // In menus A activates the focused widget instead.
        let out = m.update(&s, &menu_ui(), DT);
        assert!(out.nav.activate);
        assert!(!out.actions.contains(&InputAction::TogglePlay));
    }

    #[test]
    fn b_is_back_or_ui_back() {
        let mut m = InputMapper::new();
        let mut s = InputState::default();
        press(&mut s.b);
        assert_eq!(actions(&mut m, &s, watching()), vec![InputAction::Back]);
        let out = m.update(&s, &menu_ui(), DT);
        assert!(out.nav.back);
        assert!(!out.actions.contains(&InputAction::Back));
    }

    #[test]
    fn stick_seeks_with_repeat_and_steps_speed() {
        let mut m = InputMapper::new();
        let mut s = InputState::default();
        s.right.thumbstick = Vec2::new(0.9, 0.0);
        assert_eq!(
            actions(&mut m, &s, watching()),
            vec![InputAction::SeekStep(1)]
        );
        // Held: no repeat until the repeat interval passed.
        let mut seeks = 0;
        for _ in 0..(SEEK_REPEAT_S / DT) as usize + 2 {
            seeks += actions(&mut m, &s, watching())
                .iter()
                .filter(|a| **a == InputAction::SeekStep(1))
                .count();
        }
        assert_eq!(seeks, 1);
        s.right.thumbstick = Vec2::new(-0.9, 0.0);
        assert_eq!(
            actions(&mut m, &s, watching()),
            vec![InputAction::SeekStep(-1)]
        );
        s.right.thumbstick = Vec2::ZERO;
        assert!(actions(&mut m, &s, watching()).is_empty());
        s.right.thumbstick = Vec2::new(0.0, 0.95);
        assert_eq!(
            actions(&mut m, &s, watching()),
            vec![InputAction::SpeedStep(1)]
        );
        assert!(
            actions(&mut m, &s, watching()).is_empty(),
            "speed is edge-triggered"
        );
        s.right.thumbstick = Vec2::ZERO;
        actions(&mut m, &s, watching());
        s.right.thumbstick = Vec2::new(0.0, -0.95);
        assert_eq!(
            actions(&mut m, &s, watching()),
            vec![InputAction::SpeedStep(-1)]
        );
    }

    #[test]
    fn grip_plus_stick_adjusts_screen() {
        let mut m = InputMapper::new();
        let mut s = InputState::default();
        s.right.squeeze = 1.0;
        s.right.thumbstick = Vec2::new(0.0, 1.0);
        match actions(&mut m, &s, watching())[..] {
            [InputAction::AdjustScreen { distance, size }] => {
                assert!((distance - SCREEN_DISTANCE_RATE * DT).abs() < 1e-6);
                assert_eq!(size, 0.0);
            }
            ref other => panic!("{other:?}"),
        }
    }

    #[test]
    fn stick_navigates_menus() {
        let mut m = InputMapper::new();
        let mut s = InputState::default();
        s.right.thumbstick = Vec2::new(0.0, -1.0);
        let out = m.update(&s, &menu_ui(), DT);
        assert_eq!(out.nav.dir, Some(NavDir::Down));
        assert!(!out
            .actions
            .iter()
            .any(|a| matches!(a, InputAction::SpeedStep(_))));
        // Pointing at the UI: neither nav nor seeking.
        let ctx = InputContext {
            pointing_at_ui: true,
            ui_visible: true,
            in_player: true,
            paused: false,
        };
        s.right.thumbstick = Vec2::new(1.0, 0.0);
        let out = m.update(&s, &ctx, DT);
        assert_eq!(out.nav.dir, None);
        assert!(!out
            .actions
            .iter()
            .any(|a| matches!(a, InputAction::SeekStep(_))));
    }

    #[test]
    fn menu_short_and_long_press() {
        let mut m = InputMapper::new();
        let mut s = InputState::default();
        press(&mut s.menu);
        assert!(actions(&mut m, &s, watching()).is_empty());
        release(&mut s.menu);
        assert_eq!(
            actions(&mut m, &s, watching()),
            vec![InputAction::ToggleControls]
        );
        release(&mut s.menu);
        // Long press: recenter fires once while held, no toggle on release.
        press(&mut s.menu);
        let mut got = Vec::new();
        for _ in 0..(LONG_PRESS_S / DT) as usize + 5 {
            got.extend(actions(&mut m, &s, watching()));
            hold(&mut s.menu);
        }
        assert_eq!(got, vec![InputAction::Recenter]);
        release(&mut s.menu);
        assert!(actions(&mut m, &s, watching()).is_empty());
        // Outside the player a short menu press opens settings.
        let mut m = InputMapper::new();
        let mut s = InputState::default();
        press(&mut s.menu);
        actions(&mut m, &s, menu_ui());
        release(&mut s.menu);
        assert_eq!(
            actions(&mut m, &s, menu_ui()),
            vec![InputAction::OpenSettings]
        );
    }

    #[test]
    fn stick_click_long_press_recenters() {
        let mut m = InputMapper::new();
        let mut s = InputState::default();
        press(&mut s.right.thumbstick_button);
        let mut got = Vec::new();
        for _ in 0..(LONG_PRESS_S / DT) as usize + 3 {
            got.extend(actions(&mut m, &s, watching()));
            hold(&mut s.right.thumbstick_button);
        }
        assert_eq!(got, vec![InputAction::Recenter]);
    }

    #[test]
    fn trigger_and_buttons_while_watching() {
        let mut m = InputMapper::new();
        let mut s = InputState::default();
        press(&mut s.right.select);
        assert_eq!(
            actions(&mut m, &s, watching()),
            vec![InputAction::ToggleControls]
        );
        let mut s = InputState::default();
        press(&mut s.x);
        assert_eq!(
            actions(&mut m, &s, watching()),
            vec![InputAction::OpenPictureAdjust]
        );
        let mut s = InputState::default();
        press(&mut s.view);
        assert_eq!(
            actions(&mut m, &s, watching()),
            vec![InputAction::OpenSettings]
        );
    }

    #[test]
    fn dpad_chapters_or_frame_steps() {
        let mut m = InputMapper::new();
        let mut s = InputState::default();
        press(&mut s.dpad.right);
        assert_eq!(
            actions(&mut m, &s, watching()),
            vec![InputAction::Chapter(1)]
        );
        let paused = InputContext {
            paused: true,
            ..watching()
        };
        let mut s = InputState::default();
        press(&mut s.dpad.left);
        assert_eq!(
            actions(&mut m, &s, paused),
            vec![InputAction::FrameStep(-1)]
        );
        let out = m.update(&s, &menu_ui(), DT);
        assert_eq!(out.nav.dir, Some(NavDir::Left));
    }

    #[test]
    fn activity_reported_for_any_press() {
        let mut m = InputMapper::new();
        let mut s = InputState::default();
        press(&mut s.y);
        let out = m.update(&s, &watching(), DT);
        assert_eq!(out.actions[0], InputAction::Activity);
    }
}
