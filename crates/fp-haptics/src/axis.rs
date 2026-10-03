//! Motion axes (TCode naming) and their mapping from script file names.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// One degree of freedom of an interactive device, named as in TCode v0.3.
///
/// Positions on every axis are normalised to `0.0..=1.0`. For the linear and
/// rotation axes 0.5 is the centre; for vibration-like axes 0 is off.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Axis {
    /// Stroke (up/down). The main axis; `video.funscript`.
    L0,
    /// Surge (forward/back); `video.surge.funscript`.
    L1,
    /// Sway (left/right); `video.sway.funscript`.
    L2,
    /// Twist; `video.twist.funscript`.
    R0,
    /// Roll; `video.roll.funscript`.
    R1,
    /// Pitch; `video.pitch.funscript`.
    R2,
    /// Vibration; `video.vib.funscript` or `video.vibrate.funscript`.
    V0,
    /// Pump / lube; `video.pump.funscript` or `video.lube.funscript`.
    V1,
    /// Valve; `video.valve.funscript`.
    A0,
    /// Suction; `video.suck.funscript`.
    A1,
}

impl Axis {
    /// Every axis, in TCode order.
    pub const ALL: [Axis; 10] = [
        Axis::L0,
        Axis::L1,
        Axis::L2,
        Axis::R0,
        Axis::R1,
        Axis::R2,
        Axis::V0,
        Axis::V1,
        Axis::A0,
        Axis::A1,
    ];

    /// The TCode channel name (`"L0"`, `"R1"`, ...).
    pub fn tcode(self) -> &'static str {
        match self {
            Axis::L0 => "L0",
            Axis::L1 => "L1",
            Axis::L2 => "L2",
            Axis::R0 => "R0",
            Axis::R1 => "R1",
            Axis::R2 => "R2",
            Axis::V0 => "V0",
            Axis::V1 => "V1",
            Axis::A0 => "A0",
            Axis::A1 => "A1",
        }
    }

    /// Human-readable name for the UI.
    pub fn label(self) -> &'static str {
        match self {
            Axis::L0 => "Stroke",
            Axis::L1 => "Surge",
            Axis::L2 => "Sway",
            Axis::R0 => "Twist",
            Axis::R1 => "Roll",
            Axis::R2 => "Pitch",
            Axis::V0 => "Vibrate",
            Axis::V1 => "Pump",
            Axis::A0 => "Valve",
            Axis::A1 => "Suck",
        }
    }

    /// The canonical file-name suffix (`video.<suffix>.funscript`), `None`
    /// for the stroke axis, which uses the plain `video.funscript`.
    pub fn script_suffix(self) -> Option<&'static str> {
        match self {
            Axis::L0 => None,
            Axis::L1 => Some("surge"),
            Axis::L2 => Some("sway"),
            Axis::R0 => Some("twist"),
            Axis::R1 => Some("roll"),
            Axis::R2 => Some("pitch"),
            Axis::V0 => Some("vib"),
            Axis::V1 => Some("pump"),
            Axis::A0 => Some("valve"),
            Axis::A1 => Some("suck"),
        }
    }

    /// Parses a TCode channel name, case-insensitively.
    pub fn from_tcode(s: &str) -> Option<Axis> {
        Axis::ALL
            .into_iter()
            .find(|a| a.tcode().eq_ignore_ascii_case(s.trim()))
    }

    /// Maps a script-name suffix (`"surge"`, `"vibrate"`, `"R1"`, ...) to an
    /// axis, case-insensitively. Accepts the common aliases and TCode names.
    pub fn from_suffix(s: &str) -> Option<Axis> {
        let s = s.trim().to_ascii_lowercase();
        let axis = match s.as_str() {
            "stroke" | "l0" => Axis::L0,
            "surge" | "l1" => Axis::L1,
            "sway" | "l2" => Axis::L2,
            "twist" | "r0" => Axis::R0,
            "roll" | "r1" => Axis::R1,
            "pitch" | "r2" => Axis::R2,
            "vib" | "vibe" | "vibrate" | "v0" => Axis::V0,
            "pump" | "lube" | "v1" => Axis::V1,
            "valve" | "a0" => Axis::A0,
            "suck" | "suction" | "a1" => Axis::A1,
            _ => return None,
        };
        Some(axis)
    }

    /// Whether this is a positional axis (linear or rotation) whose resting
    /// position is the centre, as opposed to an intensity axis (V*, A*).
    pub fn is_positional(self) -> bool {
        matches!(
            self,
            Axis::L0 | Axis::L1 | Axis::L2 | Axis::R0 | Axis::R1 | Axis::R2
        )
    }
}

