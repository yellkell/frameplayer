//! Minimal URL handling: just enough parsing to enforce the scheme, host
//! and credential rules, without pulling in a full URL library.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::error::{Error, Result};

/// The pieces of an absolute `scheme://authority/path?query#fragment` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlParts<'a> {
    /// Scheme, lower-cased by the caller's comparison (kept as written).
    pub scheme: &'a str,
    /// `user[:password]` before `@`, if present.
    pub userinfo: Option<&'a str>,
    /// Host name, IPv4 literal, or IPv6 literal without brackets.
    pub host: &'a str,
    /// Explicit port, if any.
    pub port: Option<u16>,
    /// Path including the leading `/` (may be empty).
    pub path: &'a str,
}

/// Splits an absolute URL. Rejects whitespace, control characters and
/// empty hosts.
pub fn parse_url(url: &str) -> std::result::Result<UrlParts<'_>, String> {
    if url.bytes().any(|b| b.is_ascii_control() || b == b' ') {
        return Err("contains spaces or control characters".into());
    }
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| "not an absolute URL".to_string())?;
    if scheme.is_empty()
        || !scheme
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+-.".contains(&b))
    {
        return Err("invalid scheme".into());
    }
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(auth_end);
    let path_end = tail.find(['?', '#']).unwrap_or(tail.len());
    let path = &tail[..path_end];
    let (userinfo, hostport) = match authority.rsplit_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, authority),
    };
    let (host, port) = if let Some(after) = hostport.strip_prefix('[') {
        let (h, p) = after
            .split_once(']')
            .ok_or_else(|| "unterminated IPv6 literal".to_string())?;
        let port = match p {
            "" => None,
            p => Some(parse_port(
                p.strip_prefix(':').ok_or("junk after IPv6 literal")?,
            )?),
        };
        (h, port)
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) => (h, Some(parse_port(p)?)),
            None => (hostport, None),
        }
    };
    if host.is_empty() {
        return Err("no host".into());
    }
    Ok(UrlParts {
        scheme,
        userinfo,
        host,
        port,
        path,
    })
}

fn parse_port(p: &str) -> std::result::Result<u16, String> {
    p.parse::<u16>().map_err(|_| format!("invalid port {p:?}"))
}

fn host_ip(host: &str) -> Option<IpAddr> {
    if let Ok(v6) = host.parse::<Ipv6Addr>() {
        return Some(IpAddr::V6(v6));
    }
    host.parse::<Ipv4Addr>().ok().map(IpAddr::V4)
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost") || host_ip(host).is_some_and(|ip| ip.is_loopback())
}

/// Rules for URLs the updater fetches (manifest, signature, payload):
/// `https`, no embedded credentials. Plain `http` is allowed only to a
/// loopback host (local testing); payload integrity never depends on TLS
/// because sizes and digests come from the signed manifest.
pub fn check_download_url(url: &str) -> Result<()> {
    let reject = |reason: &str| {
        Err(Error::RejectedUrl {
            url: url.to_string(),
            reason: reason.to_string(),
        })
    };
    let parts = match parse_url(url) {
        Ok(p) => p,
        Err(e) => return reject(&e),
    };
    if parts.userinfo.is_some() {
        return reject("URLs must not contain credentials");
    }
    if parts.scheme.eq_ignore_ascii_case("https") {
        return Ok(());
    }
    if parts.scheme.eq_ignore_ascii_case("http") && is_loopback_host(parts.host) {
        return Ok(());
    }
    reject("only https URLs are allowed")
}

/// True when `ip` is routable on the public internet (not loopback,
/// private, link-local, CGNAT, documentation, multicast or reserved).
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_v4(v4);
            }
            let s = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // unique local fc00::/7
                || (s[0] & 0xffc0) == 0xfe80 // link local fe80::/10
                || (s[0] == 0x2001 && s[1] == 0x0db8) // documentation
                || (s[0] == 0x0064 && s[1] == 0xff9b)) // NAT64 well-known prefix
        }
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || o[0] == 0
        || (o[0] == 100 && (o[1] & 0xc0) == 64) // CGNAT 100.64.0.0/10
        || (o[0] == 192 && o[1] == 0 && o[2] == 0) // IETF protocol assignments
        || (o[0] == 198 && (o[1] & 0xfe) == 18) // benchmarking 198.18.0.0/15
        || o[0] >= 240) // reserved
}

