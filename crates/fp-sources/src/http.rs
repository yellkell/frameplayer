//! HTTP(S) client with retries, and [`HttpFile`]: a random-access
//! [`ByteSource`] over HTTP Range requests.
//!
//! - Ranges are fetched in blocks through the shared block cache, with
//!   background read-ahead when reading sequentially.
//! - One `ureq` agent per client keeps connections alive between requests.
//! - Transient failures (connection errors, timeouts, 408/429/5xx) are
//!   retried with exponential backoff.
//! - Servers without range support fall back to one sequential stream:
//!   forward seeks skip data, backward seeks outside the cache fail with
//!   [`std::io::ErrorKind::Unsupported`].
//! - TLS is rustls with the ring provider and webpki roots.

use crate::cache::{CacheOptions, CacheStats, CachedSource, RangeFetch, lock};
use crate::config::Credentials;
use crate::error::{Error, Result};
use crate::urlutil::{basic_auth_value, redact, split_credentials};
use fp_core::ByteSource;
use std::io::{self, Read};
use std::sync::Mutex;
use std::time::Duration;
use ureq::http;

/// Connection, retry and caching settings.
#[derive(Clone, Debug)]
pub struct HttpOptions {
    /// Time allowed to open a TCP/TLS connection.
    pub connect_timeout: Duration,
    /// Time allowed for response headers, and for the body of a bounded
    /// request (one cache block, a listing, a feed).
    pub read_timeout: Duration,
    /// Extra attempts after a transient failure.
    pub max_retries: u32,
    /// Delay before the first retry; doubles on each attempt (max 5 s).
    pub retry_delay: Duration,
    /// Accept invalid TLS certificates (self-signed NAS certificates).
    pub insecure_tls: bool,
    /// `User-Agent` header.
    pub user_agent: String,
    /// Block cache used by [`HttpFile`].
    pub cache: CacheOptions,
    /// Furthest a forward seek may skip by reading and discarding data when
    /// the server does not support ranges.
    pub max_forward_skip: u64,
}

impl Default for HttpOptions {
    fn default() -> Self {
        HttpOptions {
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(30),
            max_retries: 3,
            retry_delay: Duration::from_millis(250),
            insecure_tls: false,
            user_agent: concat!("FramePlayer/", env!("CARGO_PKG_VERSION")).to_string(),
            cache: CacheOptions::default(),
            max_forward_skip: 256 << 20,
        }
    }
}

/// A complete HTTP response with a bounded body.
#[derive(Clone, Debug)]
pub struct HttpReply {
    /// Status code.
    pub status: u16,
    /// Headers with lower-case names.
    pub headers: Vec<(String, String)>,
    /// Response body.
    pub body: Vec<u8>,
}

impl HttpReply {
    /// First value of a header (name compared case-insensitively).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Body as text (invalid UTF-8 replaced).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Turns a non-2xx status into the matching [`Error`].
    pub fn error_for_status(self, url: &str) -> Result<HttpReply> {
        status_error(self.status, url).map_or(Ok(self), Err)
    }
}

/// Maps a status to an error; `None` for 2xx.
fn status_error(status: u16, url: &str) -> Option<Error> {
    match status {
        200..=299 => None,
        401 | 403 => Some(Error::Auth(redact(url))),
        404 | 410 => Some(Error::NotFound(redact(url))),
        _ => Some(Error::HttpStatus {
            status,
            url: redact(url),
        }),
    }
}

fn is_transient_status(status: u16) -> bool {
    matches!(status, 408 | 425 | 429 | 500 | 502 | 503 | 504)
}

fn is_transient_error(e: &ureq::Error) -> bool {
    matches!(
        e,
        ureq::Error::Io(_)
            | ureq::Error::Timeout(_)
            | ureq::Error::ConnectionFailed
            | ureq::Error::HostNotFound
            | ureq::Error::Protocol(_)
    )
}

/// Outcome of one attempt.
enum Failure {
    /// Worth retrying, optionally after a server-requested delay.
    Retry(Error, Option<Duration>),
    /// Give up.
    Fatal(Error),
}

