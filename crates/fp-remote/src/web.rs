//! The LAN web remote: an embedded single-page app plus a small JSON API.
//!
//! Endpoints (all but the static assets need the pairing token, given once
//! as `?token=` and then carried by an `HttpOnly; SameSite=Strict` cookie,
//! or as `Authorization: Bearer <token>`):
//!
//! | Method | Path | |
//! |---|---|---|
//! | GET | `/` | the app (`/?token=` sets the cookie and redirects to `/`) |
//! | GET | `/api/status` | [`PlaybackStatus`] JSON plus `now_ms` (server clock) |
//! | POST | `/api/command` | [`fp_core::PlayerCommand`] JSON |
//! | GET | `/api/library?q=&limit=&offset=` | `{"items": [RemoteItem...]}` |
//! | GET | `/api/thumb/<id>` | thumbnail bytes |
//! | POST | `/api/text` | `{"text": "..."}` typed into the headset |
//! | GET | `/api/events` | server-sent events, one status JSON per `data:` |

use crate::RemoteError;
use crate::hub::{RemoteEvent, RemoteLibrary, Shared};
use crate::net::is_allowed_peer;
use crate::status::{ChangeTracker, command_is_valid};
use crate::token::constant_time_eq;
use fp_core::playback::now_ms;
use fp_core::{PlaybackStatus, PlayerCommand};
use serde::{Deserialize, Serialize};
use std::io::{Cursor, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tiny_http::{Header, Method, Request, Response, Server};

const INDEX_HTML: &str = include_str!("web/index.html");
const APP_JS: &str = include_str!("web/app.js");
const APP_CSS: &str = include_str!("web/app.css");
const PAIR_HTML: &str = include_str!("web/pair.html");

/// Name of the cookie carrying the pairing token.
pub const COOKIE_NAME: &str = "fp_remote_token";

/// Request-handling threads. Live-status streams get their own threads.
const WORKERS: usize = 4;
const RECV_POLL: Duration = Duration::from_millis(200);
const MAX_BODY: usize = 64 * 1024;
const MAX_TEXT_CHARS: usize = 4096;
const MAX_QUERY_CHARS: usize = 256;
const MAX_THUMB_BYTES: u64 = 16 * 1024 * 1024;
const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 200;
const MAX_OFFSET: usize = 1_000_000;
/// Event-stream polling tick, refresh interval while playing, heartbeat.
const STREAM_TICK: Duration = Duration::from_millis(100);
const STREAM_REFRESH: Duration = Duration::from_secs(2);
const STREAM_HEARTBEAT: Duration = Duration::from_secs(15);

const SECURITY_HEADERS: &[(&str, &str)] = &[
    (
        "Content-Security-Policy",
        "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; \
         connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
    ),
    ("X-Content-Type-Options", "nosniff"),
    ("X-Frame-Options", "DENY"),
    ("Referrer-Policy", "no-referrer"),
];

struct Ctx {
    shared: Arc<Shared>,
    lib: Arc<dyn RemoteLibrary>,
    stop: Arc<AtomicBool>,
    streams: Arc<AtomicUsize>,
    max_streams: usize,
}

/// A running web remote.
pub(crate) struct WebServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    streams: Arc<AtomicUsize>,
    server: Option<Arc<Server>>,
    threads: Vec<JoinHandle<()>>,
}

impl WebServer {
    pub(crate) fn start(
        bind: SocketAddr,
        shared: Arc<Shared>,
        lib: Arc<dyn RemoteLibrary>,
        max_streams: usize,
    ) -> Result<WebServer, RemoteError> {
        let listener =
            TcpListener::bind(bind).map_err(|source| RemoteError::Bind { addr: bind, source })?;
        let addr = listener.local_addr()?;
        let server = Arc::new(
            Server::from_listener(listener, None).map_err(|e| RemoteError::Http(e.to_string()))?,
        );
        let stop = Arc::new(AtomicBool::new(false));
        let streams = Arc::new(AtomicUsize::new(0));
        let ctx = Arc::new(Ctx {
            shared,
            lib,
            stop: stop.clone(),
            streams: streams.clone(),
            max_streams,
        });
        let mut web = WebServer {
            addr,
            stop,
            streams,
            server: Some(server.clone()),
            threads: Vec::new(),
        };
        for i in 0..WORKERS {
            let (server, ctx) = (server.clone(), ctx.clone());
            let spawned = thread::Builder::new()
                .name(format!("web-remote-{i}"))
                .spawn(move || worker(&server, &ctx));
            match spawned {
                Ok(h) => web.threads.push(h),
                Err(e) => {
                    web.stop();
                    return Err(e.into());
                }
            }
        }
        log::info!("web remote listening on {addr}");
        Ok(web)
    }

