//! WebDAV shares (Nextcloud, ownCloud, Synology/QNAP WebDAV, Apache
//! `mod_dav`, nginx-dav, rclone serve webdav, ...).
//!
//! Folders are listed with `PROPFIND` and `Depth: 1`; files are read with
//! the HTTP [`crate::http::HttpFile`] (GET with Range). Configured URLs may
//! use `webdav(s)://` or `dav(s)://`, which map to `http(s)://`. Locations
//! are absolute `http(s)://` URLs without credentials.

use crate::config::{HttpConfig, SourceKind};
use crate::error::{Error, Result};
use crate::http::{HttpClient, HttpOptions};
use crate::httpdir::clean_url_and_credentials;
use crate::timeutil::{parse_http_date, parse_iso8601};
use crate::urlutil::{
    is_http_url, parent_url, percent_decode, redact, resolve, webdav_to_http, with_trailing_slash,
};
use crate::xml;
use crate::{Source, dir_entry, file_entry, sort_entries};
use fp_core::source::{ByteSource, Entry};
use std::sync::Arc;

/// Properties requested for each child.
const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:"><D:prop>
<D:resourcetype/><D:getcontentlength/><D:getlastmodified/><D:displayname/><D:getcontenttype/>
</D:prop></D:propfind>"#;

/// A WebDAV share.
pub struct WebDavSource {
    id: String,
    name: String,
    root: String,
    client: HttpClient,
}

impl std::fmt::Debug for WebDavSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebDavSource")
            .field("id", &self.id)
            .field("root", &redact(&self.root))
            .field("client", &self.client)
            .finish()
    }
}

