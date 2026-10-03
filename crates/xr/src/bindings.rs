//! Action definitions and suggested bindings, as plain data so they can be
//! validated by tests without a runtime.
//!
//! The Steam Frame controller paths follow Valve's published OpenXR profile
//! (ValveSoftware/Unity, `SteamFrameControllerProfile.cs`). The profile
//! needs `XR_VALVE_frame_controller_interaction`; without it SteamVR presents
//! the Frame controllers as emulated Touch controllers, so the Touch profile
//! is the fallback, then the KHR simple controller.

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
    /// Valve Steam Frame controllers (XR_VALVE_frame_controller_interaction).
    Frame,
    /// Oculus Touch: what SteamVR presents the Frame controllers as when the
    /// Frame profile is unavailable.
    Touch,
    /// KHR simple controller (universal fallback).
    Simple,
}

impl Profile {
    pub const ALL: [Profile; 3] = [Profile::Frame, Profile::Touch, Profile::Simple];

    pub fn path(self) -> &'static str {
        match self {
            Profile::Frame => "/interaction_profiles/valve/frame_controller_valve",
            Profile::Touch => "/interaction_profiles/oculus/touch_controller",
            Profile::Simple => "/interaction_profiles/khr/simple_controller",
        }
    }

    pub fn from_path(p: &str) -> Option<Profile> {
        Profile::ALL.into_iter().find(|x| x.path() == p)
    }
}

/// The OpenXR extension that defines the Frame controller profile. Not in the
/// Khronos registry yet; SteamVR on the Steam Frame provides it.
pub const FRAME_CONTROLLER_EXTENSION: &str = "XR_VALVE_frame_controller_interaction";

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

/// Frame controllers: A/B/X/Y and menu on the right, D-pad and view on the
/// left, a shoulder button (the bumper), grip, trigger and stick on both,
/// touch on every button. The system button is reserved by the runtime.
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
    (ActionId::Bumper, "/user/hand/left/input/shoulder/click"),
    (ActionId::Bumper, "/user/hand/right/input/shoulder/click"),
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

/// Touch, as SteamVR emulates it for the Frame controllers: A/B on the
/// right, X/Y and menu on the left, no trigger click (the analog threshold in
/// `input` stands in), no D-pad (emulated from the left stick), no view or
/// shoulder buttons.
const TOUCH_EXTRA: &[Binding] = &[
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
    (ActionId::ButtonX, "/user/hand/left/input/x/click"),
    (ActionId::ButtonY, "/user/hand/left/input/y/click"),
    (ActionId::TouchA, "/user/hand/right/input/a/touch"),
    (ActionId::TouchB, "/user/hand/right/input/b/touch"),
    (ActionId::TouchX, "/user/hand/left/input/x/touch"),
    (ActionId::TouchY, "/user/hand/left/input/y/touch"),
    (ActionId::Menu, "/user/hand/left/input/menu/click"),
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

/// Suggested bindings for `profile`.
pub fn bindings(profile: Profile) -> Vec<Binding> {
    let common = || COMMON_HANDS.iter().flatten().copied();
    match profile {
        Profile::Frame => common().chain(FRAME_EXTRA.iter().copied()).collect(),
        Profile::Touch => common().chain(TOUCH_EXTRA.iter().copied()).collect(),
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
            let b = bindings(p);
            assert!(!b.is_empty());
            let mut seen = HashSet::new();
            for (a, path) in &b {
                assert!(path.starts_with(LEFT) || path.starts_with(RIGHT), "{path}");
                assert_eq!(kind_for_path(path), Some(a.kind()), "{p:?} {a:?} {path}");
                assert!(seen.insert((*a, *path)), "duplicate {path}");
            }
        }
        assert!(Profile::from_path("/interaction_profiles/valve/index_controller").is_none());
    }

    #[test]
    fn frame_layout_matches_valve_profile() {
        let b = bindings(Profile::Frame);
        let has = |a: ActionId, p: &str| b.iter().any(|&(x, q)| x == a && q == p);
        assert!(has(ActionId::ButtonX, "/user/hand/right/input/x/click"));
        assert!(has(ActionId::DpadUp, "/user/hand/left/input/dpad_up/click"));
        assert!(has(ActionId::Menu, "/user/hand/right/input/menu/click"));
        assert!(has(ActionId::View, "/user/hand/left/input/view/click"));
        assert!(has(
            ActionId::Bumper,
            "/user/hand/left/input/shoulder/click"
        ));
        assert!(!b
            .iter()
            .any(|(_, p)| p.contains("bumper") || p.contains("/system/")));
        // Every action except gaze has at least one Frame binding.
        for a in ActionId::ALL {
            assert!(b.iter().any(|&(x, _)| x == a), "{a:?} unbound on Frame");
        }
        assert_eq!(kind_for_path(EYE_GAZE_POSE), Some(ActionKind::Pose));
        assert!(FRAME_CONTROLLER_EXTENSION.starts_with("XR_VALVE_"));
    }

    #[test]
    fn touch_emulation_layout() {
        let b = bindings(Profile::Touch);
        let has = |a: ActionId, p: &str| b.iter().any(|&(x, q)| x == a && q == p);
        assert!(has(ActionId::ButtonA, "/user/hand/right/input/a/click"));
        assert!(has(ActionId::ButtonX, "/user/hand/left/input/x/click"));
        assert!(has(ActionId::Menu, "/user/hand/left/input/menu/click"));
        // Touch has no trigger click or D-pad: input falls back to the analog
        // trigger threshold and stick D-pad emulation.
        assert!(!b
            .iter()
            .any(|&(x, _)| x == ActionId::TriggerClick || x == ActionId::DpadUp));
    }
}
