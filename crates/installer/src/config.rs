//! Local state: paired devices, SSH key location, known_hosts.
//!
//! Stored in the platform config dir (`~/.config/frameplayer-install` on
//! Linux, `~/Library/Application Support/frameplayer-install` on macOS,
//! `%APPDATA%\frameplayer-install` on Windows).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Default `steamos-devkit-service` HTTP port.
// [verify] 32000 is the port used by ValveSoftware/steamos-devkit's service on
// the Steam Deck; confirm the Frame's service uses the same.
pub const DEVKIT_SERVICE_PORT: u16 = 32000;

/// A headset we have paired with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    /// Friendly name (mDNS instance name or the host).
    pub name: String,
    pub host: String,
    #[serde(default = "default_service_port")]
    pub service_port: u16,
    #[serde(default = "default_ssh_port")]
    pub ssh_port: u16,
    /// Remote login reported by the devkit service.
    pub user: String,
}

fn default_service_port() -> u16 {
    DEVKIT_SERVICE_PORT
}
fn default_ssh_port() -> u16 {
    22
}

/// Persistent installer state (`devices.json`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub devices: Vec<Device>,
    /// Name of the device used when none is given.
    #[serde(default)]
    pub default_device: Option<String>,
}

impl State {
    /// Insert or replace (matched by host) and make it the default.
    pub fn upsert(&mut self, d: Device) {
        self.devices
            .retain(|x| x.host != d.host && x.name != d.name);
        self.default_device = Some(d.name.clone());
        self.devices.push(d);
    }

    /// Look up by name or host; `None` selects the default (or the only) device.
    pub fn find(&self, which: Option<&str>) -> Option<&Device> {
        match which {
            Some(w) => self.devices.iter().find(|d| d.name == w || d.host == w),
            None => self
                .default_device
                .as_deref()
                .and_then(|n| self.devices.iter().find(|d| d.name == n))
                .or_else(|| (self.devices.len() == 1).then(|| &self.devices[0])),
        }
    }

    pub fn remove(&mut self, which: &str) -> bool {
        let before = self.devices.len();
        self.devices.retain(|d| d.name != which && d.host != which);
        if self.default_device.as_deref() == Some(which) {
            self.default_device = None;
        }
        before != self.devices.len()
    }
}

/// Paths of everything the installer keeps on the desktop side.
#[derive(Debug, Clone)]
pub struct Paths {
    pub dir: PathBuf,
}

impl Paths {
    /// Platform default, overridable with `FRAMEPLAYER_INSTALL_HOME`.
    pub fn platform_default() -> Result<Self> {
        if let Some(d) = std::env::var_os("FRAMEPLAYER_INSTALL_HOME") {
            return Ok(Self {
                dir: PathBuf::from(d),
            });
        }
        let base = dirs::config_dir().context("no config directory on this platform")?;
        Ok(Self {
            dir: base.join("frameplayer-install"),
        })
    }

    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn ssh_key(&self) -> PathBuf {
        self.dir.join("id_ed25519")
    }
    pub fn ssh_pubkey(&self) -> PathBuf {
        self.dir.join("id_ed25519.pub")
    }
    pub fn known_hosts(&self) -> PathBuf {
        self.dir.join("known_hosts")
    }
    pub fn state_file(&self) -> PathBuf {
        self.dir.join("devices.json")
    }
    pub fn downloads(&self) -> PathBuf {
        self.dir.join("downloads")
    }

    pub fn load_state(&self) -> Result<State> {
        load_state(&self.state_file())
    }

    pub fn save_state(&self, s: &State) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let tmp = self.state_file().with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(s)? + "\n")?;
        std::fs::rename(&tmp, self.state_file())?;
        Ok(())
    }
}

fn load_state(p: &Path) -> Result<State> {
    match std::fs::read(p) {
        Ok(b) => serde_json::from_slice(&b).with_context(|| format!("parsing {}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str, host: &str) -> Device {
        Device {
            name: name.into(),
            host: host.into(),
            service_port: 32000,
            ssh_port: 22,
            user: "steamos".into(),
        }
    }

    #[test]
    fn state_roundtrip_and_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::at(dir.path());
        assert_eq!(p.load_state().unwrap(), State::default());
        let mut s = State::default();
        s.upsert(dev("frame", "192.168.1.20"));
        assert_eq!(s.find(None).unwrap().host, "192.168.1.20");
        s.upsert(dev("frame2", "192.168.1.21"));
        assert_eq!(s.find(None).unwrap().name, "frame2");
        assert_eq!(s.find(Some("192.168.1.20")).unwrap().name, "frame");
        // Re-pairing the same host replaces the entry.
        s.upsert(dev("frame-renamed", "192.168.1.20"));
        assert_eq!(s.devices.len(), 2);
        p.save_state(&s).unwrap();
        assert_eq!(p.load_state().unwrap(), s);
        assert!(s.remove("frame2"));
        assert_eq!(s.find(None).unwrap().name, "frame-renamed");
    }

    #[test]
    fn device_defaults_when_missing() {
        let d: Device = serde_json::from_str(r#"{"name":"f","host":"h","user":"u"}"#).unwrap();
        assert_eq!(d.service_port, 32000);
        assert_eq!(d.ssh_port, 22);
    }
}
