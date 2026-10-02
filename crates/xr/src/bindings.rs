//! Action definitions and suggested bindings, as plain data so they can be
//! validated by tests without a runtime.
//!
//! Binding tiers: suggesting bindings for a profile fails as a whole if any
//! single path is rejected, so every profile has a `full` list (including
//! guessed component paths) and a `core` list of paths that are near
//! certain. The runtime layer tries `full` first and falls back to `core`.

/// Value type of an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Bool,
    Float,
    Vec2,
    Pose,
    Haptic,
}

/// Every action FramePlayer defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionId {
    // Per-hand (subaction paths /user/hand/left|right).
    Trigger,
    TriggerClick,
    TriggerTouch,
    Squeeze,
    Bumper,
    Thumbstick,
    ThumbstickClick,
    ThumbstickTouch,
    AimPose,
    GripPose,
    Haptic,
    // Global.
    ButtonA,
    ButtonB,
    ButtonX,
    ButtonY,
    TouchA,
    TouchB,
    TouchX,
    TouchY,
    Menu,
    View,
    DpadUp,
    DpadDown,
    DpadLeft,
    DpadRight,
}

impl ActionId {
    pub const ALL: [ActionId; 25] = [
        ActionId::Trigger,
        ActionId::TriggerClick,
        ActionId::TriggerTouch,
        ActionId::Squeeze,
        ActionId::Bumper,
        ActionId::Thumbstick,
        ActionId::ThumbstickClick,
        ActionId::ThumbstickTouch,
        ActionId::AimPose,
        ActionId::GripPose,
        ActionId::Haptic,
        ActionId::ButtonA,
        ActionId::ButtonB,
        ActionId::ButtonX,
        ActionId::ButtonY,
        ActionId::TouchA,
        ActionId::TouchB,
        ActionId::TouchX,
        ActionId::TouchY,
        ActionId::Menu,
        ActionId::View,
        ActionId::DpadUp,
        ActionId::DpadDown,
        ActionId::DpadLeft,
        ActionId::DpadRight,
    ];

    /// `(action name, localized name)`; names follow OpenXR's
    /// `[a-z0-9_.-]` rule.
    pub fn names(self) -> (&'static str, &'static str) {
        use ActionId::*;
        match self {
            Trigger => ("trigger", "Trigger"),
            TriggerClick => ("select", "Select"),
            TriggerTouch => ("trigger_touch", "Trigger touch"),
            Squeeze => ("squeeze", "Grip"),
            Bumper => ("bumper", "Bumper"),
            Thumbstick => ("thumbstick", "Thumbstick"),
            ThumbstickClick => ("thumbstick_click", "Thumbstick click"),
            ThumbstickTouch => ("thumbstick_touch", "Thumbstick touch"),
            AimPose => ("aim_pose", "Aim pose"),
            GripPose => ("grip_pose", "Grip pose"),
            Haptic => ("haptic", "Vibration"),
            ButtonA => ("button_a", "A"),
            ButtonB => ("button_b", "B"),
            ButtonX => ("button_x", "X"),
            ButtonY => ("button_y", "Y"),
            TouchA => ("touch_a", "A touch"),
            TouchB => ("touch_b", "B touch"),
            TouchX => ("touch_x", "X touch"),
            TouchY => ("touch_y", "Y touch"),
            Menu => ("menu", "Menu"),
            View => ("view", "View"),
            DpadUp => ("dpad_up", "D-pad up"),
            DpadDown => ("dpad_down", "D-pad down"),
            DpadLeft => ("dpad_left", "D-pad left"),
            DpadRight => ("dpad_right", "D-pad right"),
        }
    }

    pub fn kind(self) -> ActionKind {
        use ActionId::*;
        match self {
            Trigger | Squeeze => ActionKind::Float,
            Thumbstick => ActionKind::Vec2,
            AimPose | GripPose => ActionKind::Pose,
            Haptic => ActionKind::Haptic,
            _ => ActionKind::Bool,
        }
    }

