//! URL helpers: percent-encoding, joining, credential stripping and
//! redaction, HTML entity decoding and natural sorting of names.

use crate::config::Credentials;
use crate::error::{Error, Result};
use base64::Engine;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use std::cmp::Ordering;

/// Characters escaped in one path segment: everything except the RFC 3986
/// unreserved set. Over-escaping is always safe; under-escaping (`#`, `?`,
/// `%`, spaces, unicode) breaks requests.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Percent-encodes one path segment (a file or folder name).
///
/// `"a b#1.mp4"` becomes `"a%20b%231.mp4"`; non-ASCII is UTF-8 encoded.
pub fn encode_segment(name: &str) -> String {
    utf8_percent_encode(name, SEGMENT).to_string()
}

/// Encodes a value for an `application/x-www-form-urlencoded` body.
pub fn encode_form_value(value: &str) -> String {
    utf8_percent_encode(value, SEGMENT).to_string()
}

/// Decodes `%XX` escapes. Invalid UTF-8 is replaced, `+` is left alone (it
/// only means space in query strings).
pub fn percent_decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .into_owned()
}

/// Appends a child name to a directory URL, encoding it. Directories get a
/// trailing slash.
pub fn join_child(dir_url: &str, name: &str, is_dir: bool) -> String {
    let mut out = String::with_capacity(dir_url.len() + name.len() + 8);
    out.push_str(dir_url);
    if !out.ends_with('/') {
        out.push('/');
    }
    out.push_str(&encode_segment(name));
    if is_dir {
        out.push('/');
    }
    out
}

/// Ensures a directory URL ends with `/` (before any query string).
pub fn with_trailing_slash(url: &str) -> String {
    let (path, query) = match url.find('?') {
        Some(i) => (&url[..i], &url[i..]),
        None => (url, ""),
    };
    if path.ends_with('/') {
        url.to_string()
    } else {
        format!("{path}/{query}")
    }
}

/// Resolves `reference` (absolute, root-relative or relative) against
/// `base`. A literal `#` in the reference is escaped first: servers that
/// return unencoded names must not lose the rest of the name to a fragment.
pub fn resolve(base: &str, reference: &str) -> Result<String> {
    let base_url =
        url::Url::parse(base).map_err(|e| Error::invalid(base, format!("bad base URL: {e}")))?;
    let reference = reference.trim().replace('#', "%23");
    base_url
        .join(&reference)
        .map(|u| u.to_string())
        .map_err(|e| Error::invalid(&reference, format!("cannot resolve against base: {e}")))
}

/// Splits a URL (or plain path) into scheme + authority, path, and query +
/// fragment.
fn split_url(location: &str) -> (&str, &str, &str) {
    let path_start = match location.find("://") {
        Some(i) => {
            let after = i + 3;
            location[after..]
                .find(['/', '?', '#'])
                .map(|j| after + j)
                .unwrap_or(location.len())
        }
        None => 0,
    };
    let path_end = location[path_start..]
        .find(['?', '#'])
        .map(|j| path_start + j)
        .unwrap_or(location.len());
    (
        &location[..path_start],
        &location[path_start..path_end],
        &location[path_end..],
    )
}

/// Decoded last non-empty path segment of a URL or path: the file or folder
/// name. Empty for a bare host.
pub fn last_segment(location: &str) -> String {
    let (_, path, _) = split_url(location);
    let seg = path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default();
    percent_decode(seg)
}

/// URL of the folder containing `url`, with a trailing slash. `None` at the
/// root of the host.
pub fn parent_url(url: &str) -> Option<String> {
    let (prefix, path, _) = split_url(url);
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    let cut = trimmed.rfind('/')?;
    Some(format!("{prefix}{}/", &trimmed[..cut]))
}

/// Removes `user:password@` from a URL, returning the clean URL and the
/// decoded credentials.
pub fn split_credentials(url: &str) -> Result<(String, Option<Credentials>)> {
    let mut parsed = url::Url::parse(url).map_err(|e| Error::invalid(url, e.to_string()))?;
    if parsed.username().is_empty() && parsed.password().is_none() {
        return Ok((url.to_string(), None));
    }
    let creds = Credentials {
        username: percent_decode(parsed.username()),
        password: parsed.password().map(percent_decode).unwrap_or_default(),
    };
    // Both setters only fail for URLs that cannot have credentials, which
    // we just read credentials from.
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    Ok((parsed.to_string(), Some(creds)))
}

