//! HTTP(S) transport shared by the HTTP, WebDAV, DLNA, DeoVR and HLS/DASH
//! code, plus the plain HTTP source.
//!
//! [`HttpClient`] wraps reqwest (rustls) and adds reactive Basic/Digest
//! authentication: credentials are only sent after the server asks, in the
//! scheme it asks for, so a Digest server never sees a Basic password.
//! [`HttpFile`] implements [`RandomAccess`] with `Range` requests and falls
//! back to forward streaming (restarting the GET on backwards seeks) for
//! servers that ignore ranges.

pub mod digest;

use crate::config::SourceKind;
use crate::error::{Result, SourceError};
use crate::source::{Entry, RandomAccess, Source};
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use digest::Challenge;
use parking_lot::Mutex;
use reqwest::header::{
    HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_RANGE, RANGE, WWW_AUTHENTICATE,
};
use reqwest::{Method, Response, StatusCode};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use url::Url;
use zeroize::{Zeroize, ZeroizeOnDrop};

pub const USER_AGENT: &str = concat!("FramePlayer/", env!("CARGO_PKG_VERSION"));

/// Username/password for HTTP-based sources.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct HttpAuth {
    pub username: String,
    pub password: String,
}

impl HttpAuth {
    pub fn new(username: &str, password: &str) -> Self {
        HttpAuth {
            username: username.into(),
            password: password.into(),
        }
    }
}

impl std::fmt::Debug for HttpAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpAuth")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone)]
enum AuthState {
    None,
    Basic,
    Digest(digest::DigestChallenge),
}

/// Shared HTTP client with lazy authentication. Cheap to clone.
#[derive(Clone)]
pub struct HttpClient {
    inner: reqwest::Client,
    auth: Option<HttpAuth>,
    state: Arc<Mutex<AuthState>>,
    nc: Arc<AtomicU32>,
}

impl std::fmt::Debug for HttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpClient")
            .field("auth", &self.auth)
            .finish()
    }
}

impl HttpClient {
    pub fn new(auth: Option<HttpAuth>) -> Result<Self> {
        let inner = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()?;
        Ok(Self::with_client(inner, auth))
    }

    /// Wrap an existing reqwest client (custom proxy/TLS settings, tests).
    pub fn with_client(inner: reqwest::Client, auth: Option<HttpAuth>) -> Self {
        HttpClient {
            inner,
            auth,
            state: Arc::new(Mutex::new(AuthState::None)),
            nc: Arc::new(AtomicU32::new(0)),
        }
    }

    pub fn reqwest(&self) -> &reqwest::Client {
        &self.inner
    }

    fn auth_header(&self, method: &Method, url: &Url) -> Option<HeaderValue> {
        let auth = self.auth.as_ref()?;
        let state = self.state.lock().clone();
        let value = match state {
            AuthState::None => return None,
            AuthState::Basic => {
                use base64::Engine;
                let raw = format!("{}:{}", auth.username, auth.password);
                format!(
                    "Basic {}",
                    base64::engine::general_purpose::STANDARD.encode(raw)
                )
            }
            AuthState::Digest(c) => {
                let mut uri = url.path().to_string();
                if let Some(q) = url.query() {
                    uri.push('?');
                    uri.push_str(q);
                }
                let nc = self.nc.fetch_add(1, Ordering::Relaxed) + 1;
                let cnonce = hex::encode(rand::random::<[u8; 8]>());
                digest::digest_authorization(
                    &c,
                    &auth.username,
                    &auth.password,
                    method.as_str(),
                    &uri,
                    nc,
                    &cnonce,
                )
            }
        };
        let mut v = HeaderValue::from_str(&value).ok()?;
        v.set_sensitive(true);
        Some(v)
    }

    async fn send_once(
        &self,
        method: &Method,
        url: &Url,
        headers: &HeaderMap,
        body: &Option<Bytes>,
    ) -> Result<Response> {
        let mut req = self
            .inner
            .request(method.clone(), url.clone())
            .headers(headers.clone());
        if let Some(h) = self.auth_header(method, url) {
            req = req.header(AUTHORIZATION, h);
        }
        if let Some(b) = body {
            req = req.body(b.clone());
        }
        Ok(req.send().await?)
    }