impl WebDavSource {
    /// Creates the source. Fails when the URL is not `http(s)://`,
    /// `webdav(s)://` or `dav(s)://`.
    pub fn new(config: &HttpConfig) -> Result<WebDavSource> {
        let url = webdav_to_http(&config.url);
        if !is_http_url(&url) {
            return Err(Error::invalid(
                &config.url,
                "expected an http(s)://, webdav(s):// or dav(s):// URL",
            ));
        }
        let (url, creds) = clean_url_and_credentials(&url, config.credentials.as_ref())?;
        let opts = HttpOptions {
            insecure_tls: config.insecure_tls,
            ..HttpOptions::default()
        };
        Ok(WebDavSource {
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

impl Source for WebDavSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> SourceKind {
        SourceKind::WebDav
    }

    fn describe(&self) -> String {
        format!("WebDAV {}", redact(&self.root))
    }

    fn list(&self, location: Option<&str>) -> Result<Vec<Entry>> {
        let url = with_trailing_slash(&webdav_to_http(location.unwrap_or(&self.root)));
        let reply = self
            .client
            .request(
                "PROPFIND",
                &url,
                &[
                    ("Depth", "1"),
                    ("Content-Type", "application/xml; charset=utf-8"),
                ],
                Some(PROPFIND_BODY.as_bytes()),
                64 << 20,
            )?
            .error_for_status(&url)?;
        if reply.status != 207 && reply.status != 200 {
            return Err(Error::HttpStatus {
                status: reply.status,
                url: redact(&url),
            });
        }
        let mut entries = parse_multistatus(&reply.text(), &url)?;
        sort_entries(&mut entries);
        Ok(entries)
    }

    fn open(&self, location: &str) -> Result<Arc<dyn ByteSource>> {
        Ok(Arc::new(self.client.open(&webdav_to_http(location))?))
    }

    fn parent(&self, location: &str) -> Option<String> {
        parent_url(location)
    }
}

/// Path of a URL, decoded, without trailing slash, for comparing hrefs.
fn norm_path(url: &str) -> String {
    let path = url::Url::parse(url)
        .map(|u| u.path().to_string())
        .unwrap_or_else(|_| url.to_string());
    percent_decode(path.trim_end_matches('/'))
}

/// Parses a `207 Multi-Status` PROPFIND answer for the folder `request_url`.
/// The folder itself is left out. Properties come from the `propstat` with a
/// 2xx status (or the only one when no status is given).
pub fn parse_multistatus(xml_text: &str, request_url: &str) -> Result<Vec<Entry>> {
    let doc = xml::parse(xml_text)?;
    let ms = doc
        .find("multistatus")
        .ok_or_else(|| Error::parse("WebDAV response", "no multistatus element"))?;
    let self_path = norm_path(request_url);
    let mut out = Vec::new();
    for resp in ms.children_named("response") {
        let Some(href) = resp.child_text("href") else {
            continue;
        };
        let Ok(location) = resolve(request_url, href) else {
            log::debug!("skipping unusable WebDAV href {href:?}");
            continue;
        };
        if norm_path(&location) == self_path {
            continue;
        }
        let mut is_dir = false;
        let mut size = None;
        let mut modified = None;
        let mut display = None;
        for ps in resp.children_named("propstat") {
            let ok = ps.child_text("status").is_none_or(|s| {
                s.split_whitespace()
                    .nth(1)
                    .is_some_and(|c| c.starts_with('2'))
            });
            if !ok {
                continue;
            }
            let Some(prop) = ps.child("prop") else {
                continue;
            };
            if let Some(rt) = prop.child("resourcetype") {
                is_dir |= rt.child("collection").is_some();
            }
            if let Some(len) = prop.child_text("getcontentlength") {
                size = len.parse::<u64>().ok();
            }
            if let Some(lm) = prop.child_text("getlastmodified") {
                modified = parse_http_date(lm).or_else(|| parse_iso8601(lm));
            }
            if let Some(dn) = prop.child_text("displayname") {
                display = Some(dn.to_string());
            }
        }
        // Some servers omit the trailing slash on collection hrefs.
        let location = if is_dir {
            with_trailing_slash(&location)
        } else {
            location
        };
        let mut name = crate::urlutil::last_segment(&location);
        if name.is_empty() {
            name = display.unwrap_or_default();
        }
        if name.is_empty() || name.starts_with('.') {
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

    #[test]
    fn parses_nextcloud_style_multistatus() {
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:s="http://sabredav.org/ns" xmlns:oc="http://owncloud.org/ns">
 <d:response><d:href>/remote.php/dav/files/bob/VR/</d:href>
  <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype>
   <d:getlastmodified>Fri, 12 Jan 2024 10:22:33 GMT</d:getlastmodified></d:prop>
   <d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  <d:propstat><d:prop><d:getcontentlength/></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat>
 </d:response>
 <d:response><d:href>/remote.php/dav/files/bob/VR/Sub%20Folder/</d:href>
  <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop>
   <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
 <d:response><d:href>/remote.php/dav/files/bob/VR/Clip%20%231_MKX200.mp4</d:href>
  <d:propstat><d:prop><d:resourcetype/><d:getcontentlength>123456789</d:getcontentlength>
   <d:getlastmodified>Fri, 12 Jan 2024 10:22:33 GMT</d:getlastmodified></d:prop>
   <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
 <d:response><d:href>http://nas/remote.php/dav/files/bob/VR/%C3%BC.funscript</d:href>
  <d:propstat><d:prop><d:getcontentlength>10</d:getcontentlength></d:prop></d:propstat></d:response>
</d:multistatus>"#;
        let v = parse_multistatus(xml, "http://nas/remote.php/dav/files/bob/VR/").unwrap();
        assert_eq!(v.len(), 3, "{v:?}");
        assert_eq!(v[0].name, "Sub Folder");
        assert_eq!(v[0].kind, EntryKind::Directory);
        assert_eq!(
            v[0].location,
            "http://nas/remote.php/dav/files/bob/VR/Sub%20Folder/"
        );
        assert_eq!(v[1].name, "Clip #1_MKX200.mp4");
        assert_eq!(v[1].kind, EntryKind::Video);
        assert_eq!(v[1].size, Some(123_456_789));
        assert_eq!(v[1].modified, Some(1_705_054_953));
        assert_eq!(
            v[1].format.map(|f| f.projection),
            Some(fp_core::format::Projection::fisheye(200.0))
        );
        assert_eq!(
            v[1].location,
            "http://nas/remote.php/dav/files/bob/VR/Clip%20%231_MKX200.mp4"
        );
        assert_eq!(v[2].name, "ü.funscript");
        assert_eq!(v[2].size, Some(10));
    }

    #[test]
    fn collection_without_trailing_slash_and_errors() {
        let xml = r#"<multistatus xmlns="DAV:"><response><href>/dav/a</href>
<propstat><prop><resourcetype><collection/></resourcetype></prop><status>HTTP/1.1 200 OK</status></propstat>
</response><response><href>/dav</href><propstat><prop><resourcetype><collection/></resourcetype></prop></propstat></response></multistatus>"#;
        let v = parse_multistatus(xml, "https://h/dav/").unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].location, "https://h/dav/a/");
        assert!(parse_multistatus("<html/>", "https://h/").is_err());
    }

    #[test]
    fn accepts_dav_schemes() {
        let s = WebDavSource::new(&HttpConfig {
            id: "w".into(),
            name: "W".into(),
            url: "davs://u:secret@nas:5006/vr".into(),
            credentials: None,
            insecure_tls: true,
        })
        .unwrap();
        assert_eq!(s.root(), "https://nas:5006/vr/");
        assert!(!s.describe().contains("secret"));
        assert!(!format!("{s:?}").contains("secret"));
    }
}