/// Query parameters whose values are secrets (Stash and others put API keys
/// in feed URLs).
const SECRET_PARAMS: &[&str] = &[
    "apikey",
    "api_key",
    "password",
    "passwd",
    "pwd",
    "pass",
    "token",
    "access_token",
    "auth",
    "key",
];

/// Hides passwords and API keys in a URL so it can be logged or shown:
/// `http://bob:pw@nas/x?apikey=1` becomes `http://bob:***@nas/x?apikey=***`.
pub fn redact(location: &str) -> String {
    let mut out = location.to_string();
    if let Some(scheme_end) = out.find("://") {
        let auth_start = scheme_end + 3;
        let auth_end = out[auth_start..]
            .find(['/', '?', '#'])
            .map(|i| auth_start + i)
            .unwrap_or(out.len());
        if let Some(at) = out[auth_start..auth_end].rfind('@') {
            let userinfo = &out[auth_start..auth_start + at];
            if let Some(colon) = userinfo.find(':') {
                let user = userinfo[..colon].to_string();
                out.replace_range(auth_start..auth_start + at, &format!("{user}:***"));
            }
        }
    }
    if let Some(q) = out.find('?') {
        let (head, query) = out.split_at(q + 1);
        let (query, frag) = match query.find('#') {
            Some(i) => (&query[..i], &query[i..]),
            None => (query, ""),
        };
        let redacted: Vec<String> = query
            .split('&')
            .map(|pair| match pair.split_once('=') {
                Some((k, _)) if SECRET_PARAMS.iter().any(|s| s.eq_ignore_ascii_case(k)) => {
                    format!("{k}=***")
                }
                _ => pair.to_string(),
            })
            .collect();
        out = format!("{head}{}{frag}", redacted.join("&"));
    }
    out
}

/// `Authorization` header value for HTTP Basic auth.
pub fn basic_auth_value(creds: &Credentials) -> String {
    let raw = format!("{}:{}", creds.username, creds.password);
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(raw)
    )
}

/// Rewrites `webdav://`, `dav://` to `http://` and `webdavs://`, `davs://`
/// to `https://`. Other URLs are returned unchanged.
pub fn webdav_to_http(url: &str) -> String {
    for (from, to) in [
        ("webdavs://", "https://"),
        ("davs://", "https://"),
        ("webdav://", "http://"),
        ("dav://", "http://"),
    ] {
        if url.len() >= from.len() && url[..from.len()].eq_ignore_ascii_case(from) {
            return format!("{to}{}", &url[from.len()..]);
        }
    }
    url.to_string()
}

