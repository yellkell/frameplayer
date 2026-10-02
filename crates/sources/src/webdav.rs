//! WebDAV source: `PROPFIND Depth: 1` listings and ranged GETs.
//!
//! URIs use `webdav://` (plain HTTP) or `webdavs://` (HTTPS); `dav(s)://`
//! and raw `http(s)://` are accepted too.

use crate::config::SourceKind;
use crate::error::{Result, SourceError};
use crate::http::{check_status, HttpClient, HttpFile};
use crate::source::{Entry, RandomAccess, Source};
use crate::xml;
use async_trait::async_trait;
use bytes::Bytes;
use percent_encoding::percent_decode_str;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use reqwest::Method;
use url::Url;

const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:"><D:prop><D:resourcetype/><D:getcontentlength/><D:getlastmodified/><D:displayname/></D:prop></D:propfind>"#;

/// Convert a `webdav(s)://` URI to the HTTP(S) URL to request.
pub fn to_http_url(uri: &str) -> Result<Url> {
    let (scheme, rest) = uri
        .split_once("://")
        .ok_or_else(|| SourceError::InvalidUri(uri.into()))?;
    let http = match scheme.to_ascii_lowercase().as_str() {
        "webdav" | "dav" | "http" => "http",
        "webdavs" | "davs" | "https" => "https",
        _ => return Err(SourceError::InvalidUri(format!("not a WebDAV URI: {uri}"))),
    };
    Ok(Url::parse(&format!("{http}://{rest}"))?)
}

/// Convert an HTTP(S) URL back to our `webdav(s)://` form.
pub fn from_http_url(u: &Url) -> String {
    let s = u.to_string();
    if let Some(r) = s.strip_prefix("https://") {
        format!("webdavs://{r}")
    } else if let Some(r) = s.strip_prefix("http://") {
        format!("webdav://{r}")
    } else {
        s
    }
}

/// Parse an RFC 1123 date (`getlastmodified`) to Unix seconds.
pub fn parse_http_date(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc2822(s.trim())
        .ok()
        .map(|d| d.timestamp())
        .or_else(|| {
            chrono::DateTime::parse_from_rfc3339(s.trim())
                .ok()
                .map(|d| d.timestamp())
        })
}

/// Parse a 207 multistatus body. `request_url` is the collection that was
/// listed; its own entry is omitted.
pub fn parse_multistatus(body: &str, request_url: &Url) -> Result<Vec<Entry>> {
    let root = xml::parse(body)?;
    let self_path = percent_decode_str(request_url.path().trim_end_matches('/'))
        .decode_utf8_lossy()
        .into_owned();
    let mut out = Vec::new();
    for resp in root.children_named("response") {
        let Some(href) = resp.child_text("href") else {
            continue;
        };
        let Ok(url) = request_url.join(href) else {
            continue;
        };
        let path = percent_decode_str(url.path().trim_end_matches('/'))
            .decode_utf8_lossy()
            .into_owned();
        if path == self_path {
            continue;
        }
        // Pick the propstat with a 200 status (servers return 404 propstats
        // for properties they don't have).
        let prop = resp
            .children_named("propstat")
            .find(|ps| ps.child_text("status").is_none_or(|s| s.contains(" 200")))
            .and_then(|ps| ps.child("prop"));
        let is_dir = prop
            .and_then(|p| p.child("resourcetype"))
            .is_some_and(|rt| rt.child("collection").is_some())
            || href.ends_with('/');
        let name = prop
            .and_then(|p| p.child_text("displayname"))
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| path.rsplit('/').next().unwrap_or("").to_string());
        if name.is_empty() {
            continue;
        }
        out.push(Entry {
            name,
            uri: from_http_url(&url),
            is_dir,
            size: if is_dir {
                None
            } else {
                prop.and_then(|p| p.child_text("getcontentlength"))
                    .and_then(|v| v.parse().ok())
            },
            mtime: prop
                .and_then(|p| p.child_text("getlastmodified"))
                .and_then(parse_http_date),
            ..Default::default()
        });
    }
    out.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(out)
}

pub struct WebDavSource {
    client: HttpClient,
    root: Url,
}

