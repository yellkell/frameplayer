//! Plain HTTP(S) servers with automatic directory listings (Apache
//! `mod_autoindex`, nginx `autoindex`, lighttpd `mod_dirlisting`, Python's
//! `http.server`, and most NAS "web folders").
//!
//! The listing HTML is not a standard, so parsing is heuristic: every link
//! that resolves to a direct child of the listed folder is an entry (a
//! trailing `/` marks a folder), and the text following the link on the
//! same line or table row is searched for a date and a size.
//! Locations are absolute `http(s)://` URLs without credentials.

use crate::config::{Credentials, HttpConfig, SourceKind};
use crate::error::{Error, Result};
use crate::http::{HttpClient, HttpOptions};
use crate::timeutil::parse_listing_date;
use crate::urlutil::{
    html_unescape, is_http_url, parent_url, percent_decode, redact, resolve, split_credentials,
    with_trailing_slash,
};
use crate::{Source, dir_entry, file_entry, sort_entries};
use fp_core::source::{ByteSource, Entry, EntryKind};
use std::sync::Arc;

/// An HTTP(S) folder served with autoindex listings.
pub struct HttpDirSource {
    id: String,
    name: String,
    root: String,
    client: HttpClient,
}

impl std::fmt::Debug for HttpDirSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpDirSource")
            .field("id", &self.id)
            .field("root", &redact(&self.root))
            .field("client", &self.client)
            .finish()
    }
}

/// Splits credentials out of a configured URL. Credentials given
/// separately win over ones embedded in the URL.
pub(crate) fn clean_url_and_credentials(
    url: &str,
    explicit: Option<&Credentials>,
) -> Result<(String, Option<Credentials>)> {
    let (clean, embedded) = split_credentials(url)?;
    Ok((clean, explicit.cloned().or(embedded)))
}

impl HttpDirSource {
    /// Creates the source. Fails when the URL is not `http(s)://`.
    pub fn new(config: &HttpConfig) -> Result<HttpDirSource> {
        if !is_http_url(&config.url) {
            return Err(Error::invalid(
                &config.url,
                "expected an http:// or https:// URL",
            ));
        }
        let (url, creds) = clean_url_and_credentials(&config.url, config.credentials.as_ref())?;
        let opts = HttpOptions {
            insecure_tls: config.insecure_tls,
            ..HttpOptions::default()
        };
        Ok(HttpDirSource {
            id: config.id.clone(),
            name: config.name.clone(),
            root: with_trailing_slash(&url),
            client: HttpClient::new(opts, creds.as_ref()),
        })
    }

    /// URL listed by `list(None)`.
    pub fn root(&self) -> &str {
        &self.root
    }
}

impl Source for HttpDirSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> SourceKind {
        SourceKind::Http
    }

    fn describe(&self) -> String {
        format!("HTTP folder {}", redact(&self.root))
    }

    fn list(&self, location: Option<&str>) -> Result<Vec<Entry>> {
        let url = with_trailing_slash(location.unwrap_or(&self.root));
        let html = self.client.get_text(&url)?;
        let mut entries = parse_autoindex(&html, &url)?;
        sort_entries(&mut entries);
        Ok(entries)
    }

    fn open(&self, location: &str) -> Result<Arc<dyn ByteSource>> {
        Ok(Arc::new(self.client.open(location)?))
    }

    fn parent(&self, location: &str) -> Option<String> {
        parent_url(location)
    }
}

/// Finds `needle` (ASCII, lower case) in `hay` ignoring ASCII case.
fn find_ci(hay: &str, needle: &str, from: usize) -> Option<usize> {
    let h = hay.as_bytes();
    let n = needle.as_bytes();
    if from >= h.len() || n.is_empty() {
        return None;
    }
    h[from..]
        .windows(n.len())
        .position(|w| w.eq_ignore_ascii_case(n))
        .map(|i| from + i)
}

