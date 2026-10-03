//! Headset discovery over mDNS/DNS-SD (`_steamos-devkit._tcp.local.`), the
//! service type `steamos-devkit-service` advertises in Developer Mode.
// [verify] The Frame advertises the same service type as the Deck.

use crate::devkit::DevkitProperties;
use anyhow::Result;
use std::net::IpAddr;
use std::time::{Duration, Instant};

pub const SERVICE_TYPE: &str = "_steamos-devkit._tcp.local.";

/// A headset found on the LAN.
#[derive(Debug, Clone, PartialEq)]
pub struct Discovered {
    /// Instance name, e.g. `steamframe` (before `._steamos-devkit…`).
    pub name: String,
    pub hostname: String,
    pub addresses: Vec<IpAddr>,
    pub port: u16,
    pub properties: DevkitProperties,
}

impl Discovered {
    /// Best address to connect to: IPv4 first (link-local IPv6 needs a scope id).
    pub fn best_address(&self) -> Option<IpAddr> {
        self.addresses
            .iter()
            .find(|a| a.is_ipv4())
            .or_else(|| self.addresses.first())
            .copied()
    }
}

/// Strip the service suffix from a DNS-SD full name.
pub fn instance_name(fullname: &str) -> String {
    fullname
        .strip_suffix(SERVICE_TYPE)
        .map(|s| s.trim_end_matches('.'))
        .unwrap_or(fullname)
        .replace("\\032", " ")
}

/// Browse for `timeout`, returning every resolved headset (deduplicated).
pub fn browse(timeout: Duration) -> Result<Vec<Discovered>> {
    browse_until(timeout, None)
}

/// Like [`browse`], but with `settle = Some(d)` return `d` after the first
/// headset resolves (so a found headset doesn't wait out the full timeout).
pub fn browse_until(timeout: Duration, settle: Option<Duration>) -> Result<Vec<Discovered>> {
    let daemon = mdns_sd::ServiceDaemon::new()?;
    let rx = daemon.browse(SERVICE_TYPE)?;
    let mut deadline = Instant::now() + timeout;
    let mut found: Vec<Discovered> = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(mdns_sd::ServiceEvent::ServiceResolved(info)) => {
                let mut addrs: Vec<IpAddr> = info.get_addresses().iter().copied().collect();
                addrs.sort();
                let d = Discovered {
                    name: instance_name(info.get_fullname()),
                    hostname: info.get_hostname().trim_end_matches('.').to_string(),
                    addresses: addrs,
                    port: info.get_port(),
                    properties: DevkitProperties::from_txt(
                        info.get_properties().clone().into_property_map_str(),
                    ),
                };
                found.retain(|f| f.name != d.name);
                found.push(d);
                if let Some(settle) = settle {
                    deadline = deadline.min(Instant::now() + settle);
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = daemon.stop_browse(SERVICE_TYPE);
    let _ = daemon.shutdown();
    found.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_names() {
        assert_eq!(
            instance_name("steamframe._steamos-devkit._tcp.local."),
            "steamframe"
        );
        assert_eq!(
            instance_name("My\\032Frame._steamos-devkit._tcp.local."),
            "My Frame"
        );
        assert_eq!(instance_name("odd"), "odd");
    }

    #[test]
    fn prefers_ipv4() {
        let d = Discovered {
            name: "f".into(),
            hostname: "f.local".into(),
            addresses: vec!["fe80::1".parse().unwrap(), "192.168.1.9".parse().unwrap()],
            port: 32000,
            properties: Default::default(),
        };
        assert_eq!(d.best_address().unwrap().to_string(), "192.168.1.9");
    }
}
