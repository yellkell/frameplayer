//! User settings for script playback, serialisable so the app can persist
//! them.

use crate::axis::Axis;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Per-axis output mapping.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AxisSettings {
    /// When false nothing is sent on this axis.
    pub enabled: bool,
    /// Output position for script position 0, `0.0..=1.0`.
    pub min: f32,
    /// Output position for script position 100, `0.0..=1.0`. May be below
    /// `min`, which also inverts.
    pub max: f32,
    /// Flip the script (`1 - pos`) before applying `min`/`max`.
    pub invert: bool,
}

impl Default for AxisSettings {
    fn default() -> Self {
        AxisSettings {
            enabled: true,
            min: 0.0,
            max: 1.0,
            invert: false,
        }
    }
}

impl AxisSettings {
    /// Maps a script position (`0..=1`) to the output position.
    pub fn apply(&self, pos: f32) -> f32 {
        let p = pos.clamp(0.0, 1.0);
        let p = if self.invert { 1.0 - p } else { p };
        let (min, max) = (self.min.clamp(0.0, 1.0), self.max.clamp(0.0, 1.0));
        (min + (max - min) * p).clamp(0.0, 1.0)
    }
}

/// Per-device switches, keyed by device name in [`HapticsSettings::devices`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceSettings {
    /// When false the device is left alone (and stopped when switched off
    /// during playback).
    pub enabled: bool,
    /// Axes this device should not receive.
    pub disabled_axes: BTreeSet<Axis>,
}

impl Default for DeviceSettings {
    fn default() -> Self {
        DeviceSettings {
            enabled: true,
            disabled_axes: BTreeSet::new(),
        }
    }
}

impl DeviceSettings {
    /// Whether `axis` is enabled for this device.
    pub fn axis_enabled(&self, axis: Axis) -> bool {
        !self.disabled_axes.contains(&axis)
    }
}

/// Everything the user can tune about script playback.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HapticsSettings {
    /// Added to the video position to get the script time, in
    /// milliseconds. Positive values make devices act earlier, which
    /// compensates for device and transport latency.
    pub offset_ms: i64,
    /// Output mapping per axis; axes not listed use
    /// [`AxisSettings::default`].
    pub axes: BTreeMap<Axis, AxisSettings>,
    /// Maximum speed of positional axes in funscript units (0–100) per
    /// second; moves that would be faster are shortened. 0 disables the
    /// limit.
    pub speed_limit: f32,
    /// Per-device switches keyed by [`crate::Device::name`]; devices not
    /// listed use [`DeviceSettings::default`].
    pub devices: BTreeMap<String, DeviceSettings>,
    /// Drive the vibration axis (V0) from the stroke script's speed when no
    /// vibration script is loaded, so vibrators work with ordinary
    /// single-axis scripts.
    pub stroke_to_vibration: bool,
}

impl Default for HapticsSettings {
    fn default() -> Self {
        HapticsSettings {
            offset_ms: 0,
            axes: BTreeMap::new(),
            speed_limit: 0.0,
            devices: BTreeMap::new(),
            stroke_to_vibration: true,
        }
    }
}

impl HapticsSettings {
    /// Settings for `axis` (defaults when unset).
    pub fn axis(&self, axis: Axis) -> AxisSettings {
        self.axes.get(&axis).copied().unwrap_or_default()
    }

    /// Mutable settings for `axis`, inserting defaults when unset.
    pub fn axis_mut(&mut self, axis: Axis) -> &mut AxisSettings {
        self.axes.entry(axis).or_default()
    }

    /// Settings for the device called `name` (defaults when unset).
    pub fn device(&self, name: &str) -> DeviceSettings {
        self.devices.get(name).cloned().unwrap_or_default()
    }

    /// Mutable settings for the device called `name`, inserting defaults
    /// when unset.
    pub fn device_mut(&mut self, name: &str) -> &mut DeviceSettings {
        self.devices.entry(name.to_owned()).or_default()
    }

    /// Whether `axis` should be sent to the device called `name`.
    pub fn sends(&self, name: &str, axis: Axis) -> bool {
        self.axis(axis).enabled
            && self
                .devices
                .get(name)
                .is_none_or(|d| d.enabled && d.axis_enabled(axis))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn axis_mapping() {
        let s = AxisSettings {
            min: 0.2,
            max: 0.8,
            ..Default::default()
        };
        assert!((s.apply(0.0) - 0.2).abs() < 1e-6);
        assert!((s.apply(1.0) - 0.8).abs() < 1e-6);
        assert!((s.apply(0.5) - 0.5).abs() < 1e-6);
        let inv = AxisSettings { invert: true, ..s };
        assert!((inv.apply(1.0) - 0.2).abs() < 1e-6);
        assert!((inv.apply(2.0) - 0.2).abs() < 1e-6);
    }

    #[test]
    fn json_round_trip_and_partial() {
        let mut s = HapticsSettings {
            offset_ms: -40,
            speed_limit: 300.0,
            ..Default::default()
        };
        s.axis_mut(Axis::R1).invert = true;
        s.device_mut("OSR2").disabled_axes.insert(Axis::R2);
        s.device_mut("Handy").enabled = false;
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"R1\""), "{json}");
        let back: HapticsSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
        assert!(!back.sends("Handy", Axis::L0));
        assert!(!back.sends("OSR2", Axis::R2));
        assert!(back.sends("OSR2", Axis::R1));
        assert!(back.sends("unknown", Axis::L0));

        let partial: HapticsSettings = serde_json::from_str(r#"{"offset_ms":25}"#).unwrap();
        assert_eq!(partial.offset_ms, 25);
        assert!(partial.stroke_to_vibration);
        assert_eq!(partial.axis(Axis::L0), AxisSettings::default());
    }
}