impl From<Error> for Failure {
    fn from(e: Error) -> Self {
        Failure::Fatal(e)
    }
}

type Response = http::Response<ureq::Body>;

/// HTTP client shared by the HTTP, WebDAV, DLNA and feed sources.
///
/// Cloning is cheap and shares the connection pool.
#[derive(Clone)]
pub struct HttpClient {
    agent: ureq::Agent,
    auth: Option<String>,
    opts: HttpOptions,
}

impl std::fmt::Debug for HttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpClient")
            .field("auth", &self.auth.as_ref().map(|_| "***"))
            .field("opts", &self.opts)
            .finish_non_exhaustive()
    }
}

impl HttpClient {
    /// Creates a client. `credentials` are sent as HTTP Basic auth with
    /// every request; credentials embedded in a URL are used when none are
    /// given here.
    pub fn new(opts: HttpOptions, credentials: Option<&Credentials>) -> HttpClient {
        let tls = ureq::tls::TlsConfig::builder()
            .disable_verification(opts.insecure_tls)
            .build();
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .allow_non_standard_methods(true)
            .timeout_connect(Some(opts.connect_timeout))
            .timeout_recv_response(Some(opts.read_timeout))
            .timeout_recv_body(Some(opts.read_timeout))
            .max_idle_connections_per_host(4)
            .user_agent(opts.user_agent.as_str())
            .tls_config(tls)
            .build();
        HttpClient {
            agent: ureq::Agent::new_with_config(config),
            auth: credentials.map(basic_auth_value),
            opts,
        }
    }

    /// The options this client was created with.
    pub fn options(&self) -> &HttpOptions {
        &self.opts
    }

    fn retrying<T>(
        &self,
        url: &str,
        mut op: impl FnMut() -> std::result::Result<T, Failure>,
    ) -> Result<T> {
        let mut attempt = 0;
        loop {
            match op() {
                Ok(v) => return Ok(v),
                Err(Failure::Fatal(e)) => return Err(e),
                Err(Failure::Retry(e, after)) => {
                    if attempt >= self.opts.max_retries {
                        return Err(e);
                    }
                    let backoff = self
                        .opts
                        .retry_delay
                        .saturating_mul(1 << attempt.min(8))
                        .min(Duration::from_secs(5));
                    let delay = after.unwrap_or(backoff).min(Duration::from_secs(10));
                    log::debug!("retrying {} in {delay:?} after: {e}", redact(url));
                    std::thread::sleep(delay);
                    attempt += 1;
                }
            }
        }
    }

    /// Sends one request. Transient transport errors become `Retry`.
    fn send_once(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
        unbounded_body: bool,
    ) -> std::result::Result<Response, Failure> {
        let (clean_url, url_creds) = split_credentials(url)?;
        let auth = self
            .auth
            .clone()
            .or_else(|| url_creds.as_ref().map(basic_auth_value));
        let mut builder = http::Request::builder().method(method).uri(&clean_url);
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        if let Some(a) = &auth {
            builder = builder.header("Authorization", a.as_str());
        }
        let build_err = |e: http::Error| Failure::Fatal(Error::invalid(url, e.to_string()));
        let result = match body {
            Some(b) => {
                let req = builder.body(b.to_vec()).map_err(build_err)?;
                self.agent.run(req)
            }
            None => {
                let req = builder.body(()).map_err(build_err)?;
                if unbounded_body {
                    let req = self
                        .agent
                        .configure_request(req)
                        .timeout_recv_body(None)
                        .build();
                    self.agent.run(req)
                } else {
                    self.agent.run(req)
                }
            }
        };
        result.map_err(|e| {
            let err = Error::Http {
                url: redact(&clean_url),
                message: e.to_string(),
            };
            if is_transient_error(&e) {
                Failure::Retry(err, None)
            } else {
                Failure::Fatal(err)
            }
        })
    }

    /// Sends a request and reads the whole response body (at most `limit`
    /// bytes). Transient failures (including 408/429/5xx statuses) are
    /// retried; every other status is returned as is.
    pub fn request(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
        limit: u64,
    ) -> Result<HttpReply> {
        self.request_inner(method, url, headers, body, limit, true, |_| true)
            .map(|r| r.0)
    }

