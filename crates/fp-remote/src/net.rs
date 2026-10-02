//! LAN address helpers: which peers may talk to us, and which addresses the
//! headset can be reached on.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Whether a connection from `ip` is allowed: loopback, private IPv4
/// (10/8, 172.16/12, 192.168/16), IPv4 link-local (169.254/16), IPv6
/// unique-local (fc00::/7) and link-local (fe80::/10), including IPv4
/// addresses mapped into IPv6. Everything else, public addresses in
/// particular, is rejected.
pub fn is_allowed_peer(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => allowed_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => allowed_v4(v4),
            None => allowed_v6(v6),
        },
    }
}

fn allowed_v4(ip: Ipv4Addr) -> bool {
    ip.is_loopback() || ip.is_private() || ip.is_link_local()
}

fn allowed_v6(ip: Ipv6Addr) -> bool {
    let first = ip.segments()[0];
    ip.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
}

/// Non-loopback IPv4 addresses of interfaces that are up, private ranges
/// first, for building the pairing URL. Empty when none or on error.
pub fn lan_addresses() -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs writes a linked list it allocates into `ifap`; we
    // free it below with freeifaddrs and never use it afterwards.
    if unsafe { libc::getifaddrs(&mut ifap) } != 0 {
        return out;
    }
    let mut cur = ifap;
    while !cur.is_null() {
        // SAFETY: `cur` is a non-null node of the list getifaddrs returned,
        // valid until freeifaddrs.
        let ifa = unsafe { &*cur };
        let up = ifa.ifa_flags & libc::IFF_UP as libc::c_uint != 0;
        let loopback = ifa.ifa_flags & libc::IFF_LOOPBACK as libc::c_uint != 0;
        if up && !loopback && !ifa.ifa_addr.is_null() {
            // SAFETY: ifa_addr is non-null and points to a sockaddr whose
            // family tells us its real type.
            let family = unsafe { (*ifa.ifa_addr).sa_family };
            if libc::c_int::from(family) == libc::AF_INET {
                // SAFETY: AF_INET addresses are sockaddr_in.
                let sin = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in) };
                let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                if !ip.is_loopback() && !ip.is_unspecified() && !out.contains(&ip) {
                    out.push(ip);
                }
            }
        }
        cur = ifa.ifa_next;
    }
    // SAFETY: `ifap` came from a successful getifaddrs and is freed once.
    unsafe { libc::freeifaddrs(ifap) };
    // Stable sort: private/link-local (reachable from a phone) first.
    out.sort_by_key(|ip| !allowed_v4(*ip));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> bool {
        is_allowed_peer(s.parse().unwrap())
    }

    #[test]
    fn allows_lan_and_loopback() {
        for a in [
            "127.0.0.1",
            "127.8.9.10",
            "10.0.0.5",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.20",
            "169.254.3.4",
            "::1",
            "fe80::1",
            "fd12:3456::1",
            "fc00::1",
            "::ffff:192.168.1.2",
            "::ffff:127.0.0.1",
        ] {
            assert!(ok(a), "{a} should be allowed");
        }
    }

    #[test]
    fn rejects_public_and_odd() {
        for a in [
            "8.8.8.8",
            "172.32.0.1",
            "172.15.255.255",
            "192.169.0.1",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "2001:4860::8888",
            "::",
            "::ffff:8.8.8.8",
            "fec0::1",
        ] {
            assert!(!ok(a), "{a} should be rejected");
        }
    }

    #[test]
    fn lan_addresses_has_no_loopback() {
        for ip in lan_addresses() {
            assert!(!ip.is_loopback());
        }
    }
}
