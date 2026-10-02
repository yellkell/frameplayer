//! HTTP authentication challenges: Basic and Digest (RFC 7616 / RFC 2617).

use md5::{Digest as _, Md5};
use sha2::Sha256;

/// A parsed `WWW-Authenticate` challenge.
#[derive(Debug, Clone, PartialEq)]
pub enum Challenge {
    Basic,
    Digest(DigestChallenge),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct DigestChallenge {
    pub realm: String,
    pub nonce: String,
    pub opaque: Option<String>,
    /// "MD5", "MD5-sess", "SHA-256", "SHA-256-sess".
    pub algorithm: String,
    /// Whether the server offered qop=auth.
    pub qop_auth: bool,
}

/// Split `k=v, k="v, with comma"` parameter lists.
fn params(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = s.trim();
    while !rest.is_empty() {
        let Some(eq) = rest.find('=') else { break };
        let key = rest[..eq]
            .trim()
            .trim_start_matches(',')
            .trim()
            .to_ascii_lowercase();
        rest = rest[eq + 1..].trim_start();
        let value;
        if let Some(r) = rest.strip_prefix('"') {
            let mut v = String::new();
            let mut chars = r.char_indices();
            let mut end = r.len();
            while let Some((i, c)) = chars.next() {
                match c {
                    '\\' => {
                        if let Some((_, n)) = chars.next() {
                            v.push(n);
                        }
                    }
                    '"' => {
                        end = i + 1;
                        break;
                    }
                    c => v.push(c),
                }
            }
            value = v;
            rest = &r[end.min(r.len())..];
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            value = rest[..end].trim().to_string();
            rest = &rest[end..];
        }
        rest = rest.trim_start().trim_start_matches(',').trim_start();
        out.push((key, value));
    }
    out
}

/// Parse one `WWW-Authenticate` header value. Digest is preferred when a
/// server sends several challenges, so callers should try all headers.
pub fn parse_challenge(header: &str) -> Option<Challenge> {
    let h = header.trim();
    let (scheme, rest) = h.split_once(char::is_whitespace).unwrap_or((h, ""));
    match scheme.to_ascii_lowercase().as_str() {
        "basic" => Some(Challenge::Basic),
        "digest" => {
            let mut c = DigestChallenge {
                algorithm: "MD5".into(),
                ..Default::default()
            };
            for (k, v) in params(rest) {
                match k.as_str() {
                    "realm" => c.realm = v,
                    "nonce" => c.nonce = v,
                    "opaque" => c.opaque = Some(v),
                    "algorithm" => c.algorithm = v,
                    "qop" => {
                        c.qop_auth = v.split(',').any(|q| q.trim().eq_ignore_ascii_case("auth"))
                    }
                    _ => {}
                }
            }
            (!c.nonce.is_empty()).then_some(Challenge::Digest(c))
        }
        _ => None,
    }
}

fn hash(algorithm: &str, data: &str) -> String {
    if algorithm.to_ascii_uppercase().starts_with("SHA-256") {
        hex::encode(Sha256::digest(data.as_bytes()))
    } else {
        hex::encode(Md5::digest(data.as_bytes()))
    }
}

/// Compute the `Authorization: Digest ...` header value.
pub fn digest_authorization(
    c: &DigestChallenge,
    user: &str,
    pass: &str,
    method: &str,
    uri: &str,
    nc: u32,
    cnonce: &str,
) -> String {
    let alg = c.algorithm.as_str();
    let mut ha1 = hash(alg, &format!("{user}:{}:{pass}", c.realm));
    if alg.to_ascii_lowercase().ends_with("-sess") {
        ha1 = hash(alg, &format!("{ha1}:{}:{cnonce}", c.nonce));
    }
    let ha2 = hash(alg, &format!("{method}:{uri}"));
    let nc_s = format!("{nc:08x}");
    let response = if c.qop_auth {
        hash(
            alg,
            &format!("{ha1}:{}:{nc_s}:{cnonce}:auth:{ha2}", c.nonce),
        )
    } else {
        hash(alg, &format!("{ha1}:{}:{ha2}", c.nonce))
    };
    let q = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let mut h = format!(
        "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", algorithm={}, response=\"{}\"",
        q(user),
        q(&c.realm),
        q(&c.nonce),
        q(uri),
        alg,
        response
    );
    if c.qop_auth {
        h.push_str(&format!(", qop=auth, nc={nc_s}, cnonce=\"{cnonce}\""));
    }
    if let Some(o) = &c.opaque {
        h.push_str(&format!(", opaque=\"{}\"", q(o)));
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc2617_vector() {
        let hdr = r#"Digest realm="testrealm@host.com", qop="auth,auth-int", nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093", opaque="5ccc069c403ebaf9f0171e9517f40e41""#;
        let Some(Challenge::Digest(c)) = parse_challenge(hdr) else {
            panic!("not digest")
        };
        assert!(c.qop_auth);
        assert_eq!(c.realm, "testrealm@host.com");
        let h = digest_authorization(
            &c,
            "Mufasa",
            "Circle Of Life",
            "GET",
            "/dir/index.html",
            1,
            "0a4f113b",
        );
        assert!(
            h.contains("response=\"6629fae49393a05397450978507c4ef1\""),
            "{h}"
        );
        assert!(h.contains("nc=00000001"));
        assert!(h.contains("opaque=\"5ccc069c403ebaf9f0171e9517f40e41\""));
    }

    #[test]
    fn rfc7616_sha256_vector() {
        let hdr = r#"Digest realm="http-auth@example.org", qop="auth, auth-int", algorithm=SHA-256, nonce="7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v", opaque="FQhe/qaU925kfnzjCev0ciny7QMkPqMAFRtzCUYo5tdS""#;
        let Some(Challenge::Digest(c)) = parse_challenge(hdr) else {
            panic!("not digest")
        };
        assert_eq!(c.algorithm, "SHA-256");
        let h = digest_authorization(
            &c,
            "Mufasa",
            "Circle of Life",
            "GET",
            "/dir/index.html",
            1,
            "f2/wE4q74E6zIJEtWaHKaf5wv/H5QzzpXusqGemxURZJ",
        );
        assert!(
            h.contains(
                "response=\"753927fa0e85d155564e2e272a28d1802ca10daf4496794697cf8db5856cb6c1\""
            ),
            "{h}"
        );
    }

    #[test]
    fn basic_and_garbage() {
        assert_eq!(parse_challenge("Basic realm=\"x\""), Some(Challenge::Basic));
        assert_eq!(parse_challenge("Bearer"), None);
        assert_eq!(parse_challenge("Digest realm=\"x\""), None);
    }
}