    /// Send a request, answering one auth challenge if the server issues
    /// one. Returns the response whatever its status, except that a final
    /// 401 maps to [`SourceError::Auth`].
    pub async fn send(
        &self,
        method: Method,
        url: &str,
        headers: HeaderMap,
        body: Option<Bytes>,
    ) -> Result<Response> {
        let url = Url::parse(url)?;
        let resp = self.send_once(&method, &url, &headers, &body).await?;
        if resp.status() != StatusCode::UNAUTHORIZED {
            return Ok(resp);
        }
        if self.auth.is_none() {
            return Err(SourceError::Auth);
        }
        let challenges: Vec<Challenge> = resp
            .headers()
            .get_all(WWW_AUTHENTICATE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .filter_map(digest::parse_challenge)
            .collect();
        let new_state = challenges
            .iter()
            .find_map(|c| match c {
                Challenge::Digest(d) => Some(AuthState::Digest(d.clone())),
                _ => None,
            })
            .or_else(|| {
                challenges
                    .contains(&Challenge::Basic)
                    .then_some(AuthState::Basic)
            });
        let Some(new_state) = new_state else {
            return Err(SourceError::Auth);
        };
        let basic_already_rejected = matches!(
            (&*self.state.lock(), &new_state),
            (AuthState::Basic, AuthState::Basic)
        );
        if basic_already_rejected {
            // We already sent Basic credentials and they were rejected.
            return Err(SourceError::Auth);
        }
        *self.state.lock() = new_state;
        self.nc.store(0, Ordering::Relaxed);
        let resp = self.send_once(&method, &url, &headers, &body).await?;
        if resp.status() == StatusCode::UNAUTHORIZED {
            return Err(SourceError::Auth);
        }
        Ok(resp)
    }

    pub async fn get(&self, url: &str) -> Result<Response> {
        check_status(self.send(Method::GET, url, HeaderMap::new(), None).await?)
    }

    pub async fn get_bytes(&self, url: &str) -> Result<Bytes> {
        Ok(self.get(url).await?.bytes().await?)
    }

    pub async fn get_text(&self, url: &str) -> Result<String> {
        Ok(self.get(url).await?.text().await?)
    }

    /// GET a byte range (`end` inclusive, `None` = to the end).
    pub async fn get_range(&self, url: &str, start: u64, end: Option<u64>) -> Result<Response> {
        let mut h = HeaderMap::new();
        let v = match end {
            Some(e) => format!("bytes={start}-{e}"),
            None => format!("bytes={start}-"),
        };
        h.insert(RANGE, HeaderValue::from_str(&v).expect("ascii"));
        self.send(Method::GET, url, h, None).await
    }
}

/// Map non-success statuses to errors.
pub fn check_status(resp: Response) -> Result<Response> {
    let s = resp.status();
    if s.is_success() {
        return Ok(resp);
    }
    let url = resp.url().to_string();
    Err(match s {
        StatusCode::NOT_FOUND | StatusCode::GONE => SourceError::NotFound(url),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => SourceError::Auth,
        _ => SourceError::Status {
            status: s.as_u16(),
            url,
        },
    })
}

/// Parse `Content-Range: bytes 0-0/12345` → total size.
pub fn parse_content_range_total(v: &str) -> Option<u64> {
    let total = v.trim().strip_prefix("bytes")?.trim().rsplit_once('/')?.1;
    total.trim().parse().ok()
}

struct StreamState {
    resp: Response,
    /// File offset of `pending[0]`.
    pos: u64,
    pending: Bytes,
}

/// A remote file read over HTTP(S).
pub struct HttpFile {
    client: HttpClient,
    url: String,
    size: Option<u64>,
    ranged: AtomicBool,
    stream: tokio::sync::Mutex<Option<StreamState>>,
}

impl HttpFile {
    /// Probe the URL (one `Range: bytes=0-0` GET) and open it.
    pub async fn open(client: HttpClient, url: &str) -> Result<HttpFile> {
        let resp = client.get_range(url, 0, Some(0)).await?;
        let final_url = resp.url().to_string();
        match resp.status() {
            StatusCode::PARTIAL_CONTENT => {
                let size = resp
                    .headers()
                    .get(CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(parse_content_range_total);
                Ok(HttpFile {
                    client,
                    url: final_url,
                    size,
                    ranged: AtomicBool::new(true),
                    stream: Default::default(),
                })
            }
            StatusCode::RANGE_NOT_SATISFIABLE => Ok(HttpFile {
                client,
                url: final_url,
                size: Some(0),
                ranged: AtomicBool::new(true),
                stream: Default::default(),
            }),
            s if s.is_success() => {
                tracing::debug!("{url}: server ignores Range, using forward streaming");
                let size = resp.content_length();
                let st = StreamState {
                    resp,
                    pos: 0,
                    pending: Bytes::new(),
                };
                Ok(HttpFile {
                    client,
                    url: final_url,
                    size,
                    ranged: AtomicBool::new(false),
                    stream: tokio::sync::Mutex::new(Some(st)),
                })
            }
            _ => Err(check_status(resp)
                .err()
                .unwrap_or(SourceError::Protocol("unexpected status".into()))),
        }
    }

    /// The URL after redirects.
    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn supports_ranges(&self) -> bool {
        self.ranged.load(Ordering::Relaxed)
    }

    async fn read_streaming(
        &self,
        offset: u64,
        len: usize,
        fresh: Option<Response>,
    ) -> Result<Bytes> {
        let mut guard = self.stream.lock().await;
        if let Some(resp) = fresh {
            *guard = Some(StreamState {
                resp,
                pos: 0,
                pending: Bytes::new(),
            });
        }
        let restart = match guard.as_ref() {
            None => true,
            Some(st) => st.pos > offset,
        };
        if restart {
            let resp = check_status(
                self.client
                    .send(Method::GET, &self.url, HeaderMap::new(), None)
                    .await?,
            )?;
            *guard = Some(StreamState {
                resp,
                pos: 0,
                pending: Bytes::new(),
            });
        }
        let st = guard.as_mut().expect("stream state set above");
        // Skip forward to `offset`.
        while st.pos + st.pending.len() as u64 <= offset {
            st.pos += st.pending.len() as u64;
            match st.resp.chunk().await? {
                Some(c) => st.pending = c,
                None => {
                    st.pending = Bytes::new();
                    return Ok(Bytes::new());
                }
            }
        }
        let skip = (offset - st.pos) as usize;
        let _ = st.pending.split_to(skip);
        st.pos = offset;
        let mut out = BytesMut::with_capacity(len.min(8 << 20));
        while out.len() < len {
            if st.pending.is_empty() {
                match st.resp.chunk().await? {
                    Some(c) => st.pending = c,
                    None => break,
                }
            }
            let take = (len - out.len()).min(st.pending.len());
            out.extend_from_slice(&st.pending.split_to(take));
            st.pos += take as u64;
        }
        Ok(out.freeze())
    }
}

#[async_trait]
impl RandomAccess for HttpFile {
    async fn read_at(&self, offset: u64, len: usize) -> Result<Bytes> {
        if len == 0 || self.size.is_some_and(|s| offset >= s) {
            return Ok(Bytes::new());
        }
        if !self.ranged.load(Ordering::Relaxed) {
            return self.read_streaming(offset, len, None).await;
        }
        let end = offset + len as u64 - 1;
        let resp = self.client.get_range(&self.url, offset, Some(end)).await?;
        match resp.status() {
            StatusCode::PARTIAL_CONTENT => {
                let mut b = resp.bytes().await?;
                b.truncate(len);
                Ok(b)
            }
            StatusCode::RANGE_NOT_SATISFIABLE => Ok(Bytes::new()),
            s if s.is_success() => {
                // Server stopped honouring ranges (e.g. a CDN edge); degrade.
                self.ranged.store(false, Ordering::Relaxed);
                self.read_streaming(offset, len, Some(resp)).await
            }
            _ => Err(check_status(resp)
                .err()
                .unwrap_or(SourceError::Protocol("unexpected status".into()))),
        }
    }