    /// Whether the action is created with left/right subaction paths.
    pub fn per_hand(self) -> bool {
        use ActionId::*;
        matches!(
            self,
            Trigger
                | TriggerClick
                | TriggerTouch
                | Squeeze
                | Bumper
                | Thumbstick
                | ThumbstickClick
                | ThumbstickTouch
                | AimPose
                | GripPose
                | Haptic
        )
    }

    pub fn index(self) -> usize {
        ActionId::ALL
            .iter()
            .position(|&a| a == self)
            .expect("in ALL")
    }
}

/// Interaction profiles we suggest bindings for, in priority order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Profile {
    /// Valve Steam Frame controllers.
    Frame,
    /// Valve Index controllers (also what many runtimes emulate).
    Index,
    /// KHR simple controller (universal fallback).
    Simple,
}

impl Profile {
    pub const ALL: [Profile; 3] = [Profile::Frame, Profile::Index, Profile::Simple];

    pub fn path(self) -> &'static str {
        match self {
            Profile::Frame => "/interaction_profiles/valve/frame_controller_valve",
            Profile::Index => "/interaction_profiles/valve/index_controller",
            Profile::Simple => "/interaction_profiles/khr/simple_controller",
        }
    }

    pub fn from_path(p: &str) -> Option<Profile> {
        Profile::ALL.into_iter().find(|x| x.path() == p)
    }
}

/// Eye gaze interaction profile and its single pose binding.
pub const EYE_GAZE_PROFILE: &str = "/interaction_profiles/ext/eye_gaze_interaction";
pub const EYE_GAZE_POSE: &str = "/user/eyes_ext/input/gaze_ext/pose";

pub const LEFT: &str = "/user/hand/left";
pub const RIGHT: &str = "/user/hand/right";

macro_rules! both {
    ($id:expr, $suffix:literal) => {
        [
            ($id, concat!("/user/hand/left", $suffix)),
            ($id, concat!("/user/hand/right", $suffix)),
        ]
    };
}

