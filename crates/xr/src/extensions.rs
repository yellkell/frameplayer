//! Which OpenXR extensions FramePlayer asks for, and a report of which ones
//! the runtime actually exposes.

use crate::bindings::FRAME_CONTROLLER_EXTENSION;
use openxr as xr;

/// Extensions the `openxr` crate has no field for live in
/// `ExtensionSet::other`, by name.
fn has_other(set: &xr::ExtensionSet, name: &str) -> bool {
    set.other.iter().any(|n| n == name)
}

/// Extension availability / enablement report. Logged at startup so the
/// `[verify]` questions about SteamVR on the Frame get answered from logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExtensionReport {
    pub vulkan_enable2: bool,
    pub eye_gaze_interaction: bool,
    pub hand_tracking: bool,
    pub composition_layer_cylinder: bool,
    pub composition_layer_depth: bool,
    pub display_refresh_rate: bool,
    pub convert_timespec_time: bool,
    pub foveation: bool,
    /// XR_VALVE_frame_controller_interaction: the Frame controller profile.
    pub frame_controller: bool,
}

impl ExtensionReport {
    pub fn from_available(a: &xr::ExtensionSet) -> ExtensionReport {
        ExtensionReport {
            vulkan_enable2: a.khr_vulkan_enable2,
            eye_gaze_interaction: a.ext_eye_gaze_interaction,
            hand_tracking: a.ext_hand_tracking,
            composition_layer_cylinder: a.khr_composition_layer_cylinder,
            composition_layer_depth: a.khr_composition_layer_depth,
            display_refresh_rate: a.fb_display_refresh_rate,
            convert_timespec_time: a.khr_convert_timespec_time,
            foveation: a.fb_foveation,
            frame_controller: has_other(a, FRAME_CONTROLLER_EXTENSION),
        }
    }

    /// Extensions to enable given availability and what the app wants.
    pub fn to_enable(&self, want: &ExtensionWishes) -> xr::ExtensionSet {
        let mut e = xr::ExtensionSet::default();
        e.khr_vulkan_enable2 = self.vulkan_enable2;
        e.ext_eye_gaze_interaction = self.eye_gaze_interaction && want.eye_gaze;
        e.ext_hand_tracking = self.hand_tracking && want.hand_tracking;
        e.khr_composition_layer_cylinder = self.composition_layer_cylinder;
        e.khr_composition_layer_depth = self.composition_layer_depth && want.depth_layers;
        e.fb_display_refresh_rate = self.display_refresh_rate;
        e.khr_convert_timespec_time = self.convert_timespec_time;
        if self.frame_controller {
            e.other.push(FRAME_CONTROLLER_EXTENSION.to_string());
        }
        e
    }

    /// The enabled subset, as a report (what the session can rely on).
    pub fn enabled(&self, want: &ExtensionWishes) -> ExtensionReport {
        let e = self.to_enable(want);
        let mut r = ExtensionReport::from_available(&e);
        r.foveation = false;
        r
    }

    /// One-line summary for logs.
    pub fn summary(&self) -> String {
        let f = |b: bool| if b { "yes" } else { "no" };
        format!(
            "vulkan_enable2={} eye_gaze={} hand_tracking={} cylinder={} depth={} refresh_rate={} timespec={} fb_foveation={} frame_controller={}",
            f(self.vulkan_enable2),
            f(self.eye_gaze_interaction),
            f(self.hand_tracking),
            f(self.composition_layer_cylinder),
            f(self.composition_layer_depth),
            f(self.display_refresh_rate),
            f(self.convert_timespec_time),
            f(self.foveation),
            f(self.frame_controller),
        )
    }
}

/// Optional features the app would like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtensionWishes {
    pub eye_gaze: bool,
    pub hand_tracking: bool,
    pub depth_layers: bool,
}

impl Default for ExtensionWishes {
    fn default() -> Self {
        ExtensionWishes {
            eye_gaze: true,
            hand_tracking: true,
            depth_layers: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enable_only_available_and_wanted() {
        let mut a = xr::ExtensionSet::default();
        a.khr_vulkan_enable2 = true;
        a.ext_hand_tracking = true;
        a.khr_composition_layer_depth = true;
        a.fb_foveation = true;
        let r = ExtensionReport::from_available(&a);
        assert!(r.vulkan_enable2 && r.hand_tracking && !r.eye_gaze_interaction && r.foveation);
        let want = ExtensionWishes {
            eye_gaze: true,
            hand_tracking: false,
            depth_layers: true,
        };
        let e = r.to_enable(&want);
        assert!(
            e.khr_vulkan_enable2
                && !e.ext_hand_tracking
                && !e.ext_eye_gaze_interaction
                && e.khr_composition_layer_depth
        );
        let en = r.enabled(&want);
        assert!(!en.hand_tracking && en.composition_layer_depth && !en.foveation);
        assert!(r.summary().contains("hand_tracking=yes"));
        assert!(!r.frame_controller && !en.frame_controller);
    }

    #[test]
    fn frame_controller_extension_round_trips() {
        let mut a = xr::ExtensionSet::default();
        a.khr_vulkan_enable2 = true;
        a.other.push(FRAME_CONTROLLER_EXTENSION.to_string());
        let r = ExtensionReport::from_available(&a);
        assert!(r.frame_controller);
        let e = r.to_enable(&ExtensionWishes::default());
        assert_eq!(e.other, ["XR_VALVE_frame_controller_interaction"]);
        assert!(r.enabled(&ExtensionWishes::default()).frame_controller);
        assert!(r.summary().contains("frame_controller=yes"));
    }
}