    pub(crate) fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub(crate) fn client_count(&self) -> usize {
        self.streams.load(Ordering::Acquire)
    }

    pub(crate) fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(server) = &self.server {
            for _ in 0..WORKERS {
                server.unblock();
            }
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        // Last reference: tiny_http closes the listener.
        self.server = None;
    }
}

impl Drop for WebServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn worker(server: &Server, ctx: &Arc<Ctx>) {
    while !ctx.stop.load(Ordering::Acquire) {
        match server.recv_timeout(RECV_POLL) {
            Ok(Some(rq)) => {
                // A panicking library implementation must not take the
                // worker down with it; the dropped request gets a 500.
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle(rq, ctx)));
                if r.is_err() {
                    log::error!("web remote: request handler panicked");
                }
            }
            Ok(None) => {}
            Err(e) => {
                log::error!("web remote: server failed: {e}");
                break;
            }
        }
    }
}

/// A buffered response before it becomes a tiny_http one.
struct Reply {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
    cache: &'static str,
    headers: Vec<(&'static str, String)>,
}

impl Reply {
    fn new(status: u16, content_type: &'static str, body: impl Into<Vec<u8>>) -> Reply {
        Reply {
            status,
            content_type,
            body: body.into(),
            cache: "no-store",
            headers: Vec::new(),
        }
    }

    fn json<T: Serialize>(value: &T) -> Reply {
        match serde_json::to_vec(value) {
            Ok(body) => Reply::new(200, "application/json", body),
            Err(e) => Reply::error(500, &e.to_string()),
        }
    }

    fn error(status: u16, message: &str) -> Reply {
        let body = serde_json::json!({ "error": message }).to_string();
        Reply::new(status, "application/json", body)
    }

    fn header(mut self, name: &'static str, value: impl Into<String>) -> Reply {
        self.headers.push((name, value.into()));
        self
    }

    fn into_response(self) -> Response<Cursor<Vec<u8>>> {
        let mut r = Response::from_data(self.body).with_status_code(self.status);
        let all = SECURITY_HEADERS
            .iter()
            .map(|(n, v)| (*n, (*v).to_owned()))
            .chain([
                ("Content-Type", self.content_type.to_owned()),
                ("Cache-Control", self.cache.to_owned()),
            ])
            .chain(self.headers);
        for (n, v) in all {
            // Only fails on non-ASCII; every value here is ASCII.
            if let Ok(h) = Header::from_bytes(n.as_bytes(), v.as_bytes()) {
                r.add_header(h);
            }
        }
        r
    }
}

fn respond(rq: Request, reply: Reply) {
    if let Err(e) = rq.respond(reply.into_response()) {
        log::debug!("web remote: response not delivered: {e}");
    }
}

/// How (and whether) a request proved it knows the token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Auth {
    /// No valid token.
    None,
    /// Valid token in `?token=`; the response should set the cookie.
    Query,
    /// Valid cookie or bearer header.
    Carried,
}

/// Checks the query parameter, cookie and bearer header against `token`,
/// each in constant time.
pub(crate) fn authenticate(
    token: &str,
    query_token: Option<&str>,
    cookie_header: Option<&str>,
    authorization: Option<&str>,
) -> Auth {
    let ok = |t: &str| !token.is_empty() && constant_time_eq(t.as_bytes(), token.as_bytes());
    if query_token.is_some_and(ok) {
        return Auth::Query;
    }
    let cookie_ok = cookie_header
        .and_then(|h| cookie_value(h, COOKIE_NAME))
        .is_some_and(ok);
    let bearer_ok = authorization
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(str::trim)
        .is_some_and(ok);
    if cookie_ok || bearer_ok {
        Auth::Carried
    } else {
        Auth::None
    }
}

fn set_cookie(token: &str) -> String {
    format!("{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age=31536000")
}

/// Value of cookie `name` in a `Cookie:` header.
pub(crate) fn cookie_value<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header
        .split(';')
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.trim())
}

