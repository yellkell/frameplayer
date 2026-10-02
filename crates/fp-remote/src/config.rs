//! Remote-control settings, persisted by the app as part of its config.

use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;

/// Port DeoVR listens on for its remote-control API; tools such as ohdoki,
/// MultiFunPlayer and ScriptPlayer connect here by default.
pub const DEFAULT_DEOVR_PORT: u16 = 23554;

/// Default port of the LAN web remote.
pub const DEFAULT_WEB_PORT: u16 = 8790;

/// Settings for [`crate::RemoteHub`].
///
/// Both servers are off by default. The `*_enabled` flags only record the
/// user's choice; the hub never starts a server on its own, the app calls
/// [`crate::RemoteHub::start_deovr_server`] / [`crate::RemoteHub::start_web`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteConfig {
    /// User wants the DeoVR-compatible TCP API running.
    pub deovr_enabled: bool,
    /// User wants the web remote running.
    pub web_enabled: bool,
    /// Interface to listen on. The default (all IPv4 interfaces) is safe
    /// because peers outside loopback and private/link-local ranges are
    /// rejected regardless.
    pub bind_address: IpAddr,
    /// DeoVR API port; 0 picks a free port (tests).
    pub deovr_port: u16,
    /// Web remote port; 0 picks a free port (tests).
    pub web_port: u16,
    /// Where the web remote's pairing token is persisted. `None` keeps a
    /// fresh token in memory only (every launch needs re-pairing).
    pub token_path: Option<PathBuf>,
    /// Maximum simultaneous DeoVR API clients; extra connections are closed.
    pub max_deovr_clients: usize,
    /// Maximum simultaneous live-status streams (open web remote pages).
    pub max_event_streams: usize,
}

impl Default for RemoteConfig {
    fn default() -> Self {
        RemoteConfig {
            deovr_enabled: false,
            web_enabled: false,
            bind_address: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            deovr_port: DEFAULT_DEOVR_PORT,
            web_port: DEFAULT_WEB_PORT,
            token_path: None,
            max_deovr_clients: 16,
            max_event_streams: 8,
        }
    }
}

impl RemoteConfig {
    /// The conventional token location: `$XDG_CONFIG_HOME/frameplayer/remote-token`.
    pub fn default_token_path() -> PathBuf {
        fp_core::dirs::config_dir().join("remote-token")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_off_and_partial_json_fills_in() {
        let c = RemoteConfig::default();
        assert!(!c.deovr_enabled && !c.web_enabled);
        assert_eq!(c.deovr_port, 23554);
        assert_eq!(c.web_port, 8790);
        let c: RemoteConfig = serde_json::from_str(r#"{"web_enabled":true}"#).unwrap();
        assert!(c.web_enabled);
        assert_eq!(c.max_deovr_clients, 16);
        let j = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<RemoteConfig>(&j).unwrap(), c);
    }
}