impl WebDavSource {
    pub fn new(client: HttpClient, root_uri: &str) -> Result<Self> {
        let mut root = to_http_url(root_uri)?;
        if !root.path().ends_with('/') {
            let p = format!("{}/", root.path());
            root.set_path(&p);
        }
        Ok(WebDavSource { client, root })
    }

    pub async fn propfind(&self, url: &Url) -> Result<Vec<Entry>> {
        let mut h = HeaderMap::new();
        h.insert("Depth", HeaderValue::from_static("1"));
        h.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/xml; charset=utf-8"),
        );
        let method = Method::from_bytes(b"PROPFIND").expect("valid method");
        let resp = check_status(
            self.client
                .send(
                    method,
                    url.as_str(),
                    h,
                    Some(Bytes::from_static(PROPFIND_BODY.as_bytes())),
                )
                .await?,
        )?;
        let final_url = resp.url().clone();
        let body = resp.text().await?;
        parse_multistatus(&body, &final_url)
    }
}

#[async_trait]
impl Source for WebDavSource {
    fn kind(&self) -> SourceKind {
        SourceKind::WebDav
    }

    fn root_uri(&self) -> String {
        from_http_url(&self.root)
    }

    async fn list(&self, dir: &str) -> Result<Vec<Entry>> {
        let mut url = if dir.is_empty() {
            self.root.clone()
        } else {
            to_http_url(dir)?
        };
        if !url.path().ends_with('/') {
            let p = format!("{}/", url.path());
            url.set_path(&p);
        }
        self.propfind(&url).await
    }

    async fn open(&self, uri: &str) -> Result<Box<dyn RandomAccess>> {
        Ok(Box::new(
            HttpFile::open(self.client.clone(), to_http_url(uri)?.as_str()).await?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MULTISTATUS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:multistatus xmlns:d="DAV:" xmlns:s="http://sabredav.org/ns">
 <d:response><d:href>/remote.php/dav/files/me/VR/</d:href>
  <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype><d:getlastmodified>Tue, 01 Sep 2026 10:00:00 GMT</d:getlastmodified></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
 </d:response>
 <d:response><d:href>/remote.php/dav/files/me/VR/Scene%20One_180_LR.mp4</d:href>
  <d:propstat><d:prop><d:resourcetype/><d:getcontentlength>1234567890</d:getcontentlength><d:getlastmodified>Wed, 02 Sep 2026 12:30:00 GMT</d:getlastmodified></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  <d:propstat><d:prop><d:displayname/></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat>
 </d:response>
 <d:response><d:href>/remote.php/dav/files/me/VR/Sub/</d:href>
  <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
 </d:response>
</d:multistatus>"#;

    #[test]
    fn multistatus_parsing() {
        let req = Url::parse("https://cloud.example/remote.php/dav/files/me/VR/").unwrap();
        let e = parse_multistatus(MULTISTATUS, &req).unwrap();
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].name, "Sub");
        assert!(e[0].is_dir);
        assert_eq!(
            e[0].uri,
            "webdavs://cloud.example/remote.php/dav/files/me/VR/Sub/"
        );
        let f = &e[1];
        assert_eq!(f.name, "Scene One_180_LR.mp4");
        assert_eq!(f.size, Some(1_234_567_890));
        assert_eq!(f.mtime, Some(1_788_352_200));
        assert!(!f.is_dir);
    }

    #[test]
    fn uri_mapping() {
        assert_eq!(
            to_http_url("webdavs://h:8443/dav/x").unwrap().as_str(),
            "https://h:8443/dav/x"
        );
        assert_eq!(
            to_http_url("webdav://h/dav").unwrap().as_str(),
            "http://h/dav"
        );
        assert!(to_http_url("smb://h/x").is_err());
        assert_eq!(
            from_http_url(&Url::parse("http://h/a%20b").unwrap()),
            "webdav://h/a%20b"
        );
        let s = WebDavSource::new(HttpClient::new(None).unwrap(), "webdavs://h/dav").unwrap();
        assert_eq!(s.root_uri(), "webdavs://h/dav/");
    }
}