/// Decodes `%XX` escapes and `+` (as space) in a query component.
pub(crate) fn percent_decode(s: &str) -> String {
    fn hex(b: u8) -> Option<u8> {
        (b as char).to_digit(16).map(|d| d as u8)
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => match (hex(b[i + 1]), hex(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h << 4 | l);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Splits `/path?query` and decodes the query into pairs.
pub(crate) fn parse_url(url: &str) -> (&str, Vec<(String, String)>) {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let pairs = query
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect();
    (path, pairs)
}

fn param<'a>(q: &'a [(String, String)], name: &str) -> Option<&'a str> {
    q.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

fn header_value<'a>(rq: &'a Request, name: &'static str) -> Option<&'a str> {
    rq.headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str())
}

fn handle(mut rq: Request, ctx: &Arc<Ctx>) {
    if !rq.remote_addr().is_some_and(|a| is_allowed_peer(a.ip())) {
        log::warn!("web remote: refusing non-LAN peer {:?}", rq.remote_addr());
        return respond(rq, Reply::error(403, "forbidden"));
    }
    let method = rq.method().clone();
    let url = rq.url().to_owned();
    let (path, query) = parse_url(&url);

    // Static code and styles carry no data; the unpaired page needs them.
    match (&method, path) {
        (Method::Get, "/app.js") => {
            return respond(rq, cached(Reply::new(200, "text/javascript", APP_JS)));
        }
        (Method::Get, "/app.css") => {
            return respond(rq, cached(Reply::new(200, "text/css", APP_CSS)));
        }
        _ => {}
    }

    let token = ctx.shared.token();
    let auth = authenticate(
        &token,
        param(&query, "token"),
        header_value(&rq, "Cookie"),
        header_value(&rq, "Authorization"),
    );
    if auth == Auth::None {
        let reply = if method == Method::Get && path == "/" {
            Reply::new(401, "text/html; charset=utf-8", PAIR_HTML)
        } else {
            Reply::error(401, "not paired")
        };
        return respond(rq, reply);
    }
    let cookie = (auth == Auth::Query).then(|| set_cookie(&token));

    if method == Method::Get && path == "/api/events" {
        return start_event_stream(rq, ctx, cookie);
    }
    let reply = match (&method, path) {
        // The token leaves the address bar (and history) right away.
        (Method::Get, "/" | "/index.html") if auth == Auth::Query => {
            Reply::new(303, "text/plain", "").header("Location", "/")
        }
        (Method::Get, "/" | "/index.html") => {
            Reply::new(200, "text/html; charset=utf-8", INDEX_HTML)
        }
        (Method::Get, "/api/status") => Reply::json(&WebStatus::now(&ctx.shared.status.snapshot())),
        (Method::Post, "/api/command") => match read_body(&mut rq) {
            Ok(body) => post_command(&body, ctx),
            Err(r) => r,
        },
        (Method::Post, "/api/text") => match read_body(&mut rq) {
            Ok(body) => post_text(&body, ctx),
            Err(r) => r,
        },
        (Method::Get, "/api/library") => library(&query, ctx),
        (Method::Get, p) if p.starts_with("/api/thumb/") => {
            thumbnail(&p["/api/thumb/".len()..], ctx)
        }
        (_, "/" | "/index.html" | "/api/status" | "/api/library" | "/api/events") => {
            Reply::error(405, "method not allowed").header("Allow", "GET")
        }
        (_, "/api/command" | "/api/text") => {
            Reply::error(405, "method not allowed").header("Allow", "POST")
        }
        _ => Reply::error(404, "not found"),
    };
    let reply = match cookie {
        Some(c) => reply.header("Set-Cookie", c),
        None => reply,
    };
    respond(rq, reply);
}

fn cached(mut r: Reply) -> Reply {
    // Short cache: assets change with app updates.
    r.cache = "private, max-age=300";
    r
}

fn read_body(rq: &mut Request) -> Result<Vec<u8>, Reply> {
    if rq.body_length().is_some_and(|n| n > MAX_BODY) {
        return Err(Reply::error(413, "body too large"));
    }
    let mut body = Vec::new();
    match rq
        .as_reader()
        .take(MAX_BODY as u64 + 1)
        .read_to_end(&mut body)
    {
        Ok(_) if body.len() > MAX_BODY => Err(Reply::error(413, "body too large")),
        Ok(_) => Ok(body),
        Err(e) => Err(Reply::error(400, &e.to_string())),
    }
}

/// Status as the web API reports it: [`PlaybackStatus`] fields plus the
/// server's clock, so the page can extrapolate the position correctly even
/// when the phone's clock differs from the headset's.
#[derive(Serialize)]
struct WebStatus<'a> {
    #[serde(flatten)]
    status: &'a PlaybackStatus,
    now_ms: u64,
}

