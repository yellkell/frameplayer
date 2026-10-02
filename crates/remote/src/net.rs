//! LAN-only access policy, token generation and local address discovery.

use rand::Rng;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};

/// Whether a peer address is on the local network: loopback, RFC 1918 private ranges,
/// link-local (IPv4 169.254/16, IPv6 fe80::/10) or IPv6 unique-local (fc00::/7). IPv4-mapped
/// IPv6 addresses are judged by their IPv4 part. Everything else (including CGNAT and public
/// addresses) is rejected.
pub fn is_lan_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_lan_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_lan_v4(v4);
            }
            let seg0 = v6.segments()[0];
            v6.is_loopback() || (seg0 & 0xfe00) == 0xfc00 || (seg0 & 0xffc0) == 0xfe80
        }
    }
}

fn is_lan_v4(v4: Ipv4Addr) -> bool {
    v4.is_loopback() || v4.is_private() || v4.is_link_local()
}

/// Best-effort LAN address of this machine, for the pairing URL. Uses the "connect a UDP
/// socket" trick (no packet is sent) to find the interface holding the default route.
pub fn detect_lan_ip() -> Option<IpAddr> {
    let probe = |bind: &str, target: SocketAddr| -> Option<IpAddr> {
        let s = UdpSocket::bind(bind).ok()?;
        s.connect(target).ok()?;
        let ip = s.local_addr().ok()?.ip();
        (!ip.is_unspecified() && is_lan_ip(ip) && !ip.is_loopback()).then_some(ip)
    };
    probe(
        "0.0.0.0:0",
        SocketAddr::from((Ipv4Addr::new(192, 0, 2, 1), 9)),
    )
    .or_else(|| {
        probe(
            "0.0.0.0:0",
            SocketAddr::from((Ipv4Addr::new(10, 255, 255, 255), 9)),
        )
    })
    .or_else(|| {
        probe(
            "[::]:0",
            SocketAddr::from((Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 1), 9)),
        )
    })
}

/// A fresh random 32-character alphanumeric access token (~190 bits).
pub fn generate_token() -> String {
    rand::thread_rng()
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(32)
        .map(char::from)
        .collect()
}

/// Constant-time token comparison (length is not secret).
pub fn tokens_match(expected: &str, given: &str) -> bool {
    let (a, b) = (expected.as_bytes(), given.as_bytes());
    if a.len() != b.len() || a.is_empty() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lan_classification() {
        for ok in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.20",
            "169.254.3.4",
            "::1",
            "fd12:3456::1",
            "fc00::1",
            "fe80::1",
            "::ffff:192.168.0.5",
        ] {
            assert!(is_lan_ip(ok.parse().unwrap()), "{ok} should be LAN");
        }
        for bad in [
            "8.8.8.8",
            "172.32.0.1",
            "100.64.0.1",
            "1.1.1.1",
            "2001:db8::1",
            "::ffff:8.8.8.8",
            "ff02::1",
        ] {
            assert!(!is_lan_ip(bad.parse().unwrap()), "{bad} should not be LAN");
        }
    }

    #[test]
    fn tokens() {
        let t = generate_token();
        assert_eq!(t.len(), 32);
        assert!(t.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(t, generate_token());
        assert!(tokens_match(&t, &t.clone()));
        assert!(!tokens_match(&t, "nope"));
        assert!(!tokens_match("", ""));
    }
}