    /// Like [`HttpClient::request`], but a 5xx status is returned instead of
    /// retried: SOAP reports faults as `500` with a meaningful body.
    pub fn request_no_status_retry(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
        limit: u64,
    ) -> Result<HttpReply> {
        self.request_inner(method, url, headers, body, limit, false, |_| true)
            .map(|r| r.0)
    }

    /// Like [`HttpClient::request`], but the body is only read when
    /// `want_body` accepts the response headers (lower-case names).
    /// Otherwise the reply comes back with an empty body and `false`, and
    /// the connection is dropped without downloading anything. Used to tell
    /// a JSON document from a media stream at an unknown URL.
    pub fn request_if(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
        limit: u64,
        want_body: impl Fn(&HttpReply) -> bool,
    ) -> Result<(HttpReply, bool)> {
        self.request_inner(method, url, headers, body, limit, true, want_body)
    }

    #[allow(clippy::too_many_arguments)]
    fn request_inner(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
        limit: u64,
        retry_status: bool,
        want_body: impl Fn(&HttpReply) -> bool,
    ) -> Result<(HttpReply, bool)> {
        self.retrying(url, || {
            let resp = self.send_once(method, url, headers, body, false)?;
            let status = resp.status().as_u16();
            let retry_after = retry_after(&resp);
            let headers = resp
                .headers()
                .iter()
                .map(|(k, v)| {
                    (
                        k.as_str().to_ascii_lowercase(),
                        String::from_utf8_lossy(v.as_bytes()).into_owned(),
                    )
                })
                .collect();
            let mut reply = HttpReply {
                status,
                headers,
                body: Vec::new(),
            };
            if retry_status && is_transient_status(status) {
                return Err(Failure::Retry(
                    Error::HttpStatus {
                        status,
                        url: redact(url),
                    },
                    retry_after,
                ));
            }
            if !want_body(&reply) {
                return Ok((reply, false));
            }
            reply.body = read_body(resp.into_body().into_reader(), limit).map_err(|e| {
                Failure::Retry(
                    Error::Http {
                        url: redact(url),
                        message: format!("reading body: {e}"),
                    },
                    None,
                )
            })?;
            Ok((reply, true))
        })
    }

    /// GETs a URL and returns its body (at most `limit` bytes); non-2xx
    /// statuses are errors.
    pub fn get_bytes(&self, url: &str, limit: u64) -> Result<Vec<u8>> {
        Ok(self
            .request("GET", url, &[], None, limit)?
            .error_for_status(url)?
            .body)
    }

    /// GETs a URL as text (at most 32 MiB).
    pub fn get_text(&self, url: &str) -> Result<String> {
        let body = self.get_bytes(url, 32 << 20)?;
        Ok(String::from_utf8_lossy(&body).into_owned())
    }

    /// Opens a URL for random access.
    pub fn open(&self, url: &str) -> Result<HttpFile> {
        HttpFile::open(self, url)
    }
}

fn retry_after(resp: &Response) -> Option<Duration> {
    resp.headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// Reads a body to its end, keeping at most `limit` bytes. Reading to EOF
/// lets the agent return the connection to its pool; when the body is
/// longer than `limit` the rest is abandoned (and the connection closed).
fn read_body(mut reader: impl Read, limit: u64) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    (&mut reader).take(limit).read_to_end(&mut out)?;
    if out.len() as u64 == limit {
        // Probe for EOF so a body of exactly `limit` bytes is fully consumed.
        let mut probe = [0u8; 1];
        let _ = reader.read(&mut probe);
    }
    Ok(out)
}

/// Parsed `Content-Range` header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ContentRange {
    /// First and last byte, `None` for `bytes */total`.
    pub range: Option<(u64, u64)>,
    /// Complete length, `None` for `*`.
    pub total: Option<u64>,
}

