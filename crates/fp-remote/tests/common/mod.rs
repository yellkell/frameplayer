//! Helpers shared by the socket-level tests: a hub on ephemeral loopback
//! ports, a raw DeoVR client and a minimal HTTP/1.1 client.

#![allow(dead_code)]

use fp_core::PlaybackStatus;
use fp_remote::{RemoteConfig, RemoteEvent, RemoteHub, RemoteItem, RemoteLibrary};
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub fn hub() -> RemoteHub {
    RemoteHub::new(RemoteConfig {
        bind_address: IpAddr::V4(Ipv4Addr::LOCALHOST),
        deovr_port: 0,
        web_port: 0,
        ..RemoteConfig::default()
    })
    .unwrap()
}

pub fn playing(location: &str, position: f64) -> PlaybackStatus {
    PlaybackStatus {
        location: location.into(),
        title: "Test video".into(),
        duration: 600.0,
        position,
        speed: 1.0,
        playing: true,
        sampled_at_ms: fp_core::playback::now_ms(),
    }
}

/// Waits until `f` holds or `timeout` passes.
pub fn wait_for(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + timeout;
    while Instant::now() < end {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    f()
}

pub fn next_event(hub: &RemoteHub) -> RemoteEvent {
    hub.events()
        .recv_timeout(Duration::from_secs(3))
        .expect("event")
}

// ---- DeoVR client ----

pub struct Deovr {
    pub stream: TcpStream,
}

impl Deovr {
    pub fn connect(addr: SocketAddr) -> Deovr {
        let stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(4)))
            .unwrap();
        Deovr { stream }
    }

    pub fn send(&mut self, payload: &[u8]) {
        let mut f = (payload.len() as u32).to_le_bytes().to_vec();
        f.extend_from_slice(payload);
        self.stream.write_all(&f).unwrap();
    }

    /// Next frame; `None` when the server closed the connection.
    pub fn frame(&mut self) -> Option<Vec<u8>> {
        let mut len = [0u8; 4];
        match self.stream.read_exact(&mut len) {
            Ok(()) => {}
            Err(e) if is_closed(&e) => return None,
            Err(e) => panic!("read: {e}"),
        }
        let mut buf = vec![0u8; u32::from_le_bytes(len) as usize];
        match self.stream.read_exact(&mut buf) {
            Ok(()) => Some(buf),
            Err(e) if is_closed(&e) => None,
            Err(e) => panic!("read: {e}"),
        }
    }

    /// Next non-empty frame parsed as JSON, skipping keep-alives.
    pub fn status(&mut self) -> serde_json::Value {
        loop {
            let f = self.frame().expect("connection open");
            if !f.is_empty() {
                return serde_json::from_slice(&f).unwrap();
            }
        }
    }

    /// Skips status messages until one satisfies `f` (panics after 4 s).
    pub fn status_where(&mut self, f: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
        let end = Instant::now() + Duration::from_secs(4);
        while Instant::now() < end {
            let s = self.status();
            if f(&s) {
                return s;
            }
        }
        panic!("no matching status");
    }

    /// Reads until the server closes the connection; panics on timeout.
    pub fn expect_closed(&mut self) {
        let end = Instant::now() + Duration::from_secs(4);
        while Instant::now() < end {
            if self.frame().is_none() {
                return;
            }
        }
        panic!("server did not close the connection");
    }
}

fn is_closed(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::UnexpectedEof | ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted
    )
}

// ---- HTTP client ----

#[derive(Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap()
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

pub fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> HttpResponse {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    parse_response(&raw)
}

pub fn parse_response(raw: &[u8]) -> HttpResponse {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("header end");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|c| c.parse().ok())
        .expect("status line");
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect();
    HttpResponse {
        status,
        headers,
        body: raw[split + 4..].to_vec(),
    }
}

// ---- Fake library ----

pub struct FakeLibrary {
    pub items: Vec<RemoteItem>,
    pub thumb: PathBuf,
}

impl FakeLibrary {
    pub fn new() -> FakeLibrary {
        let dir = std::env::temp_dir().join(format!("fp-remote-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // One file per instance: tests run in parallel in one process, and
        // rewriting a shared file truncates it under a concurrent reader.
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let thumb = dir.join(format!("thumb-1-{n}.jpg"));
        std::fs::write(&thumb, JPEG).unwrap();
        let item = |id: i64, title: &str, thumb: bool| RemoteItem {
            id,
            title: title.into(),
            location: format!("/media/{id}.mp4"),
            duration: Some(60.0 * id as f64),
            format_label: "180° Side by side".into(),
            has_thumbnail: thumb,
        };
        FakeLibrary {
            items: vec![
                item(1, "Beach sunset", true),
                item(2, "Mountain hike", false),
                item(3, "Beach volleyball <script>alert(1)</script>", false),
                item(4, "City walk", false),
            ],
            thumb,
        }
    }
}

/// Smallest plausible JPEG prefix: the bytes only need to round-trip.
pub const JPEG: &[u8] = b"\xff\xd8\xff\xe0\x00\x10JFIF\x00test-thumbnail\xff\xd9";

impl RemoteLibrary for FakeLibrary {
    fn search(&self, query: &str, limit: usize, offset: usize) -> Vec<RemoteItem> {
        let q = query.to_lowercase();
        self.items
            .iter()
            .filter(|i| i.title.to_lowercase().contains(&q))
            .skip(offset)
            .take(limit)
            .cloned()
            .collect()
    }

    fn thumbnail_path(&self, id: i64) -> Option<PathBuf> {
        (id == 1).then(|| self.thumb.clone())
    }
}