impl WebStatus<'_> {
    fn now(status: &PlaybackStatus) -> WebStatus<'_> {
        WebStatus {
            status,
            now_ms: now_ms(),
        }
    }
}

fn post_command(body: &[u8], ctx: &Ctx) -> Reply {
    let cmd: PlayerCommand = match serde_json::from_slice(body) {
        Ok(c) => c,
        Err(e) => return Reply::error(400, &format!("invalid command: {e}")),
    };
    if !command_is_valid(&cmd) {
        return Reply::error(400, "invalid command value");
    }
    ctx.shared.emit(RemoteEvent::Command(cmd));
    Reply::json(&serde_json::json!({ "ok": true }))
}

#[derive(Deserialize)]
struct TextBody {
    text: String,
}

fn post_text(body: &[u8], ctx: &Ctx) -> Reply {
    let t: TextBody = match serde_json::from_slice(body) {
        Ok(t) => t,
        Err(e) => return Reply::error(400, &format!("invalid body: {e}")),
    };
    if t.text.chars().count() > MAX_TEXT_CHARS {
        return Reply::error(413, "text too long");
    }
    // Control characters have no business in a search box or URL field.
    let text: String = t.text.chars().filter(|c| !c.is_control()).collect();
    ctx.shared.emit(RemoteEvent::Text(text));
    Reply::json(&serde_json::json!({ "ok": true }))
}

fn library(query: &[(String, String)], ctx: &Ctx) -> Reply {
    let q: String = param(query, "q")
        .unwrap_or("")
        .chars()
        .take(MAX_QUERY_CHARS)
        .collect();
    let num = |name, default: usize, max: usize| {
        param(query, name)
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(default)
            .min(max)
    };
    let limit = num("limit", DEFAULT_LIMIT, MAX_LIMIT).max(1);
    let offset = num("offset", 0, MAX_OFFSET);
    let mut items = ctx.lib.search(q.trim(), limit, offset);
    items.truncate(limit);
    Reply::json(&serde_json::json!({ "items": items, "offset": offset, "limit": limit }))
}

fn thumbnail(id: &str, ctx: &Ctx) -> Reply {
    let Ok(id) = id.parse::<i64>() else {
        return Reply::error(404, "not found");
    };
    let Some(path) = ctx.lib.thumbnail_path(id) else {
        return Reply::error(404, "no thumbnail");
    };
    let data = std::fs::File::open(&path).and_then(|f| {
        let mut buf = Vec::new();
        f.take(MAX_THUMB_BYTES + 1).read_to_end(&mut buf)?;
        Ok(buf)
    });
    match data {
        Ok(buf) if buf.len() as u64 > MAX_THUMB_BYTES => Reply::error(500, "thumbnail too large"),
        Ok(buf) => {
            let mut r = Reply::new(200, image_type(&buf), buf);
            r.cache = "private, max-age=3600";
            r
        }
        Err(e) => {
            log::warn!("web remote: thumbnail {}: {e}", path.display());
            Reply::error(404, "no thumbnail")
        }
    }
}

/// JPEG unless the bytes say PNG or WebP.
fn image_type(b: &[u8]) -> &'static str {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        "image/webp"
    } else {
        "image/jpeg"
    }
}

/// Decrements the live-stream count when a stream ends, however it ends.
struct StreamGuard(Arc<AtomicUsize>);

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn start_event_stream(rq: Request, ctx: &Arc<Ctx>, cookie: Option<String>) {
    if ctx.streams.fetch_add(1, Ordering::AcqRel) >= ctx.max_streams {
        ctx.streams.fetch_sub(1, Ordering::AcqRel);
        return respond(rq, Reply::error(503, "too many live connections"));
    }
    let guard = StreamGuard(ctx.streams.clone());
    let ctx2 = ctx.clone();
    let spawned = thread::Builder::new()
        .name("web-remote-events".into())
        .spawn(move || {
            let _guard = guard;
            let mut w = rq.into_writer();
            if let Err(e) = event_stream(&mut w, &ctx2, cookie.as_deref()) {
                log::debug!("web remote: event stream ended: {e}");
            }
        });
    // On failure the closure, request and guard are dropped: tiny_http
    // answers 500 and the count is restored.
    if let Err(e) = spawned {
        log::warn!("web remote: cannot start event stream: {e}");
    }
}