pub(crate) fn parse_content_range(v: &str) -> Option<ContentRange> {
    let v = v.trim();
    let rest = v.strip_prefix("bytes")?.trim_start();
    let rest = rest.strip_prefix('=').unwrap_or(rest).trim();
    let (range, total) = rest.split_once('/')?;
    let total = match total.trim() {
        "*" => None,
        t => Some(t.parse().ok()?),
    };
    let range = match range.trim() {
        "*" => None,
        r => {
            let (a, b) = r.split_once('-')?;
            let (a, b) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
            if b < a {
                return None;
            }
            Some((a, b))
        }
    };
    Some(ContentRange { range, total })
}

struct Stream {
    reader: Box<dyn Read + Send>,
    pos: u64,
}

/// Range fetcher behind [`HttpFile`].
struct HttpFetcher {
    client: HttpClient,
    url: String,
    size: Option<u64>,
    ranges: bool,
    stream: Mutex<Option<Stream>>,
}

impl HttpFetcher {
    fn display(&self) -> String {
        redact(&self.url)
    }

    fn fetch_range(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut end = offset + len as u64 - 1;
        if let Some(size) = self.size {
            if offset >= size {
                return Ok(Vec::new());
            }
            end = end.min(size - 1);
        }
        let range = format!("bytes={offset}-{end}");
        let want = (end - offset + 1) as usize;
        self.client.retrying(&self.url, || {
            let resp = self.client.send_once(
                "GET",
                &self.url,
                &[("Range", range.as_str())],
                None,
                false,
            )?;
            let status = resp.status().as_u16();
            let cr = resp
                .headers()
                .get("content-range")
                .and_then(|v| v.to_str().ok())
                .and_then(parse_content_range);
            let after = retry_after(&resp);
            let io_retry = |e: io::Error| {
                Failure::Retry(
                    Error::Http {
                        url: self.display(),
                        message: format!("reading range {range}: {e}"),
                    },
                    None,
                )
            };
            match status {
                206 => {
                    if let Some(ContentRange {
                        range: Some((start, _)),
                        ..
                    }) = cr
                    {
                        if start != offset {
                            return Err(Failure::Fatal(Error::Http {
                                url: self.display(),
                                message: format!(
                                    "asked for {range}, server sent bytes from {start}"
                                ),
                            }));
                        }
                    }
                    let data =
                        read_body(resp.into_body().into_reader(), want as u64).map_err(io_retry)?;
                    let at_eof = self.size.is_some_and(|s| offset + data.len() as u64 >= s);
                    if data.len() < want && !at_eof && self.size.is_some() {
                        return Err(Failure::Retry(
                            Error::Http {
                                url: self.display(),
                                message: format!(
                                    "short range response: {} of {want} bytes",
                                    data.len()
                                ),
                            },
                            None,
                        ));
                    }
                    Ok(data)
                }
                416 => {
                    let _ = read_body(resp.into_body().into_reader(), 64 << 10);
                    Ok(Vec::new())
                }
                200 if offset == 0 => {
                    // Range ignored for this request; the start of the
                    // body is still the data we asked for.
                    read_body(resp.into_body().into_reader(), want as u64).map_err(io_retry)
                }
                200 => Err(Failure::Fatal(Error::Unsupported(format!(
                    "{} stopped honouring range requests",
                    self.display()
                )))),
                s if is_transient_status(s) => Err(Failure::Retry(
                    Error::HttpStatus {
                        status: s,
                        url: self.display(),
                    },
                    after,
                )),
                s => Err(Failure::Fatal(status_error(s, &self.url).unwrap_or(
                    Error::HttpStatus {
                        status: s,
                        url: self.display(),
                    },
                ))),
            }
        })
    }