type Binding = (ActionId, &'static str);

const COMMON_HANDS: [[Binding; 2]; 6] = [
    both!(ActionId::Trigger, "/input/trigger/value"),
    both!(ActionId::Squeeze, "/input/squeeze/value"),
    both!(ActionId::Thumbstick, "/input/thumbstick"),
    both!(ActionId::AimPose, "/input/aim/pose"),
    both!(ActionId::GripPose, "/input/grip/pose"),
    both!(ActionId::Haptic, "/output/haptic"),
];

/// Paths for the Frame controllers. Layout per the outline: A/B/X/Y and
/// menu on the right controller, D-pad and view on the left (Steam Deck
/// style), bumper/grip/trigger/stick on both, capacitive touch everywhere.
// [verify] Every component path below on SteamVR for the Frame (dump with
// `xrEnumerateBoundSourcesForAction` / SteamVR binding UI). Paths mirror
// the Index profile's naming where the hardware matches.
const FRAME_EXTRA: &[Binding] = &[
    (
        ActionId::TriggerClick,
        "/user/hand/left/input/trigger/click",
    ),
    (
        ActionId::TriggerClick,
        "/user/hand/right/input/trigger/click",
    ),
    (
        ActionId::TriggerTouch,
        "/user/hand/left/input/trigger/touch",
    ),
    (
        ActionId::TriggerTouch,
        "/user/hand/right/input/trigger/touch",
    ),
    (ActionId::Bumper, "/user/hand/left/input/bumper/click"),
    (ActionId::Bumper, "/user/hand/right/input/bumper/click"),
    (
        ActionId::ThumbstickClick,
        "/user/hand/left/input/thumbstick/click",
    ),
    (
        ActionId::ThumbstickClick,
        "/user/hand/right/input/thumbstick/click",
    ),
    (
        ActionId::ThumbstickTouch,
        "/user/hand/left/input/thumbstick/touch",
    ),
    (
        ActionId::ThumbstickTouch,
        "/user/hand/right/input/thumbstick/touch",
    ),
    (ActionId::ButtonA, "/user/hand/right/input/a/click"),
    (ActionId::ButtonB, "/user/hand/right/input/b/click"),
    (ActionId::ButtonX, "/user/hand/right/input/x/click"),
    (ActionId::ButtonY, "/user/hand/right/input/y/click"),
    (ActionId::TouchA, "/user/hand/right/input/a/touch"),
    (ActionId::TouchB, "/user/hand/right/input/b/touch"),
    (ActionId::TouchX, "/user/hand/right/input/x/touch"),
    (ActionId::TouchY, "/user/hand/right/input/y/touch"),
    (ActionId::Menu, "/user/hand/right/input/menu/click"),
    (ActionId::View, "/user/hand/left/input/view/click"),
    (ActionId::DpadUp, "/user/hand/left/input/dpad_up/click"),
    (ActionId::DpadDown, "/user/hand/left/input/dpad_down/click"),
    (ActionId::DpadLeft, "/user/hand/left/input/dpad_left/click"),
    (
        ActionId::DpadRight,
        "/user/hand/left/input/dpad_right/click",
    ),
];

/// Frame bindings that are near certain (mirror Index components).
const FRAME_CORE_EXTRA: &[Binding] = &[
    (
        ActionId::TriggerClick,
        "/user/hand/left/input/trigger/click",
    ),
    (
        ActionId::TriggerClick,
        "/user/hand/right/input/trigger/click",
    ),
    (
        ActionId::ThumbstickClick,
        "/user/hand/left/input/thumbstick/click",
    ),
    (
        ActionId::ThumbstickClick,
        "/user/hand/right/input/thumbstick/click",
    ),
    (ActionId::ButtonA, "/user/hand/right/input/a/click"),
    (ActionId::ButtonB, "/user/hand/right/input/b/click"),
];

/// Valve Index: A/B on both hands; left A/B stand in for X/Y. The system
/// button is reserved by SteamVR, so Menu/View stay unbound (the app can
/// treat a long B press as menu) and the D-pad is emulated from the left
/// thumbstick (see `input::stick_to_dpad`).
const INDEX_EXTRA: &[Binding] = &[
    (
        ActionId::TriggerClick,
        "/user/hand/left/input/trigger/click",
    ),
    (
        ActionId::TriggerClick,
        "/user/hand/right/input/trigger/click",
    ),
    (
        ActionId::TriggerTouch,
        "/user/hand/left/input/trigger/touch",
    ),
    (
        ActionId::TriggerTouch,
        "/user/hand/right/input/trigger/touch",
    ),
    (
        ActionId::ThumbstickClick,
        "/user/hand/left/input/thumbstick/click",
    ),
    (
        ActionId::ThumbstickClick,
        "/user/hand/right/input/thumbstick/click",
    ),
    (
        ActionId::ThumbstickTouch,
        "/user/hand/left/input/thumbstick/touch",
    ),
    (
        ActionId::ThumbstickTouch,
        "/user/hand/right/input/thumbstick/touch",
    ),
    (ActionId::ButtonA, "/user/hand/right/input/a/click"),
    (ActionId::ButtonB, "/user/hand/right/input/b/click"),
    (ActionId::ButtonX, "/user/hand/left/input/a/click"),
    (ActionId::ButtonY, "/user/hand/left/input/b/click"),
    (ActionId::TouchA, "/user/hand/right/input/a/touch"),
    (ActionId::TouchB, "/user/hand/right/input/b/touch"),
    (ActionId::TouchX, "/user/hand/left/input/a/touch"),
    (ActionId::TouchY, "/user/hand/left/input/b/touch"),
];

const SIMPLE: &[Binding] = &[
    (ActionId::TriggerClick, "/user/hand/left/input/select/click"),
    (
        ActionId::TriggerClick,
        "/user/hand/right/input/select/click",
    ),
    (ActionId::Menu, "/user/hand/left/input/menu/click"),
    (ActionId::Menu, "/user/hand/right/input/menu/click"),
    (ActionId::AimPose, "/user/hand/left/input/aim/pose"),
    (ActionId::AimPose, "/user/hand/right/input/aim/pose"),
    (ActionId::GripPose, "/user/hand/left/input/grip/pose"),
    (ActionId::GripPose, "/user/hand/right/input/grip/pose"),
    (ActionId::Haptic, "/user/hand/left/output/haptic"),
    (ActionId::Haptic, "/user/hand/right/output/haptic"),
];

/// Suggested bindings for `profile`. `full = false` returns the
/// conservative core subset.
pub fn bindings(profile: Profile, full: bool) -> Vec<Binding> {
    let common = || COMMON_HANDS.iter().flatten().copied();
    match profile {
        Profile::Frame => common()
            .chain(
                if full { FRAME_EXTRA } else { FRAME_CORE_EXTRA }
                    .iter()
                    .copied(),
            )
            .collect(),
        Profile::Index => common().chain(INDEX_EXTRA.iter().copied()).collect(),
        Profile::Simple => SIMPLE.to_vec(),
    }
}

/// Expected action kind for a binding path, from its last component.
pub fn kind_for_path(path: &str) -> Option<ActionKind> {
    if path.ends_with("/output/haptic") {
        return Some(ActionKind::Haptic);
    }
    let last = path.rsplit('/').next()?;
    Some(match last {
        "click" | "touch" => ActionKind::Bool,
        "value" | "force" => ActionKind::Float,
        "pose" => ActionKind::Pose,
        "thumbstick" | "trackpad" => ActionKind::Vec2,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn action_names_valid_and_unique() {
        let mut seen = HashSet::new();
        for a in ActionId::ALL {
            let (n, l) = a.names();
            assert!(
                n.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_-.".contains(c)),
                "{n}"
            );
            assert!(!l.is_empty());
            assert!(seen.insert(n), "duplicate {n}");
            assert_eq!(ActionId::ALL[a.index()], a);
        }
    }

    #[test]
    fn bindings_well_formed() {
        for p in Profile::ALL {
            assert!(p.path().starts_with("/interaction_profiles/"));
            assert_eq!(Profile::from_path(p.path()), Some(p));
            for full in [true, false] {
                let b = bindings(p, full);
                assert!(!b.is_empty());
                let mut seen = HashSet::new();
                for (a, path) in &b {
                    assert!(path.starts_with(LEFT) || path.starts_with(RIGHT), "{path}");
                    assert_eq!(kind_for_path(path), Some(a.kind()), "{p:?} {a:?} {path}");
                    assert!(seen.insert((*a, *path)), "duplicate {path}");
                }
            }
        }
        // The core Frame set is a subset of the full one.
        let full: HashSet<_> = bindings(Profile::Frame, true).into_iter().collect();
        assert!(bindings(Profile::Frame, false)
            .iter()
            .all(|b| full.contains(b)));
        assert!(Profile::from_path("/interaction_profiles/htc/vive_controller").is_none());
    }

    #[test]
    fn frame_layout_matches_outline() {
        let b = bindings(Profile::Frame, true);
        let has = |a: ActionId, p: &str| b.iter().any(|&(x, q)| x == a && q == p);
        assert!(has(ActionId::ButtonX, "/user/hand/right/input/x/click"));
        assert!(has(ActionId::DpadUp, "/user/hand/left/input/dpad_up/click"));
        assert!(has(ActionId::Menu, "/user/hand/right/input/menu/click"));
        assert!(has(ActionId::View, "/user/hand/left/input/view/click"));
        // Every action except gaze has at least one Frame binding.
        for a in ActionId::ALL {
            assert!(b.iter().any(|&(x, _)| x == a), "{a:?} unbound on Frame");
        }
        assert_eq!(kind_for_path(EYE_GAZE_POSE), Some(ActionKind::Pose));
    }
}
