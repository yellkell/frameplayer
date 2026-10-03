//! Network: interface kinds (no addresses), whether an SSDP M-SEARCH gets
//! any reply (multicast out + unicast replies in through the firewall),
//! and whether the remote-control ports can be bound. Answers P16.

use crate::parse;
use crate::report::{CheckOutput, Status};
use crate::runner::CheckContext;
use crate::util::Out;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::time::{Duration, Instant};

/// DeoVR remote API port and FramePlayer's web remote port.
pub const PORTS: &[(u16, &str)] = &[(23554, "DeoVR remote API"), (8642, "web remote")];

pub fn run(_ctx: &CheckContext) -> CheckOutput {
    let mut o = Out::new();
    let ifs = interfaces();
    let has_lan_v4 = ifs.iter().any(|i| i.v4_classes.contains("lan"));
    let has_v6 = ifs.iter().any(|i| i.has_global_v6);
    let mut kinds: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for i in &ifs {
        if i.up {
            kinds.entry(i.kind).or_default().push(i.name.clone());
        }
    }
    o.set(
        "interfaces",
        json!({
            "up_by_kind": kinds,
            "has_lan_ipv4": has_lan_v4,
            "ipv4_classes": ifs.iter().flat_map(|i| i.v4_classes.iter().cloned()).collect::<BTreeSet<_>>(),
            "has_global_ipv6": has_v6,
        }),
    );

    let ssdp = ssdp_probe(Duration::from_secs(3));
    match &ssdp {
        Ok((replies, responders)) => {
            o.set(
                "ssdp",
                json!({ "replies": replies, "distinct_responders": responders }),
            );
            if *replies > 0 {
                o.finding(
                    "ssdp",
                    Status::Pass,
                    &["P16"],
                    format!("SSDP M-SEARCH got {replies} repl(ies) from {responders} device(s) within 3 s: multicast discovery works"),
                );
            } else if has_lan_v4 {
                o.finding(
                    "ssdp",
                    Status::Unknown,
                    &["P16"],
                    "no SSDP replies within 3 s (either no UPnP devices on this LAN or the firewall drops replies)",
                );
            } else {
                o.finding(
                    "ssdp",
                    Status::Unknown,
                    &["P16"],
                    "no LAN IPv4 address; SSDP not testable",
                );
            }
        }
        Err(e) => {
            o.set("ssdp", json!({ "error": e }));
            o.finding(
                "ssdp",
                Status::Fail,
                &["P16"],
                format!("SSDP socket error: {e}"),
            );
        }
    }

    let mut ports = Vec::new();
    for &(port, what) in PORTS {
        let r = TcpListener::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)));
        let (ok, err) = match &r {
            Ok(_) => (true, None),
            Err(e) => (false, Some(e.to_string())),
        };
        drop(r);
        ports.push(json!({ "port": port, "use": what, "bindable": ok, "error": err }));
        o.finding(
            &format!("tcp_{port}"),
            if ok { Status::Pass } else { Status::Fail },
            &["P16"],
            match err {
                None => format!("TCP {port} ({what}) can be bound on all interfaces"),
                Some(e) => format!("TCP {port} ({what}) cannot be bound: {e}"),
            },
        );
    }
    o.set("tcp_ports", ports);
    o.set(
        "firewall_hints",
        json!({
            "nftables_conf": std::path::Path::new("/etc/nftables.conf").exists(),
            "firewalld": std::path::Path::new("/etc/firewalld").exists(),
            "ufw": std::path::Path::new("/etc/ufw").exists(),
        }),
    );
    let status = crate::checks::combine(&o);
    let summary = format!(
        "LAN IPv4: {}; SSDP replies: {}; ports {}",
        if has_lan_v4 { "yes" } else { "no" },
        ssdp.as_ref().map_or("error".into(), |r| r.0.to_string()),
        PORTS
            .iter()
            .map(|(p, _)| format!(
                "{p}={}",
                if o.status_of(&format!("tcp_{p}")) == Some(Status::Pass) {
                    "ok"
                } else {
                    "busy"
                }
            ))
            .collect::<Vec<_>>()
            .join(" ")
    );
    o.finish(status, summary)
}

struct Iface {
    name: String,
    kind: &'static str,
    up: bool,
    v4_classes: BTreeSet<String>,
    has_global_v6: bool,
}

fn interfaces() -> Vec<Iface> {
    let mut out: Vec<Iface> = Vec::new();
    // SAFETY: getifaddrs/freeifaddrs pair; we only read the list.
    unsafe {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut head) != 0 {
            return out;
        }
        let mut p = head;
        while !p.is_null() {
            let ifa = &*p;
            p = ifa.ifa_next;
            let name = std::ffi::CStr::from_ptr(ifa.ifa_name)
                .to_string_lossy()
                .into_owned();
            let idx = match out.iter().position(|i| i.name == name) {
                Some(i) => i,
                None => {
                    out.push(Iface {
                        kind: parse::interface_kind(&name),
                        name,
                        up: false,
                        v4_classes: BTreeSet::new(),
                        has_global_v6: false,
                    });
                    out.len() - 1
                }
            };
            let e = &mut out[idx];
            e.up |= ifa.ifa_flags & libc::IFF_UP as u32 != 0;
            if ifa.ifa_addr.is_null() {
                continue;
            }
            match (*ifa.ifa_addr).sa_family as i32 {
                libc::AF_INET => {
                    let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                    let o = u32::from_be(sin.sin_addr.s_addr).to_be_bytes();
                    e.v4_classes.insert(parse::ipv4_class(o).to_string());
                }
                libc::AF_INET6 => {
                    let sin6 = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                    let a = std::net::Ipv6Addr::from(sin6.sin6_addr.s6_addr);
                    let seg0 = a.segments()[0];
                    if !a.is_loopback() && (seg0 & 0xe000) == 0x2000 {
                        e.has_global_v6 = true;
                    }
                }
                _ => {}
            }
        }
        libc::freeifaddrs(head);
    }
    out
}

/// Send `M-SEARCH ssdp:all` (via fp-sources' request builder) and count
/// replies parsed by fp-sources' SSDP parser.
fn ssdp_probe(wait: Duration) -> Result<(usize, usize), String> {
    use fp_sources::dlna::ssdp;
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map_err(|e| e.to_string())?;
    sock.set_multicast_ttl_v4(2).map_err(|e| e.to_string())?;
    let req = ssdp::build_msearch("ssdp:all", 2);
    for _ in 0..2 {
        sock.send_to(req.as_bytes(), ssdp::SSDP_ADDR)
            .map_err(|e| e.to_string())?;
    }
    let deadline = Instant::now() + wait;
    let mut buf = [0u8; 4096];
    let mut replies = 0;
    let mut responders = BTreeSet::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if left.is_zero() {
            break;
        }
        sock.set_read_timeout(Some(left))
            .map_err(|e| e.to_string())?;
        match sock.recv_from(&mut buf) {
            Ok((n, from)) => {
                if ssdp::parse_response(&String::from_utf8_lossy(&buf[..n])).is_some() {
                    replies += 1;
                    responders.insert(from.ip());
                }
            }
            Err(_) => break,
        }
    }
    Ok((replies, responders.len()))
}