    fn fetch_streaming(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut guard = lock(&self.stream);
        let max_skip = self.client.opts.max_forward_skip;
        let reopen = match guard.as_ref() {
            Some(s) => offset < s.pos,
            None => true,
        };
        if reopen {
            // Without ranges the only way back is to restart the download
            // from byte 0, which is only reasonable near the start.
            if offset > max_skip {
                return Err(Error::Unsupported(match guard.as_ref() {
                    Some(s) => format!(
                        "{} does not support range requests: cannot seek backward from byte {} to {offset}",
                        self.display(),
                        s.pos
                    ),
                    None => format!(
                        "{} does not support range requests: cannot seek to byte {offset}",
                        self.display()
                    ),
                }));
            }
            log::debug!(
                "{}: no range support, restarting stream to reach byte {offset}",
                self.display()
            );
            let resp = self.client.retrying(&self.url, || {
                let resp = self.client.send_once("GET", &self.url, &[], None, true)?;
                let status = resp.status().as_u16();
                if is_transient_status(status) {
                    return Err(Failure::Retry(
                        Error::HttpStatus {
                            status,
                            url: self.display(),
                        },
                        retry_after(&resp),
                    ));
                }
                if let Some(e) = status_error(status, &self.url) {
                    return Err(Failure::Fatal(e));
                }
                Ok(resp)
            })?;
            *guard = Some(Stream {
                reader: Box::new(resp.into_body().into_reader()),
                pos: 0,
            });
        }
        let Some(stream) = guard.as_mut() else {
            return Err(Error::Unsupported("stream unavailable".into()));
        };
        if offset - stream.pos > max_skip {
            return Err(Error::Unsupported(format!(
                "{} does not support range requests; refusing to skip {} bytes",
                self.display(),
                offset - stream.pos
            )));
        }
        let skip = offset - stream.pos;
        let skipped = io::copy(&mut (&mut stream.reader).take(skip), &mut io::sink());
        let result = skipped.and_then(|n| {
            stream.pos += n;
            if n < skip {
                return Ok(Vec::new());
            }
            let mut out = Vec::with_capacity(len);
            (&mut stream.reader)
                .take(len as u64)
                .read_to_end(&mut out)?;
            stream.pos += out.len() as u64;
            Ok(out)
        });
        result.map_err(|e| {
            // The stream is unusable after an error; a later read reopens it.
            *guard = None;
            Error::Http {
                url: self.display(),
                message: format!("stream read failed: {e}"),
            }
        })
    }
}

impl RangeFetch for HttpFetcher {
    fn fetch(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let r = if self.ranges {
            self.fetch_range(offset, len)
        } else {
            self.fetch_streaming(offset, len)
        };
        r.map_err(io::Error::from)
    }

    fn size(&self) -> Option<u64> {
        self.size
    }

    fn describe(&self) -> String {
        self.display()
    }

    fn sequential_only(&self) -> bool {
        !self.ranges
    }
}

/// A file read over HTTP(S) with Range requests, a block cache and
/// read-ahead. See the [module documentation](self).
pub struct HttpFile {
    src: CachedSource<HttpFetcher>,
}

