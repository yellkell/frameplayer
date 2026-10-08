//! Persistent user settings (`~/.config/frameplayer/settings.json`).
//! Source credentials live separately in `sources.json` (mode 0600).

use fp_core::view::ViewSettings;
use fp_haptics::HapticsSettings;
use fp_remote::config::RemoteConfig;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A haptic device the user added.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HapticDeviceConfig {
    /// TCode over serial (`/dev/ttyACM0`), `tcp://host:port` or `udp://host:port`.
    Tcode { endpoint: String },
    /// Intiface Central / Buttplug server.
    Buttplug { url: String },
    /// The Handy, by connection key.
    Handy { key: String },
}

impl HapticDeviceConfig {
    pub fn label(&self) -> String {
        match self {
            HapticDeviceConfig::Tcode { endpoint } => format!("TCode {endpoint}"),
            HapticDeviceConfig::Buttplug { url } => format!("Intiface {url}"),
            HapticDeviceConfig::Handy { key } => {
                let shown: String = key.chars().take(3).collect();
                format!("The Handy ({shown}…)")
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Local folders indexed into the library.
    pub library_folders: Vec<PathBuf>,
    /// Also index microSD cards and USB drives when they are mounted.
    pub index_removable: bool,
    /// Use the V4L2 hardware decoder when available.
    pub hardware_decoding: bool,
    /// ALSA device ("default" routes to PipeWire on SteamOS).
    pub audio_device: String,
    pub volume: f32,
    /// Defaults for videos without saved adjustments.
    pub default_view: ViewSettings,
    /// Show the real world around flat screens and the UI.
    pub passthrough: bool,
    /// Resume videos where they were left.
    pub resume: bool,
    /// Seconds of inactivity before the playback UI hides.
    pub auto_hide_secs: f32,
    /// Thumbstick seek step in seconds.
    pub seek_step: f64,
    pub remote: RemoteConfig,
    pub haptics: HapticsSettings,
    pub haptic_devices: Vec<HapticDeviceConfig>,
    /// "stable" or "beta".
    pub update_channel: String,
    pub check_updates: bool,
    /// UI text size multiplier.
    pub ui_scale: f32,
    /// What the controller buttons and thumbsticks do.
    pub controls: crate::bindings::Bindings,
    /// The passthrough videos licence from yellkell.com/unlock.
    pub unlock: Option<String>,
    /// Where the library and adjust panels were moved to: yaw and pitch in
    /// degrees from their usual places (left and up positive).
    pub panel_offsets: PanelOffsets,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PanelOffsets {
    pub main: [f32; 2],
    pub adjust: [f32; 2],
}

impl Default for Settings {
    fn default() -> Self {
        let mut library_folders = Vec::new();
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            for d in ["Videos", "Downloads"] {
                let p = home.join(d);
                if p.is_dir() {
                    library_folders.push(p);
                }
            }
        }
        Settings {
            library_folders,
            index_removable: true,
            hardware_decoding: true,
            audio_device: "default".into(),
            volume: 1.0,
            default_view: ViewSettings::default(),
            passthrough: false,
            resume: true,
            auto_hide_secs: 4.0,
            seek_step: 10.0,
            remote: RemoteConfig::default(),
            haptics: HapticsSettings::default(),
            haptic_devices: Vec::new(),
            update_channel: "stable".into(),
            check_updates: true,
            ui_scale: 1.0,
            controls: crate::bindings::Bindings::default(),
            unlock: None,
            panel_offsets: PanelOffsets::default(),
        }
    }
}

impl Settings {
    /// Adjustments for a video that has none saved: the defaults, following
    /// the global chroma key.
    pub fn new_video_view(&self) -> ViewSettings {
        ViewSettings {
            key_own: Some(false),
            ..self.default_view
        }
    }

    pub fn path() -> PathBuf {
        fp_core::dirs::config_dir().join("settings.json")
    }

    pub fn sources_path() -> PathBuf {
        fp_core::dirs::config_dir().join("sources.json")
    }

    /// Loads settings, falling back to defaults (and logging why) when the
    /// file is missing or unreadable.
    pub fn load(path: &Path) -> Settings {
        match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str::<Settings>(&text) {
                Ok(mut s) => {
                    // Saved before the global chroma key existed: its key
                    // was never chosen, so take the current default.
                    if s.default_view.key_own.is_none() {
                        s.default_view.set_key(&ViewSettings::default());
                        s.default_view.key_own = Some(false);
                    }
                    // Saved before height existed: the right grip's up / down
                    // was free then, so it gets the new default.
                    let before_height = serde_json::from_str::<serde_json::Value>(&text)
                        .ok()
                        .is_some_and(|v| v["default_view"].get("height").is_none());
                    if before_height
                        && s.controls.right.grip_y == crate::bindings::AxisAction::Nothing
                    {
                        s.controls.right.grip_y = crate::bindings::AxisAction::Height;
                    }
                    s
                }
                Err(e) => {
                    log::warn!("settings {}: {e}; using defaults", path.display());
                    Settings::default()
                }
            },
            Err(_) => Settings::default(),
        }
    }

    /// Writes atomically (temp file + rename).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_partial_files() {
        let dir = std::env::temp_dir().join(format!("fp-settings-{}", std::process::id()));
        let path = dir.join("settings.json");
        let mut s = Settings {
            volume: 0.5,
            ..Default::default()
        };
        s.haptic_devices.push(HapticDeviceConfig::Tcode {
            endpoint: "tcp://10.0.0.2:8000".into(),
        });
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path), s);
        std::fs::write(&path, r#"{"volume": 0.25}"#).unwrap();
        let p = Settings::load(&path);
        assert_eq!(p.volume, 0.25);
        assert_eq!(p.seek_step, 10.0);
        // Saved before the global chroma key: the old green default goes.
        std::fs::write(
            &path,
            r#"{"default_view": {"zoom": 1.5, "key_color": [0.0, 1.0, 0.0]}}"#,
        )
        .unwrap();
        let p = Settings::load(&path);
        assert_eq!(p.default_view.zoom, 1.5);
        assert_eq!(p.default_view.key_color, ViewSettings::default().key_color);
        assert_eq!(p.default_view.key_own, Some(false));
        // Saved before height: the right grip's free up / down gets it; one
        // set to nothing since stays so.
        let old =
            r#"{"default_view": {"zoom": 1.0}, "controls": {"right": {"grip_y": "nothing"}}}"#;
        std::fs::write(&path, old).unwrap();
        let height = crate::bindings::AxisAction::Height;
        assert_eq!(Settings::load(&path).controls.right.grip_y, height);
        let mut s = Settings::default();
        s.controls.right.grip_y = crate::bindings::AxisAction::Nothing;
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path), s);
        std::fs::write(&path, "garbage").unwrap();
        assert_eq!(Settings::load(&path).volume, 1.0);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
