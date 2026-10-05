//! Controller input as actions, bound per interaction profile.
//!
//! Every candidate binding is offered to the runtime on its own and only the
//! accepted ones are suggested together, so a path a runtime does not know
//! costs nothing. The Steam Frame profile comes first; Touch (what SteamVR
//! presents the Frame controllers as without the Frame extension), the simple
//! controller and hand interaction are bound the same way as fallbacks.

use crate::{Result, XrContextExt};
use glam::{Quat, Vec2, Vec3};
use openxr as xr;

/// State of one hand for this frame.
#[derive(Clone, Copy, Debug, Default)]
pub struct Hand {
    pub active: bool,
    /// Pointing ray origin and orientation (-Z forward), in the play space.
    pub aim: Option<(Vec3, Quat)>,
    pub trigger: f32,
    pub squeeze: f32,
    pub stick: Vec2,
    /// Thumbstick pressed in.
    pub stick_click: bool,
    /// The four face buttons by position. On the Frame's right controller
    /// south is A, north Y, west X and east B; on the left they are the
    /// D-pad's down, up, left and right. Touch has two per hand: A or X is
    /// south, B east (right) and Y north (left).
    pub south: bool,
    pub north: bool,
    pub west: bool,
    pub east: bool,
    /// Menu (right) or View (left).
    pub menu: bool,
    /// The bumper above the grip.
    pub shoulder: bool,
}

#[derive(Clone, Debug, Default)]
pub struct InputState {
    /// Left, right.
    pub hands: [Hand; 2],
}

pub(crate) struct Actions {
    pub set: xr::ActionSet,
    aim: xr::Action<xr::Posef>,
    trigger: xr::Action<f32>,
    squeeze: xr::Action<f32>,
    stick: xr::Action<xr::Vector2f>,
    stick_click: xr::Action<bool>,
    south: xr::Action<bool>,
    north: xr::Action<bool>,
    west: xr::Action<bool>,
    east: xr::Action<bool>,
    menu: xr::Action<bool>,
    shoulder: xr::Action<bool>,
    haptic: xr::Action<xr::Haptic>,
    pub hands: [xr::Path; 2],
    aim_spaces: Option<[xr::Space; 2]>,
    pub bound: Vec<(String, usize)>,
    /// `FRAMEPLAYER_DEBUG_INPUT=1`: log each hand's input when it changes,
    /// and errors reading action states (normally ignored).
    debug: bool,
    last_logged: std::cell::RefCell<[String; 2]>,
}

#[derive(Clone, Copy)]
enum Kind {
    Aim,
    Trigger,
    Squeeze,
    Stick,
    StickClick,
    South,
    North,
    West,
    East,
    Menu,
    Shoulder,
    Haptic,
}