impl HttpFile {
    /// Opens `url` with one probe request (`Range: bytes=0-<block>`) that
    /// learns the size, whether ranges work, and fills the first block.
    pub fn open(client: &HttpClient, url: &str) -> Result<HttpFile> {
        // Embedded `user:pass@` stays in the URL: `send_once` strips it into
        // an Authorization header on every request.
        let url = url.to_string();
        let bs = client.opts.cache.block_size.max(4096) as u64;
        let range = format!("bytes=0-{}", bs - 1);
        let display = redact(&url);
        enum Probe {
            Ranged(Option<u64>, Vec<u8>),
            Plain(Option<u64>, Response),
        }
        let probe = client.retrying(&url, || {
            let resp = client.send_once("GET", &url, &[("Range", range.as_str())], None, true)?;
            let status = resp.status().as_u16();
            let cr = resp
                .headers()
                .get("content-range")
                .and_then(|v| v.to_str().ok())
                .and_then(parse_content_range);
            match status {
                206 => {
                    let total = cr.and_then(|c| c.total);
                    let want = total.map_or(bs, |t| t.min(bs));
                    let data = read_body(resp.into_body().into_reader(), want).map_err(|e| {
                        Failure::Retry(
                            Error::Http {
                                url: display.clone(),
                                message: e.to_string(),
                            },
                            None,
                        )
                    })?;
                    Ok(Probe::Ranged(total, data))
                }
                416 => {
                    let total = cr.and_then(|c| c.total).unwrap_or(0);
                    let _ = read_body(resp.into_body().into_reader(), 64 << 10);
                    Ok(Probe::Ranged(Some(total), Vec::new()))
                }
                200 => {
                    let len = resp
                        .headers()
                        .get("content-length")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.trim().parse().ok());
                    Ok(Probe::Plain(len, resp))
                }
                s if is_transient_status(s) => Err(Failure::Retry(
                    Error::HttpStatus {
                        status: s,
                        url: display.clone(),
                    },
                    retry_after(&resp),
                )),
                s => Err(Failure::Fatal(status_error(s, &url).unwrap_or(
                    Error::HttpStatus {
                        status: s,
                        url: display.clone(),
                    },
                ))),
            }
        })?;
        let opts = client.opts.cache.clone();
        let file = match probe {
            Probe::Ranged(size, first) => {
                let src = CachedSource::new(
                    HttpFetcher {
                        client: client.clone(),
                        url,
                        size,
                        ranges: true,
                        stream: Mutex::new(None),
                    },
                    &opts,
                );
                if !first.is_empty() {
                    src.seed(0, first);
                }
                HttpFile { src }
            }
            Probe::Plain(size, resp) => {
                log::info!("{display}: server ignores range requests; streaming sequentially");
                let mut reader: Box<dyn Read + Send> = Box::new(resp.into_body().into_reader());
                let mut first = Vec::new();
                (&mut reader)
                    .take(bs)
                    .read_to_end(&mut first)
                    .map_err(|e| Error::Http {
                        url: display.clone(),
                        message: e.to_string(),
                    })?;
                // A short first block means we already have the whole file.
                let size = if (first.len() as u64) < bs {
                    Some(first.len() as u64)
                } else {
                    size
                };
                let pos = first.len() as u64;
                let src = CachedSource::new(
                    HttpFetcher {
                        client: client.clone(),
                        url,
                        size,
                        ranges: false,
                        stream: Mutex::new(Some(Stream { reader, pos })),
                    },
                    &opts,
                );
                if !first.is_empty() {
                    src.seed(0, first);
                }
                HttpFile { src }
            }
        };
        Ok(file)
    }

    /// True when the server honours Range requests (random access works).
    pub fn supports_ranges(&self) -> bool {
        self.src.fetcher().ranges
    }

    /// Cache counters for this file.
    pub fn stats(&self) -> CacheStats {
        self.src.stats()
    }

    /// Bytes per cache block (and per range request).
    pub fn block_size(&self) -> u64 {
        self.src.block_size()
    }
}

impl ByteSource for HttpFile {
    fn size(&self) -> Option<u64> {
        self.src.size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        self.src.read_at(offset, buf)
    }

    fn describe(&self) -> String {
        self.src.describe()
    }
}

impl std::fmt::Debug for HttpFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpFile")
            .field("url", &self.src.describe())
            .field("size", &self.src.size())
            .field("ranges", &self.supports_ranges())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range_parsing() {
        assert_eq!(
            parse_content_range("bytes 0-1023/4096"),
            Some(ContentRange {
                range: Some((0, 1023)),
                total: Some(4096)
            })
        );
        assert_eq!(
            parse_content_range("bytes 10-19/*"),
            Some(ContentRange {
                range: Some((10, 19)),
                total: None
            })
        );
        assert_eq!(
            parse_content_range("bytes */77"),
            Some(ContentRange {
                range: None,
                total: Some(77)
            })
        );
        assert_eq!(parse_content_range("bytes 5-1/10"), None);
        assert_eq!(parse_content_range("items 0-1/2"), None);
    }

    #[test]
    fn statuses() {
        assert!(status_error(204, "u").is_none());
        assert!(matches!(status_error(401, "u"), Some(Error::Auth(_))));
        assert!(matches!(status_error(404, "u"), Some(Error::NotFound(_))));
        assert!(matches!(
            status_error(500, "u"),
            Some(Error::HttpStatus { status: 500, .. })
        ));
        assert!(is_transient_status(503));
        assert!(!is_transient_status(404));
    }

    #[test]
    fn debug_hides_auth() {
        let c = HttpClient::new(
            HttpOptions::default(),
            Some(&Credentials::new("bob", "hunter2")),
        );
        let s = format!("{c:?}");
        assert!(!s.contains("hunter2") && !s.contains("Ym9i"), "{s}");
    }
}
