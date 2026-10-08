//! A tiny local HTTP server for hermetic source tests.

#![allow(dead_code)]

use std::io::Cursor;
use std::sync::{Arc, Mutex};
use tiny_http::{Header, Response, Server};

/// A request as the handler sees it (body read up front).
#[derive(Clone, Debug)]
pub struct Req {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Req {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Path without the query string.
    pub fn path(&self) -> &str {
        self.url.split('?').next().unwrap_or("")
    }
}

pub type Resp = Response<Cursor<Vec<u8>>>;

/// A response with a status, body and headers.
pub fn resp(status: u16, body: impl Into<Vec<u8>>, headers: &[(&str, &str)]) -> Resp {
    // Always send Content-Length, as file servers do.
    let mut r = Response::from_data(body.into())
        .with_status_code(status)
        .with_chunked_threshold(usize::MAX);
    for (k, v) in headers {
        r.add_header(Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap());
    }
    r
}

/// Serves `data`, honouring `Range: bytes=a-b` / `bytes=a-` when `ranges`.
pub fn serve_bytes(req: &Req, data: &[u8], ranges: bool) -> Resp {
    let range = req
        .header("range")
        .and_then(|r| r.strip_prefix("bytes="))
        .filter(|_| ranges);
    let Some(range) = range else {
        let mut headers = vec![("Content-Type", "video/mp4")];
        if ranges {
            headers.push(("Accept-Ranges", "bytes"));
        }
        return resp(200, data.to_vec(), &headers);
    };
    let (a, b) = range.split_once('-').unwrap();
    let len = data.len() as u64;
    let start: u64 = a.parse().unwrap();
    let end: u64 = if b.is_empty() {
        len - 1
    } else {
        b.parse::<u64>().unwrap().min(len - 1)
    };
    if start >= len {
        return resp(
            416,
            Vec::new(),
            &[("Content-Range", &format!("bytes */{len}"))],
        );
    }
    resp(
        206,
        data[start as usize..=end as usize].to_vec(),
        &[
            ("Content-Type", "video/mp4"),
            ("Content-Range", &format!("bytes {start}-{end}/{len}")),
        ],
    )
}

type Handler = dyn Fn(&Req) -> Resp + Send + Sync;

/// A server on `127.0.0.1:<random port>` answering every request with
/// `handler` on its own thread, and logging requests.
pub struct TestServer {
    server: Arc<Server>,
    pub base: String,
    log: Arc<Mutex<Vec<Req>>>,
}

impl TestServer {
    pub fn start(handler: impl Fn(&Req) -> Resp + Send + Sync + 'static) -> TestServer {
        let server = Arc::new(Server::http("127.0.0.1:0").unwrap());
        let port = server.server_addr().to_ip().unwrap().port();
        let log: Arc<Mutex<Vec<Req>>> = Arc::default();
        let handler: Arc<Handler> = Arc::new(handler);
        let (s, l) = (server.clone(), log.clone());
        std::thread::spawn(move || {
            for mut request in s.incoming_requests() {
                let mut body = String::new();
                let _ = request.as_reader().read_to_string(&mut body);
                let req = Req {
                    method: request.method().as_str().to_string(),
                    url: request.url().to_string(),
                    headers: request
                        .headers()
                        .iter()
                        .map(|h| {
                            (
                                h.field.as_str().as_str().to_string(),
                                h.value.as_str().to_string(),
                            )
                        })
                        .collect(),
                    body,
                };
                l.lock().unwrap().push(req.clone());
                let handler = handler.clone();
                std::thread::spawn(move || {
                    let _ = request.respond(handler(&req));
                });
            }
        });
        TestServer {
            server,
            base: format!("http://127.0.0.1:{port}"),
            log,
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    pub fn requests(&self) -> Vec<Req> {
        self.log.lock().unwrap().clone()
    }

    pub fn count(&self, pred: impl Fn(&Req) -> bool) -> usize {
        self.log.lock().unwrap().iter().filter(|r| pred(r)).count()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.server.unblock();
    }
}

/// Deterministic test data.
pub fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 31 % 253) as u8).collect()
}