/// Value of attribute `name` inside an opening tag's text.
fn tag_attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(name).map(|i| from + i) {
        from = i + name.len();
        let before_ok = i == 0 || lower.as_bytes()[i - 1].is_ascii_whitespace();
        let rest = lower[from..].trim_start();
        if !before_ok || !rest.starts_with('=') {
            continue;
        }
        let eq = from + lower[from..].find('=')?;
        let value = tag[eq + 1..].trim_start();
        let v = match value.chars().next()? {
            q @ ('"' | '\'') => {
                let inner = &value[1..];
                &inner[..inner.find(q)?]
            }
            _ => {
                let end = value
                    .find(|c: char| c.is_ascii_whitespace() || c == '>')
                    .unwrap_or(value.len());
                &value[..end]
            }
        };
        return Some(html_unescape(v));
    }
    None
}

/// Removes tags from an HTML fragment and decodes entities.
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => {
                in_tag = true;
                out.push(' ');
            }
            '>' if in_tag => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    html_unescape(&out)
}

/// Parses a human size: `123456`, `1.2M`, `4.0K`, `3G`, `12 KiB`, `5MB`.
fn parse_size(tok: &str, next: Option<&str>) -> Option<u64> {
    let tok = tok.trim();
    let split = tok
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(tok.len());
    let (num, mut unit) = tok.split_at(split);
    if num.is_empty() {
        return None;
    }
    let value: f64 = num.parse().ok()?;
    if unit.is_empty() {
        if let Some(n) = next {
            let n = n.trim();
            if matches!(
                n.to_ascii_lowercase().as_str(),
                "b" | "bytes"
                    | "k"
                    | "kb"
                    | "kib"
                    | "m"
                    | "mb"
                    | "mib"
                    | "g"
                    | "gb"
                    | "gib"
                    | "t"
                    | "tb"
                    | "tib"
            ) {
                unit = n;
            }
        }
    }
    let mult: f64 = match unit.to_ascii_lowercase().as_str() {
        "" | "b" | "bytes" => {
            if num.contains('.') {
                return None;
            }
            1.0
        }
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" | "tib" => 1024.0_f64.powi(4),
        _ => return None,
    };
    let v = value * mult;
    (v.is_finite() && v >= 0.0).then_some(v.round() as u64)
}

/// Date and size found in the text after a link.
fn parse_details(text: &str) -> (Option<i64>, Option<u64>) {
    let toks: Vec<&str> = text.split_whitespace().collect();
    let mut modified = None;
    let mut size_from = 0;
    for i in 0..toks.len() {
        if let Some(clock) = toks.get(i + 1) {
            if let Some(t) = parse_listing_date(toks[i], clock) {
                modified = Some(t);
                size_from = i + 2;
                break;
            }
        }
    }
    let size = toks
        .iter()
        .enumerate()
        .skip(size_from)
        .find_map(|(i, t)| parse_size(t, toks.get(i + 1).copied()));
    (modified, size)
}