/// A candidate binding: component path under /user/hand/<h>/, action, and a
/// hand mask (1 left, 2 right, 3 both).
type Candidate = (&'static str, Kind, u8);

/// Candidate bindings per interaction profile.
const PROFILES: &[(&str, &[Candidate])] = &[
    (
        // Valve's published Frame profile (ValveSoftware/Unity,
        // SteamFrameControllerProfile.cs); needs
        // XR_VALVE_frame_controller_interaction, enabled in context.rs.
        "/interaction_profiles/valve/frame_controller_valve",
        &[
            ("input/aim/pose", Kind::Aim, 3),
            ("input/trigger/value", Kind::Trigger, 3),
            ("input/squeeze/value", Kind::Squeeze, 3),
            ("input/thumbstick", Kind::Stick, 3),
            ("input/thumbstick/click", Kind::StickClick, 3),
            // The right controller has A/B/X/Y (Y top, X left, B right,
            // A bottom); the left has a D-pad in the same places. Each is its
            // own button: what they do is up to the app.
            ("input/a/click", Kind::South, 2),
            ("input/y/click", Kind::North, 2),
            ("input/x/click", Kind::West, 2),
            ("input/b/click", Kind::East, 2),
            ("input/menu/click", Kind::Menu, 2),
            ("input/dpad_down/click", Kind::South, 1),
            ("input/dpad_up/click", Kind::North, 1),
            ("input/dpad_left/click", Kind::West, 1),
            ("input/dpad_right/click", Kind::East, 1),
            ("input/view/click", Kind::Menu, 1),
            ("input/shoulder/click", Kind::Shoulder, 3),
            ("output/haptic", Kind::Haptic, 3),
        ],
    ),
    (
        "/interaction_profiles/oculus/touch_controller",
        &[
            ("input/aim/pose", Kind::Aim, 3),
            ("input/trigger/value", Kind::Trigger, 3),
            ("input/squeeze/value", Kind::Squeeze, 3),
            ("input/thumbstick", Kind::Stick, 3),
            ("input/thumbstick/click", Kind::StickClick, 3),
            ("input/a/click", Kind::South, 2),
            ("input/b/click", Kind::East, 2),
            ("input/x/click", Kind::South, 1),
            ("input/y/click", Kind::North, 1),
            ("input/menu/click", Kind::Menu, 1),
            ("output/haptic", Kind::Haptic, 3),
        ],
    ),
    (
        "/interaction_profiles/khr/simple_controller",
        &[
            ("input/aim/pose", Kind::Aim, 3),
            ("input/select/click", Kind::Trigger, 3),
            ("input/menu/click", Kind::Menu, 3),
            ("output/haptic", Kind::Haptic, 3),
        ],
    ),
    (
        "/interaction_profiles/ext/hand_interaction_ext",
        &[
            ("input/aim/pose", Kind::Aim, 3),
            ("input/pinch_ext/value", Kind::Trigger, 3),
            ("input/grasp_ext/value", Kind::Squeeze, 3),
        ],
    ),
];

impl Actions {
    pub fn new(instance: &xr::Instance) -> Result<Actions> {
        let hands = [
            instance.string_to_path("/user/hand/left").ctx("path")?,
            instance.string_to_path("/user/hand/right").ctx("path")?,
        ];
        let set = instance
            .create_action_set("player", "Player", 0)
            .ctx("action set")?;
        let mk = |n: &str, l: &str| (n.to_string(), l.to_string());
        let (a, al) = mk("aim", "Pointer");
        let aim = set
            .create_action::<xr::Posef>(&a, &al, &hands)
            .ctx("action")?;
        let trigger = set
            .create_action::<f32>("trigger", "Select", &hands)
            .ctx("action")?;
        let squeeze = set
            .create_action::<f32>("squeeze", "Grab", &hands)
            .ctx("action")?;
        let stick = set
            .create_action::<xr::Vector2f>("stick", "Thumbstick", &hands)
            .ctx("action")?;
        let stick_click = set
            .create_action::<bool>("stick_click", "Reset view", &hands)
            .ctx("action")?;
        let button =
            |name: &str, label: &str| set.create_action::<bool>(name, label, &hands).ctx("action");
        let south = button("south", "A / D-pad down")?;
        let north = button("north", "Y / D-pad up")?;
        let west = button("west", "X / D-pad left")?;
        let east = button("east", "B / D-pad right")?;
        let menu = button("menu", "Menu / View")?;
        let shoulder = button("shoulder", "Bumper")?;
        let haptic = set
            .create_action::<xr::Haptic>("haptic", "Vibration", &hands)
            .ctx("action")?;
        let mut actions = Actions {
            set,
            aim,
            trigger,
            squeeze,
            stick,
            stick_click,
            south,
            north,
            west,
            east,
            menu,
            shoulder,
            haptic,
            hands,
            aim_spaces: None,
            bound: Vec::new(),
            debug: std::env::var_os("FRAMEPLAYER_DEBUG_INPUT").is_some_and(|v| v != "0"),
            last_logged: Default::default(),
        };
        for (profile, candidates) in PROFILES {
            let Ok(profile_path) = instance.string_to_path(profile) else {
                continue;
            };
            // Every profile we list has an aim pose; if the runtime rejects
            // it, the runtime does not know the profile at all.
            let Ok(probe) = instance.string_to_path("/user/hand/right/input/aim/pose") else {
                continue;
            };
            if actions
                .suggest(instance, profile_path, &[(probe, Kind::Aim)])
                .is_err()
            {
                continue;
            }
            let mut accepted: Vec<(xr::Path, Kind)> = Vec::new();
            for (component, kind, mask) in candidates.iter() {
                for (i, side) in ["left", "right"].iter().enumerate() {
                    if mask & (1 << i) == 0 {
                        continue;
                    }
                    let Ok(p) = instance.string_to_path(&format!("/user/hand/{side}/{component}"))
                    else {
                        continue;
                    };
                    if actions
                        .suggest(instance, profile_path, &[(p, *kind)])
                        .is_ok()
                    {
                        accepted.push((p, *kind));
                    }
                }
            }
            if accepted.is_empty() {
                continue;
            }
            // De-duplicate: one binding per (path) is enough.
            if actions.suggest(instance, profile_path, &accepted).is_ok() {
                actions.bound.push((profile.to_string(), accepted.len()));
            }
        }
        log::info!("input bindings: {:?}", actions.bound);
        Ok(actions)
    }

    fn suggest(
        &self,
        instance: &xr::Instance,
        profile: xr::Path,
        list: &[(xr::Path, Kind)],
    ) -> std::result::Result<(), xr::sys::Result> {
        let bindings: Vec<xr::Binding> = list
            .iter()
            .map(|(p, k)| match k {
                Kind::Aim => xr::Binding::new(&self.aim, *p),
                Kind::Trigger => xr::Binding::new(&self.trigger, *p),
                Kind::Squeeze => xr::Binding::new(&self.squeeze, *p),
                Kind::Stick => xr::Binding::new(&self.stick, *p),
                Kind::StickClick => xr::Binding::new(&self.stick_click, *p),
                Kind::South => xr::Binding::new(&self.south, *p),
                Kind::North => xr::Binding::new(&self.north, *p),
                Kind::West => xr::Binding::new(&self.west, *p),
                Kind::East => xr::Binding::new(&self.east, *p),
                Kind::Menu => xr::Binding::new(&self.menu, *p),
                Kind::Shoulder => xr::Binding::new(&self.shoulder, *p),
                Kind::Haptic => xr::Binding::new(&self.haptic, *p),
            })
            .collect();
        instance.suggest_interaction_profile_bindings(profile, &bindings)
    }

    pub fn attach<G>(&mut self, session: &xr::Session<G>) -> Result<()> {
        session
            .attach_action_sets(&[&self.set])
            .ctx("xrAttachSessionActionSets")?;
        let mk = |h| {
            self.aim
                .create_space(session, h, xr::Posef::IDENTITY)
                .ctx("aim space")
        };
        self.aim_spaces = Some([mk(self.hands[0])?, mk(self.hands[1])?]);
        Ok(())
    }

    pub fn read<G>(
        &self,
        session: &xr::Session<G>,
        base: &xr::Space,
        time: xr::Time,
    ) -> InputState {
        let mut out = InputState::default();
        if session
            .sync_actions(&[xr::ActiveActionSet::new(&self.set)])
            .is_err()
        {
            return out;
        }
        for (i, h) in self.hands.iter().enumerate() {
            let hand = &mut out.hands[i];
            let mut trigger_note = String::new();
            match self.trigger.state(session, *h) {
                Ok(s) => {
                    hand.trigger = s.current_state;
                    hand.active |= s.is_active;
                    if !s.is_active {
                        trigger_note = " trigger-inactive".into();
                    }
                }
                Err(e) => trigger_note = format!(" trigger-error={e}"),
            }
            if let Ok(s) = self.squeeze.state(session, *h) {
                hand.squeeze = s.current_state;
            }
            if let Ok(s) = self.stick.state(session, *h) {
                hand.stick = Vec2::new(s.current_state.x, s.current_state.y);
            }
            let down = |a: &xr::Action<bool>| a.state(session, *h).is_ok_and(|s| s.current_state);
            hand.stick_click = down(&self.stick_click);
            hand.south = down(&self.south);
            hand.north = down(&self.north);
            hand.west = down(&self.west);
            hand.east = down(&self.east);
            hand.menu = down(&self.menu);
            hand.shoulder = down(&self.shoulder);
            if let Some(spaces) = &self.aim_spaces
                && let Ok(loc) = spaces[i].locate(base, time)
            {
                let ok = loc.location_flags.contains(
                    xr::SpaceLocationFlags::POSITION_VALID
                        | xr::SpaceLocationFlags::ORIENTATION_VALID,
                );
                if ok {
                    hand.aim = Some(crate::pose(loc.pose));
                    hand.active = true;
                }
            }
            if self.debug {
                let line = format!(
                    "trigger {:.1} squeeze {:.1} stick {:.1},{:.1} click {} s/n/w/e {}{}{}{} menu {} shoulder {} aim {}{}",
                    hand.trigger,
                    hand.squeeze,
                    hand.stick.x,
                    hand.stick.y,
                    hand.stick_click,
                    hand.south as u8,
                    hand.north as u8,
                    hand.west as u8,
                    hand.east as u8,
                    hand.menu,
                    hand.shoulder,
                    hand.aim.is_some(),
                    trigger_note
                );
                let mut last = self.last_logged.borrow_mut();
                if last[i] != line {
                    log::info!("input {}: {line}", ["left", "right"][i]);
                    last[i] = line;
                }
            }
        }
        out
    }

    /// Short vibration for UI feedback.
    pub fn buzz<G>(&self, session: &xr::Session<G>, hand: usize, amplitude: f32, millis: i64) {
        let event = xr::HapticVibration::new()
            .amplitude(amplitude.clamp(0.0, 1.0))
            .frequency(xr::FREQUENCY_UNSPECIFIED)
            .duration(xr::Duration::from_nanos(millis * 1_000_000));
        let _ = self
            .haptic
            .apply_feedback(session, self.hands[hand.min(1)], &event);
    }
}