/// Rules shared with the community installers' manifest checks: `https`, no
/// credentials, and a host that is reachable on the public internet
/// (public IP literal, or a dotted DNS name outside local-only suffixes).
pub fn check_public_https_url(url: &str) -> std::result::Result<UrlParts<'_>, String> {
    let parts = parse_url(url)?;
    if !parts.scheme.eq_ignore_ascii_case("https") {
        return Err("must use https".into());
    }
    if parts.userinfo.is_some() {
        return Err("must not contain credentials".into());
    }
    if let Some(ip) = host_ip(parts.host) {
        if !is_public_ip(ip) {
            return Err(format!("host {} is not a public address", parts.host));
        }
        return Ok(parts);
    }
    let host = parts.host.trim_end_matches('.').to_ascii_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 2 {
        return Err(format!("host {} is not a public DNS name", parts.host));
    }
    if labels.iter().any(|l| {
        l.is_empty()
            || l.len() > 63
            || l.starts_with('-')
            || l.ends_with('-')
            || !l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    }) {
        return Err(format!("host {} is not a valid DNS name", parts.host));
    }
    // A purely numeric last label means a disguised IP ("127.1", "10.0.0.010").
    if labels
        .last()
        .is_some_and(|l| l.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(format!("host {} looks like a non-canonical IP", parts.host));
    }
    const LOCAL_SUFFIXES: [&str; 8] = [
        "localhost",
        "local",
        "internal",
        "lan",
        "home",
        "corp",
        "intranet",
        "home.arpa",
    ];
    if LOCAL_SUFFIXES
        .iter()
        .any(|s| host == *s || host.ends_with(&format!(".{s}")))
    {
        return Err(format!("host {} is a local-network name", parts.host));
    }
    Ok(parts)
}

/// Percent-encodes everything except RFC 3986 unreserved characters, for
/// use as a query parameter value.
pub fn percent_encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The last path segment of `url`, if it is a plain file name (ASCII
/// letters, digits, `._-+`, not starting with a dot). Used to name
/// downloads.
pub fn file_name_from_url(url: &str) -> Option<String> {
    let parts = parse_url(url).ok()?;
    let name = parts.path.rsplit('/').next()?;
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && name.len() <= 200
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-+".contains(&b));
    ok.then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_parts() {
        let p = parse_url("https://u:p@host.example:8443/a/b.zip?x=1#f").unwrap();
        assert_eq!(p.scheme, "https");
        assert_eq!(p.userinfo, Some("u:p"));
        assert_eq!(p.host, "host.example");
        assert_eq!(p.port, Some(8443));
        assert_eq!(p.path, "/a/b.zip");
        let p = parse_url("https://[2001:4860::1]:443/x").unwrap();
        assert_eq!(p.host, "2001:4860::1");
        assert_eq!(p.port, Some(443));
        assert!(parse_url("https:///x").is_err());
        assert!(parse_url("host/x").is_err());
        assert!(parse_url("https://a b/x").is_err());
    }

    #[test]
    fn download_url_rules() {
        assert!(check_download_url("https://github.com/a.zip").is_ok());
        assert!(check_download_url("http://127.0.0.1:9000/a.zip").is_ok());
        assert!(check_download_url("http://localhost/a.zip").is_ok());
        assert!(check_download_url("http://[::1]:80/a.zip").is_ok());
        assert!(check_download_url("http://example.com/a.zip").is_err());
        assert!(check_download_url("https://u:p@example.com/a.zip").is_err());
        assert!(check_download_url("ftp://example.com/a.zip").is_err());
    }

    #[test]
    fn public_host_rules() {
        for ok in [
            "https://github.com/x.zip",
            "https://8.8.8.8/x.zip",
            "https://[2606:4700::1111]/x.zip",
            "https://cdn.example.org./x.zip",
        ] {
            assert!(check_public_https_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://github.com/x.zip",
            "https://user@github.com/x.zip",
            "https://localhost/x.zip",
            "https://nas.local/x.zip",
            "https://router.lan/x.zip",
            "https://intranet/x.zip",
            "https://10.0.0.5/x.zip",
            "https://192.168.1.2/x.zip",
            "https://172.20.0.1/x.zip",
            "https://127.0.0.1/x.zip",
            "https://169.254.1.1/x.zip",
            "https://100.100.1.1/x.zip",
            "https://0.0.0.0/x.zip",
            "https://127.1/x.zip",
            "https://[::1]/x.zip",
            "https://[fd00::1]/x.zip",
            "https://[fe80::1]/x.zip",
            "https://[::ffff:192.168.0.1]/x.zip",
            "https://a.localhost/x.zip",
            "https://bad_host.com/x.zip",
        ] {
            assert!(check_public_https_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn percent_encoding() {
        assert_eq!(
            percent_encode_component("https://h.example/a b.json?x=1&y"),
            "https%3A%2F%2Fh.example%2Fa%20b.json%3Fx%3D1%26y"
        );
        assert_eq!(percent_encode_component("aZ09-._~"), "aZ09-._~");
    }

    #[test]
    fn file_names() {
        assert_eq!(
            file_name_from_url("https://h/x/frameplayer-1.0.0.zip?dl=1").as_deref(),
            Some("frameplayer-1.0.0.zip")
        );
        assert_eq!(file_name_from_url("https://h/x/"), None);
        assert_eq!(file_name_from_url("https://h/x/..zip"), None);
        assert_eq!(file_name_from_url("https://h/x/a%2F..%2Fb.zip"), None);
    }
}