/// Parses an autoindex HTML page listing `base_url` (a folder URL ending
/// with `/`). Only links to direct children of the folder are kept; parent
/// links, sort links and links elsewhere are dropped. Hidden names
/// (starting with `.`) are skipped.
pub fn parse_autoindex(html: &str, base_url: &str) -> Result<Vec<Entry>> {
    let base = with_trailing_slash(base_url);
    let base_parsed =
        url::Url::parse(&base).map_err(|e| Error::invalid(&base, format!("bad URL: {e}")))?;
    let mut out: Vec<Entry> = Vec::new();
    let mut pos = 0;
    while let Some(a) = find_ci(html, "<a", pos) {
        pos = a + 2;
        // `<abbr>` and friends are not links.
        if !html[pos..].starts_with(|c: char| c.is_ascii_whitespace()) {
            continue;
        }
        let Some(tag_end) = html[pos..].find('>').map(|i| pos + i) else {
            break;
        };
        let tag = &html[pos..tag_end];
        pos = tag_end + 1;
        let Some(href) = tag_attr(tag, "href") else {
            continue;
        };
        let href = href.trim();
        if href.is_empty()
            || href.starts_with(['?', '#'])
            || href.to_ascii_lowercase().starts_with("mailto:")
            || href.to_ascii_lowercase().starts_with("javascript:")
        {
            continue;
        }
        let Ok(resolved) = resolve(&base, href) else {
            continue;
        };
        let Ok(mut resolved) = url::Url::parse(&resolved) else {
            continue;
        };
        resolved.set_query(None);
        resolved.set_fragment(None);
        if resolved.scheme() != base_parsed.scheme()
            || resolved.host_str() != base_parsed.host_str()
            || resolved.port_or_known_default() != base_parsed.port_or_known_default()
        {
            continue;
        }
        let Some(rest) = resolved.path().strip_prefix(base_parsed.path()) else {
            continue;
        };
        let is_dir = rest.ends_with('/');
        let segment = rest.trim_end_matches('/');
        if segment.is_empty() || segment.contains('/') {
            continue;
        }
        let name = percent_decode(segment);
        if name.starts_with('.') {
            continue;
        }

        // Context: the rest of this table row, or of this line.
        let after = find_ci(html, "</a>", pos).map_or(pos, |i| i + 4);
        let next_a = find_ci(html, "<a ", after).unwrap_or(html.len());
        let row_end = find_ci(html, "</tr", after).unwrap_or(html.len());
        let end = if row_end < next_a {
            row_end
        } else {
            html[after..next_a].find('\n').map_or(next_a, |i| after + i)
        };
        let (modified, size) = parse_details(&strip_tags(&html[after.min(end)..end]));

        let location = resolved.to_string();
        if let Some(prev) = out.iter_mut().find(|e| e.location == location) {
            prev.modified = prev.modified.or(modified);
            if prev.kind != EntryKind::Directory {
                prev.size = prev.size.or(size);
            }
            continue;
        }
        out.push(if is_dir {
            dir_entry(name, location, modified)
        } else {
            file_entry(name, location, size, modified)
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fp_core::source::EntryKind;

    fn by_name<'a>(v: &'a [Entry], n: &str) -> &'a Entry {
        v.iter()
            .find(|e| e.name == n)
            .unwrap_or_else(|| panic!("{n} missing in {v:?}"))
    }

    #[test]
    fn nginx() {
        let html = r#"<html><head><title>Index of /vr/</title></head>
<body><h1>Index of /vr/</h1><hr><pre><a href="../">../</a>
<a href="Sub%20Dir/">Sub Dir/</a>                                           12-Jan-2024 10:22                   -
<a href="Scene_180_LR.mp4">Scene_180_LR.mp4</a>                                   12-Jan-2024 10:22          1073741824
<a href="Scene_180_LR.funscript">Scene_180_LR.funscript</a>                             13-Jan-2024 08:00               12345
<a href="a%20very%20long%20name%20that%20nginx%20truncates.mkv">a very long name that nginx trunc..&gt;</a> 01-Feb-2024 00:00 42
</pre><hr></body></html>"#;
        let v = parse_autoindex(html, "http://nas/vr/").unwrap();
        assert_eq!(v.len(), 4);
        let d = by_name(&v, "Sub Dir");
        assert_eq!(d.kind, EntryKind::Directory);
        assert_eq!(d.location, "http://nas/vr/Sub%20Dir/");
        let s = by_name(&v, "Scene_180_LR.mp4");
        assert_eq!(s.kind, EntryKind::Video);
        assert_eq!(s.size, Some(1_073_741_824));
        assert_eq!(s.modified, Some(1_705_054_920));
        assert!(s.format.is_some());
        assert_eq!(by_name(&v, "Scene_180_LR.funscript").size, Some(12345));
        let long = by_name(&v, "a very long name that nginx truncates.mkv");
        assert_eq!(long.size, Some(42));
    }

    #[test]
    fn apache_table_and_fancy() {
        let html = r#"<table>
<tr><th valign="top"><img src="/icons/blank.gif" alt="[ICO]"></th><th><a href="?C=N;O=D">Name</a></th><th><a href="?C=M;O=A">Last modified</a></th><th><a href="?C=S;O=A">Size</a></th></tr>
<tr><td valign="top"><img src="/icons/back.gif" alt="[PARENTDIR]"></td><td><a href="/media/">Parent Directory</a></td><td>&nbsp;</td><td align="right">  - </td></tr>
<tr><td valign="top"><img src="/icons/folder.gif" alt="[DIR]"></td><td><a href="Movies/">Movies/</a></td><td align="right">2024-01-12 10:22  </td><td align="right">  - </td></tr>
<tr><td valign="top"><a href="Tom%20%26%20Jerry_3dh.mp4"><img src="/icons/movie.gif" alt="[VID]"></a></td><td><a href="Tom%20%26%20Jerry_3dh.mp4">Tom &amp; Jerry_3dh.mp4</a></td><td align="right">2024-01-12 10:22  </td><td align="right">1.5G</td></tr>
<tr><td><a href="http://elsewhere/x.mp4">x.mp4</a></td></tr>
<tr><td><a href=".hidden.mp4">.hidden.mp4</a></td></tr>
</table>"#;
        let v = parse_autoindex(html, "https://h:8443/media/vr").unwrap();
        assert_eq!(v.len(), 2, "{v:?}");
        let m = by_name(&v, "Movies");
        assert_eq!(m.location, "https://h:8443/media/vr/Movies/");
        let t = by_name(&v, "Tom & Jerry_3dh.mp4");
        assert_eq!(t.size, Some(1_610_612_736));
        assert_eq!(t.modified, Some(1_705_054_920));
        assert_eq!(
            t.location,
            "https://h:8443/media/vr/Tom%20%26%20Jerry_3dh.mp4"
        );
    }

    #[test]
    fn lighttpd_and_python() {
        let html = r#"<table summary="Directory Listing"><tbody>
<tr class="d"><td class="n"><a href="../">Parent Directory</a>/</td><td class="m">&nbsp;</td><td class="s">- &nbsp;</td><td class="t">Directory</td></tr>
<tr><td class="n"><a href="clip%231.webm">clip#1.webm</a></td><td class="m">2024-Jan-12 10:22:00</td><td class="s">4.0K</td><td class="t">video/webm</td></tr>
</tbody></table>
<ul><li><a href="%E6%97%A5%E6%9C%AC.mkv">日本.mkv</a></li></ul>"#;
        let v = parse_autoindex(html, "http://h/").unwrap();
        assert_eq!(v.len(), 2);
        let c = by_name(&v, "clip#1.webm");
        assert_eq!(c.size, Some(4096));
        assert_eq!(c.location, "http://h/clip%231.webm");
        let j = by_name(&v, "日本.mkv");
        assert_eq!(j.size, None);
        assert_eq!(j.kind, EntryKind::Video);
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("123", None), Some(123));
        assert_eq!(parse_size("1.5", Some("MiB")), Some(1_572_864));
        assert_eq!(parse_size("2k", None), Some(2048));
        assert_eq!(parse_size("-", None), None);
        assert_eq!(parse_size("1.5", None), None);
        assert_eq!(parse_size("video/mp4", None), None);
    }

    #[test]
    fn attributes() {
        assert_eq!(
            tag_attr(r#" class="x" href='a b.mp4'"#, "href").as_deref(),
            Some("a b.mp4")
        );
        assert_eq!(
            tag_attr(" HREF=x.mp4 title=y", "href").as_deref(),
            Some("x.mp4")
        );
        assert_eq!(
            tag_attr(r#" data-href="no" href="yes&amp;""#, "href").as_deref(),
            Some("yes&")
        );
        assert_eq!(tag_attr(" name=x", "href"), None);
    }

    #[test]
    fn source_strips_url_credentials() {
        let s = HttpDirSource::new(&HttpConfig {
            id: "h".into(),
            name: "H".into(),
            url: "http://bob:hunter2@nas/vr".into(),
            credentials: None,
            insecure_tls: false,
        })
        .unwrap();
        assert_eq!(s.root(), "http://nas/vr/");
        assert!(!format!("{s:?}").contains("hunter2"));
        assert!(!s.describe().contains("hunter2"));
        assert!(
            HttpDirSource::new(&HttpConfig {
                id: "h".into(),
                name: "H".into(),
                url: "ftp://nas/".into(),
                credentials: None,
                insecure_tls: false,
            })
            .is_err()
        );
    }
}