/// True for `http://` and `https://` URLs.
pub fn is_http_url(s: &str) -> bool {
    let lower = s.get(..8).unwrap_or(s).to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// Decodes the HTML entities found in directory listings: the five XML
/// ones, `&nbsp;` and numeric references.
pub fn html_unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest[..rest.len().min(12)].find(';') else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Case-insensitive comparison that orders embedded numbers by value
/// (`ep2` before `ep10`).
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let mut na = String::new();
                while let Some(c) = ai.peek().copied().filter(char::is_ascii_digit) {
                    na.push(c);
                    ai.next();
                }
                let mut nb = String::new();
                while let Some(c) = bi.peek().copied().filter(char::is_ascii_digit) {
                    nb.push(c);
                    bi.next();
                }
                let ta = na.trim_start_matches('0');
                let tb = nb.trim_start_matches('0');
                let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(x), Some(y)) => {
                let ord = x.to_lowercase().cmp(y.to_lowercase());
                if ord != Ordering::Equal {
                    return ord;
                }
                ai.next();
                bi.next();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_awkward_names() {
        assert_eq!(encode_segment("a b#1?.mp4"), "a%20b%231%3F.mp4");
        assert_eq!(encode_segment("100%.mkv"), "100%25.mkv");
        assert_eq!(
            encode_segment("Ünïcødé 日本.mp4"),
            "%C3%9Cn%C3%AFc%C3%B8d%C3%A9%20%E6%97%A5%E6%9C%AC.mp4"
        );
        assert_eq!(encode_segment("a/b"), "a%2Fb");
        assert_eq!(encode_segment("x-y_z.~"), "x-y_z.~");
    }

    #[test]
    fn decode_roundtrip() {
        for name in ["a b#1?.mp4", "100%.mkv", "Ünïcødé 日本.mp4", "a+b.mp4"] {
            assert_eq!(percent_decode(&encode_segment(name)), name);
        }
        assert_eq!(percent_decode("a+b%20c"), "a+b c");
        assert_eq!(percent_decode("bad%zz"), "bad%zz");
    }

    #[test]
    fn joins_children() {
        assert_eq!(
            join_child("http://h/v", "a b.mp4", false),
            "http://h/v/a%20b.mp4"
        );
        assert_eq!(
            join_child("http://h/v/", "Dir #2", true),
            "http://h/v/Dir%20%232/"
        );
        assert_eq!(with_trailing_slash("http://h/v?x=1"), "http://h/v/?x=1");
        assert_eq!(with_trailing_slash("http://h/v/"), "http://h/v/");
    }

    #[test]
    fn resolves_references() {
        assert_eq!(
            resolve("http://h:8/a/b/", "c%20d.mp4").unwrap(),
            "http://h:8/a/b/c%20d.mp4"
        );
        assert_eq!(resolve("http://h/a/b/", "/x/y").unwrap(), "http://h/x/y");
        assert_eq!(resolve("http://h/a/", "../z/").unwrap(), "http://h/z/");
        assert_eq!(resolve("http://h/a/", "http://o/q").unwrap(), "http://o/q");
        // Raw spaces and '#' from sloppy servers.
        assert_eq!(
            resolve("http://h/a/", "my file #3.mp4").unwrap(),
            "http://h/a/my%20file%20%233.mp4"
        );
        assert!(resolve("not a url", "x").is_err());
    }

    #[test]
    fn segments_and_parents() {
        assert_eq!(last_segment("http://h/a/b%20c.mp4?x=1"), "b c.mp4");
        assert_eq!(last_segment("http://h/a/dir/"), "dir");
        assert_eq!(last_segment("http://h"), "");
        assert_eq!(last_segment("/local/path/file.mkv"), "file.mkv");
        assert_eq!(
            parent_url("http://h:80/a/b/c.mp4").as_deref(),
            Some("http://h:80/a/b/")
        );
        assert_eq!(parent_url("http://h/a/b/").as_deref(), Some("http://h/a/"));
        assert_eq!(parent_url("http://h/a").as_deref(), Some("http://h/"));
        assert_eq!(parent_url("http://h/"), None);
        assert_eq!(parent_url("http://h"), None);
    }

    #[test]
    fn strips_credentials() {
        let (u, c) = split_credentials("http://bob:p%40ss@nas:5005/dav/").unwrap();
        assert_eq!(u, "http://nas:5005/dav/");
        let c = c.unwrap();
        assert_eq!((c.username.as_str(), c.password.as_str()), ("bob", "p@ss"));
        let (u, c) = split_credentials("https://nas/x").unwrap();
        assert_eq!(u, "https://nas/x");
        assert!(c.is_none());
    }

    #[test]
    fn redacts_secrets() {
        assert_eq!(
            redact("http://bob:hunter2@nas/a?apikey=xyz&x=1#f"),
            "http://bob:***@nas/a?apikey=***&x=1#f"
        );
        assert_eq!(redact("http://bob@nas/a"), "http://bob@nas/a");
        assert_eq!(redact("smb://u:pw@host/share"), "smb://u:***@host/share");
        assert_eq!(redact("/local/a?b"), "/local/a?b");
    }

    #[test]
    fn basic_auth() {
        let c = Credentials {
            username: "Aladdin".into(),
            password: "open sesame".into(),
        };
        assert_eq!(basic_auth_value(&c), "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==");
    }

    #[test]
    fn webdav_schemes() {
        assert_eq!(webdav_to_http("webdavs://h/x"), "https://h/x");
        assert_eq!(webdav_to_http("DAV://h/x"), "http://h/x");
        assert_eq!(webdav_to_http("http://h/x"), "http://h/x");
        assert!(is_http_url("HTTPS://x"));
        assert!(!is_http_url("smb://x"));
    }

    #[test]
    fn unescapes_html() {
        assert_eq!(
            html_unescape("a &amp; b &lt;c&gt; &#39;d&#x27; &quot;"),
            "a & b <c> 'd' \""
        );
        assert_eq!(html_unescape("AT&T &bogus; &"), "AT&T &bogus; &");
    }

    #[test]
    fn natural_order() {
        let mut v = vec!["ep10.mp4", "Ep2.mp4", "ep1.mp4", "a.mp4", "ep02b.mp4"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, ["a.mp4", "ep1.mp4", "Ep2.mp4", "ep02b.mp4", "ep10.mp4"]);
    }
}
