//! SSDP (UPnP discovery): M-SEARCH over UDP multicast and response parsing.

use crate::error::Result;
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;
use tokio::net::UdpSocket;

pub const SSDP_ADDR: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(239, 255, 255, 250), 1900);
pub const MEDIA_SERVER_ST: &str = "urn:schemas-upnp-org:device:MediaServer:1";

/// One unicast reply to an M-SEARCH (or a NOTIFY alive).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SsdpResponse {
    /// Device description URL.
    pub location: String,
    pub usn: String,
    pub st: String,
    pub server: Option<String>,
}

/// Build an M-SEARCH request.
pub fn build_msearch(st: &str, mx: u8) -> String {
    format!("M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: {mx}\r\nST: {st}\r\nUSER-AGENT: Linux/1.0 UPnP/1.1 {}\r\n\r\n", crate::http::USER_AGENT)
}

/// Parse an SSDP datagram (`HTTP/1.1 200 OK` search reply or `NOTIFY`).
pub fn parse_response(datagram: &str) -> Option<SsdpResponse> {
    let mut lines = datagram.split("\r\n").flat_map(|l| l.split('\n'));
    let first = lines.next()?.trim();
    let is_reply = first.starts_with("HTTP/1.1 200") || first.starts_with("HTTP/1.0 200");
    let is_notify = first.starts_with("NOTIFY");
    if !is_reply && !is_notify {
        return None;
    }
    let mut h: HashMap<String, String> = HashMap::new();
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            h.insert(k.trim().to_ascii_uppercase(), v.trim().to_string());
        }
    }
    if is_notify && h.get("NTS").map(String::as_str) != Some("ssdp:alive") {
        return None;
    }
    Some(SsdpResponse {
        location: h.get("LOCATION")?.clone(),
        usn: h.get("USN").cloned().unwrap_or_default(),
        st: h
            .get("ST")
            .or_else(|| h.get("NT"))
            .cloned()
            .unwrap_or_default(),
        server: h.get("SERVER").cloned(),
    })
}

/// Multicast an M-SEARCH for `st` and collect unique replies (by location)
/// for `timeout`. The request is sent twice since UDP may drop it.
// [verify] SteamOS's firewall (if enabled on the Frame) must allow inbound
// unicast UDP replies to the ephemeral port.
pub async fn search(st: &str, timeout: Duration) -> Result<Vec<SsdpResponse>> {
    let sock = UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))).await?;
    sock.set_multicast_ttl_v4(2)?;
    let mx = timeout.as_secs().clamp(1, 5) as u8;
    let req = build_msearch(st, mx);
    for _ in 0..2 {
        sock.send_to(req.as_bytes(), SSDP_ADDR).await?;
    }
    let mut out: Vec<SsdpResponse> = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    let mut buf = vec![0u8; 4096];
    loop {
        let recv = tokio::time::timeout_at(deadline, sock.recv_from(&mut buf)).await;
        let Ok(Ok((n, _from))) = recv else { break };
        if let Some(r) = parse_response(&String::from_utf8_lossy(&buf[..n])) {
            if !out.iter().any(|o| o.location == r.location) {
                out.push(r);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msearch_format() {
        let m = build_msearch(MEDIA_SERVER_ST, 2);
        assert!(m.starts_with("M-SEARCH * HTTP/1.1\r\n"));
        assert!(m.contains("MAN: \"ssdp:discover\"\r\n"));
        assert!(m.contains("ST: urn:schemas-upnp-org:device:MediaServer:1\r\n"));
        assert!(m.ends_with("\r\n\r\n"));
    }

    #[test]
    fn parse_replies() {
        let r = "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nEXT:\r\nLocation: http://192.168.1.10:8200/rootDesc.xml\r\nSERVER: Linux DLNADOC/1.50 UPnP/1.0 MiniDLNA/1.3.0\r\nST: urn:schemas-upnp-org:device:MediaServer:1\r\nUSN: uuid:4d696e69-444c-164e-9d41-001c42f1b5e6::urn:schemas-upnp-org:device:MediaServer:1\r\n\r\n";
        let p = parse_response(r).unwrap();
        assert_eq!(p.location, "http://192.168.1.10:8200/rootDesc.xml");
        assert!(p.usn.starts_with("uuid:4d696e69"));
        assert_eq!(
            p.server.as_deref(),
            Some("Linux DLNADOC/1.50 UPnP/1.0 MiniDLNA/1.3.0")
        );
        let notify = "NOTIFY * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nNT: upnp:rootdevice\r\nNTS: ssdp:byebye\r\nLOCATION: http://x/\r\n\r\n";
        assert!(parse_response(notify).is_none());
        assert!(parse_response("M-SEARCH * HTTP/1.1\r\n\r\n").is_none());
        assert!(parse_response("HTTP/1.1 200 OK\r\nST: x\r\n\r\n").is_none());
    }
}