impl fmt::Display for Axis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.tcode())
    }
}

impl FromStr for Axis {
    type Err = crate::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Axis::from_tcode(s)
            .or_else(|| Axis::from_suffix(s))
            .ok_or_else(|| crate::Error::Config(format!("unknown axis {s:?}")))
    }
}

/// Splits a funscript file name into its base (the video's stem) and the
/// axis its suffix names.
///
/// `video.funscript` is the stroke axis (L0); `video.surge.funscript` is L1,
/// and so on (see [`Axis::from_suffix`]). A dotted stem whose last part is
/// not an axis name (`my.video.funscript`) is the stroke axis of
/// `my.video`. Returns `None` when the name does not end in `.funscript`.
pub fn split_script_name(file_name: &str) -> Option<(&str, Axis)> {
    let (stem, ext) = file_name.rsplit_once('.')?;
    if !ext.eq_ignore_ascii_case("funscript") || stem.is_empty() {
        return None;
    }
    let suffixed = stem
        .rsplit_once('.')
        .filter(|(base, _)| !base.is_empty())
        .and_then(|(base, suffix)| Axis::from_suffix(suffix).map(|axis| (base, axis)));
    Some(suffixed.unwrap_or((stem, Axis::L0)))
}

/// The axis a funscript file drives, from its name (see
/// [`split_script_name`]). Defaults to the stroke axis for unrecognised
/// names.
pub fn axis_for_script_name(file_name: &str) -> Axis {
    split_script_name(file_name)
        .map(|(_, a)| a)
        .unwrap_or(Axis::L0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_file_names_to_axes() {
        let cases = [
            ("video.funscript", "video", Axis::L0),
            ("video.surge.funscript", "video", Axis::L1),
            ("video.sway.funscript", "video", Axis::L2),
            ("video.twist.funscript", "video", Axis::R0),
            ("video.roll.funscript", "video", Axis::R1),
            ("video.pitch.funscript", "video", Axis::R2),
            ("video.vib.funscript", "video", Axis::V0),
            ("video.vibrate.funscript", "video", Axis::V0),
            ("video.pump.funscript", "video", Axis::V1),
            ("video.lube.funscript", "video", Axis::V1),
            ("video.valve.funscript", "video", Axis::A0),
            ("video.suck.funscript", "video", Axis::A1),
            (
                "My.Video_180_LR.ROLL.FunScript",
                "My.Video_180_LR",
                Axis::R1,
            ),
            ("my.video.funscript", "my.video", Axis::L0),
            ("clip.R2.funscript", "clip", Axis::R2),
        ];
        for (name, base, axis) in cases {
            assert_eq!(split_script_name(name), Some((base, axis)), "{name}");
        }
        assert_eq!(split_script_name("video.mp4"), None);
        assert_eq!(split_script_name(".funscript"), None);
        assert_eq!(
            split_script_name(".roll.funscript"),
            Some((".roll", Axis::L0))
        );
        assert_eq!(axis_for_script_name("whatever"), Axis::L0);
    }

    #[test]
    fn parses_axis_names() {
        assert_eq!("r1".parse::<Axis>().unwrap(), Axis::R1);
        assert_eq!("twist".parse::<Axis>().unwrap(), Axis::R0);
        assert!("X9".parse::<Axis>().is_err());
        for a in Axis::ALL {
            assert_eq!(Axis::from_tcode(a.tcode()), Some(a));
            if let Some(s) = a.script_suffix() {
                assert_eq!(Axis::from_suffix(s), Some(a));
            }
        }
    }
}