fn event_stream(w: &mut dyn Write, ctx: &Ctx, cookie: Option<&str>) -> std::io::Result<()> {
    let mut head = String::from(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\n\
         Cache-Control: no-store\r\nConnection: close\r\nX-Accel-Buffering: no\r\n",
    );
    for (n, v) in SECURITY_HEADERS {
        head.push_str(&format!("{n}: {v}\r\n"));
    }
    if let Some(c) = cookie {
        head.push_str(&format!("Set-Cookie: {c}\r\n"));
    }
    head.push_str("\r\nretry: 2000\n\n");
    w.write_all(head.as_bytes())?;
    w.flush()?;

    let mut tracker = ChangeTracker::new(STREAM_REFRESH);
    let mut last_write = Instant::now();
    while !ctx.stop.load(Ordering::Acquire) {
        let now = now_ms();
        if let Some(snap) = tracker.poll(&ctx.shared.status, now) {
            let json =
                serde_json::to_string(&WebStatus::now(&snap)).map_err(std::io::Error::other)?;
            w.write_all(format!("data: {json}\n\n").as_bytes())?;
            w.flush()?;
            last_write = Instant::now();
        } else if last_write.elapsed() >= STREAM_HEARTBEAT {
            w.write_all(b": ping\n\n")?;
            w.flush()?;
            last_write = Instant::now();
        }
        thread::sleep(STREAM_TICK);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_queries() {
        let (p, q) = parse_url("/api/library?q=a+b%2Fc%C3%A9&limit=5&x&bad=%zz%4");
        assert_eq!(p, "/api/library");
        assert_eq!(param(&q, "q"), Some("a b/cé"));
        assert_eq!(param(&q, "limit"), Some("5"));
        assert_eq!(param(&q, "x"), Some(""));
        assert_eq!(param(&q, "bad"), Some("%zz%4"));
        assert_eq!(parse_url("/").1, vec![]);
        assert_eq!(percent_decode("%"), "%");
        assert_eq!(percent_decode("%4"), "%4");
        assert_eq!(percent_decode("%41"), "A");
    }

    #[test]
    fn cookies_and_auth() {
        assert_eq!(
            cookie_value("a=1; fp_remote_token=xyz ; b=2", COOKIE_NAME),
            Some("xyz")
        );
        assert_eq!(cookie_value("a=1", COOKIE_NAME), None);
        let t = "abc123";
        assert_eq!(authenticate(t, Some(t), None, None), Auth::Query);
        assert_eq!(
            authenticate(t, Some("nope"), Some("fp_remote_token=abc123"), None),
            Auth::Carried
        );
        assert_eq!(
            authenticate(t, None, None, Some("Bearer abc123")),
            Auth::Carried
        );
        assert_eq!(
            authenticate(t, None, Some("fp_remote_token=abc12"), None),
            Auth::None
        );
        assert_eq!(
            authenticate(t, None, None, Some("Basic abc123")),
            Auth::None
        );
        assert_eq!(authenticate("", Some(""), None, None), Auth::None);
    }

    #[test]
    fn sniffs_image_types() {
        assert_eq!(image_type(b"\xff\xd8\xff\xe0"), "image/jpeg");
        assert_eq!(image_type(b"\x89PNG\r\n\x1a\n...."), "image/png");
        assert_eq!(image_type(b"RIFF\0\0\0\0WEBPVP8 "), "image/webp");
    }

    #[test]
    fn page_never_uses_html_injection_sinks() {
        for sink in [
            "innerHTML",
            "outerHTML",
            "insertAdjacentHTML",
            "document.write",
            "eval(",
        ] {
            assert!(!APP_JS.contains(sink), "app.js uses {sink}");
            assert!(!INDEX_HTML.contains(sink));
        }
        // No external resources: CSP would block them anyway.
        for page in [INDEX_HTML, APP_JS, APP_CSS, PAIR_HTML] {
            assert!(!page.contains("http://") && !page.contains("https://"));
        }
    }
}
