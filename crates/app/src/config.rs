//! User configuration, persisted as TOML in `$XDG_CONFIG_HOME/frameplayer/config.toml`.
//!
//! Every field has a default so a missing or partial file is never an error;
//! unknown keys are ignored so older builds can read newer configs.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: General,
    pub playback: Playback,
    pub comfort: Comfort,
    pub remote: Remote,
    pub haptics: Haptics,
    pub updates: Updates,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    /// Directories scanned into the library on start (local paths or source URIs).
    pub library_roots: Vec<String>,
    /// Requested display refresh rate in Hz; 0 = runtime default. [verify] which
    /// rates SteamVR on the Frame offers to native apps.
    pub refresh_rate_hz: f32,
    pub log_level: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Playback {
    pub default_speed: f32,
    /// Audio/video offset in milliseconds (positive delays audio).
    pub av_offset_ms: i32,
    pub resume_playback: bool,
    pub seek_step_s: f32,
    pub subtitle_depth_m: f32,
    pub preferred_audio_language: Option<String>,
    pub preferred_subtitle_language: Option<String>,
    /// Cap for software decode, in pixels of the larger dimension.
    pub software_decode_max: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Comfort {
    /// Keep the flat screen locked to the head instead of the world.
    pub head_locked_screen: bool,
    /// Lying-down mode: gravity override so "up" follows the head.
    pub lying_down: bool,
    /// Environment brightness, 0..1.
    pub environment_dim: f32,
    /// Show passthrough behind the UI / flat screen when the system allows it.
    pub passthrough_background: bool,
    /// Dim the UI when the user's gaze leaves it.
    pub gaze_dimming: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Remote {
    /// Our REST/WS API + LAN web remote. Off by default (§3.6).
    pub api_enabled: bool,
    pub api_port: u16,
    /// Generated on first enable; never logged.
    pub api_token: Option<String>,
    /// DeoVR-compatible TCP remote (ohdoki, script players). Off by default.
    pub deovr_enabled: bool,
    pub deovr_port: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Haptics {
    pub enabled: bool,
    /// "handy", "buttplug", "tcode" or empty.
    pub backend: String,
    pub handy_connection_key: Option<String>,
    pub buttplug_url: String,
    pub tcode_address: Option<String>,
    pub offset_ms: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Updates {
    pub check_on_start: bool,
    /// "stable" or "beta".
    pub channel: String,
    pub manifest_url: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            general: General::default(),
            playback: Playback::default(),
            comfort: Comfort::default(),
            remote: Remote::default(),
            haptics: Haptics::default(),
            updates: Updates::default(),
        }
    }
}

impl Default for General {
    fn default() -> Self {
        General { library_roots: Vec::new(), refresh_rate_hz: 0.0, log_level: "info".into() }
    }
}

impl Default for Playback {
    fn default() -> Self {
        Playback {
            default_speed: 1.0,
            av_offset_ms: 0,
            resume_playback: true,
            seek_step_s: 10.0,
            subtitle_depth_m: 2.0,
            preferred_audio_language: None,
            preferred_subtitle_language: None,
            software_decode_max: 3840,
        }
    }
}

impl Default for Comfort {
    fn default() -> Self {
        Comfort { head_locked_screen: false, lying_down: false, environment_dim: 0.2, passthrough_background: false, gaze_dimming: true }
    }
}

impl Default for Remote {
    fn default() -> Self {
        Remote { api_enabled: false, api_port: 8642, api_token: None, deovr_enabled: false, deovr_port: 23554 }
    }
}

impl Default for Haptics {
    fn default() -> Self {
        Haptics {
            enabled: false,
            backend: String::new(),
            handy_connection_key: None,
            buttplug_url: "ws://127.0.0.1:12345".into(),
            tcode_address: None,
            offset_ms: 0,
        }
    }
}

impl Default for Updates {
    fn default() -> Self {
        Updates {
            check_on_start: true,
            channel: "stable".into(),
            manifest_url: "https://github.com/yellkell/frameplayer/releases/latest/download/manifest.json".into(),
        }
    }
}

/// Standard locations, honouring XDG variables with `$HOME` fallbacks.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
}

impl Paths {
    pub fn from_env() -> Paths {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        let xdg = |var: &str, fallback: &str| std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home.join(fallback));
        Paths {
            config_dir: xdg("XDG_CONFIG_HOME", ".config").join("frameplayer"),
            data_dir: xdg("XDG_DATA_HOME", ".local/share").join("frameplayer"),
            cache_dir: xdg("XDG_CACHE_HOME", ".cache").join("frameplayer"),
        }
    }

    pub fn under(root: &Path) -> Paths {
        Paths { config_dir: root.join("config"), data_dir: root.join("data"), cache_dir: root.join("cache") }
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn create_all(&self) -> Result<()> {
        for d in [&self.config_dir, &self.data_dir, &self.cache_dir] {
            std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
        }
        Ok(())
    }
}

impl Config {
    /// Load, falling back to defaults if the file does not exist. A file that
    /// fails to parse is moved aside (so the user's edits aren't lost) and
    /// defaults are used.
    pub fn load(path: &Path) -> Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(s) => match toml::from_str(&s) {
                Ok(c) => Ok(c),
                Err(e) => {
                    let bad = path.with_extension("toml.bad");
                    tracing::warn!("config {} is invalid ({e}); moved to {}", path.display(), bad.display());
                    let _ = std::fs::rename(path, &bad);
                    Ok(Config::default())
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Write atomically (temp file + rename).
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, toml::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_file_uses_defaults() {
        let c: Config = toml::from_str("[playback]\ndefault_speed = 1.5\n[future_section]\nx = 1\n").unwrap();
        assert_eq!(c.playback.default_speed, 1.5);
        assert_eq!(c.playback.seek_step_s, 10.0);
        assert!(!c.remote.api_enabled);
    }

    #[test]
    fn save_load_roundtrip_and_bad_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        assert_eq!(Config::load(&p).unwrap(), Config::default());
        let mut c = Config::default();
        c.general.library_roots.push("/home/deck/Videos".into());
        c.save(&p).unwrap();
        assert_eq!(Config::load(&p).unwrap(), c);
        std::fs::write(&p, "this is = = not toml").unwrap();
        assert_eq!(Config::load(&p).unwrap(), Config::default());
        assert!(p.with_extension("toml.bad").exists());
    }
}