    fn size(&self) -> Option<u64> {
        self.size
    }
}

/// Extract link targets from an HTML directory index (nginx/Apache
/// autoindex, `python -m http.server`, Caddy browse).
pub fn parse_html_index(html: &str, base: &Url) -> Vec<Entry> {
    let mut out = Vec::new();
    let lower = html.to_ascii_lowercase();
    let mut i = 0;
    while let Some(p) = lower[i..].find("href=") {
        let start = i + p + 5;
        let bytes = html.as_bytes();
        let (val, next) = match bytes.get(start) {
            Some(b'"') | Some(b'\'') => {
                let q = bytes[start] as char;
                match html[start + 1..].find(q) {
                    Some(e) => (&html[start + 1..start + 1 + e], start + 2 + e),
                    None => break,
                }
            }
            _ => {
                let e = html[start..]
                    .find(|c: char| c.is_whitespace() || c == '>')
                    .unwrap_or(html.len() - start);
                (&html[start..start + e], start + e)
            }
        };
        i = next;
        let val = val.replace("&amp;", "&");
        if val.is_empty()
            || val.starts_with('?')
            || val.starts_with('#')
            || val.starts_with("javascript:")
            || val.starts_with("mailto:")
        {
            continue;
        }
        let Ok(u) = base.join(&val) else { continue };
        // Only descendants of the listed directory.
        if u.origin() != base.origin()
            || !u.path().starts_with(base.path())
            || u.path() == base.path()
        {
            continue;
        }
        let is_dir = u.path().ends_with('/');
        let seg = u
            .path()
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("");
        let name = percent_encoding::percent_decode_str(seg)
            .decode_utf8_lossy()
            .into_owned();
        if name.is_empty() || out.iter().any(|e: &Entry| e.uri == u.as_str()) {
            continue;
        }
        out.push(Entry {
            name,
            uri: u.to_string(),
            is_dir,
            ..Default::default()
        });
    }
    out
}

/// Plain HTTP(S): a direct file URL, or a directory with an HTML index.
pub struct HttpSource {
    client: HttpClient,
    root: Url,
}

impl HttpSource {
    pub fn new(client: HttpClient, root: &str) -> Result<Self> {
        Ok(HttpSource {
            client,
            root: Url::parse(root)?,
        })
    }
}

#[async_trait]
impl Source for HttpSource {
    fn kind(&self) -> SourceKind {
        SourceKind::Http
    }

    fn root_uri(&self) -> String {
        self.root.to_string()
    }

    async fn list(&self, dir: &str) -> Result<Vec<Entry>> {
        let dir = if dir.is_empty() {
            self.root.clone()
        } else {
            Url::parse(dir)?
        };
        if crate::is_video_name(dir.path()) {
            let name = dir.path().rsplit('/').next().unwrap_or("video");
            let name = percent_encoding::percent_decode_str(name)
                .decode_utf8_lossy()
                .into_owned();
            return Ok(vec![Entry {
                name,
                uri: dir.to_string(),
                ..Default::default()
            }]);
        }
        let resp = self.client.get(dir.as_str()).await?;
        let base = resp.url().clone();
        let html = resp.text().await?;
        Ok(parse_html_index(&html, &base))
    }

    async fn open(&self, uri: &str) -> Result<Box<dyn RandomAccess>> {
        Ok(Box::new(HttpFile::open(self.client.clone(), uri).await?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range() {
        assert_eq!(parse_content_range_total("bytes 0-0/12345"), Some(12345));
        assert_eq!(parse_content_range_total("bytes 0-0/*"), None);
    }

    #[test]
    fn html_index() {
        let base = Url::parse("http://h/videos/").unwrap();
        let html = r#"<a href="../">..</a><a href="sub%20dir/">sub dir/</a>
            <A HREF='clip_180_LR.mp4'>x</A><a href="?C=N;O=D">sort</a><a href=/other/x.mp4>no</a>
            <a href="http://evil/x.mp4">no</a><a href="clip_180_LR.mp4">dup</a>"#;
        let e = parse_html_index(html, &base);
        assert_eq!(e.len(), 2, "{e:?}");
        assert_eq!(e[0].name, "sub dir");
        assert!(e[0].is_dir);
        assert_eq!(e[1].uri, "http://h/videos/clip_180_LR.mp4");
    }
}
