//! Wi-Fi upload: a page any computer or phone on the LAN opens in a browser
//! to drop videos into a folder on the headset (the Quest's Movies), with
//! no cable and nothing installed on the computer.
//!
//! It runs only while the app shows its address and code, and every API
//! call carries that two-digit code (`X-Upload-Code`). A file is written
//! hidden as `.partial-<name>` (library scans skip hidden files; the name
//! keeps its extension, which Android's shared storage requires) and
//! renamed once all of it has arrived. The page also lists the videos in
//! the folder and can delete them (only files right in the folder, and on
//! Android only those FramePlayer wrote: the system refuses the rest).
//!
//! | Method | Path | |
//! |---|---|---|
//! | GET | `/` | the page |
//! | GET | `/upload.js`, `/upload.css` | its script and style |
//! | GET | `/api/space` | `{"free": bytes, "folder": name}` |
//! | PUT | `/api/upload?name=` | the file as the body; `{"saved": name}` |
//! | GET | `/api/files` | `{"files": [{"name", "size", "modified"}...]}` |
//! | DELETE | `/api/files?name=` | `{"deleted": name}` |

use crate::RemoteError;
use crate::net::is_allowed_peer;
use crate::token::constant_time_eq;
use crate::web::parse_url;
use fp_core::source::{is_subtitle_name, is_video_name};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tiny_http::{Header, Method, Request, Response, Server};

const UPLOAD_HTML: &str = include_str!("web/upload.html");
const UPLOAD_JS: &str = include_str!("web/upload.js");
const UPLOAD_CSS: &str = include_str!("web/upload.css");

/// Port tried first; any free one if it is taken.
pub const DEFAULT_UPLOAD_PORT: u16 = 8791;
/// Uploads at once (the page sends one at a time; more tabs may not).
const WORKERS: usize = 3;
const RECV_POLL: Duration = Duration::from_millis(200);
/// Space left free on the headset after an upload.
const RESERVE: u64 = 512 << 20;
/// Wrong codes before the server answers no one: a new code means turning
/// the upload off and on in the headset. The code is short (easy to type),
/// so this stays low: a guesser gets 5 of 100.
const MAX_FAILURES: u32 = 5;
const MAX_NAME_BYTES: usize = 180;
const CHUNK: usize = 1 << 20;

const SECURITY_HEADERS: &[(&str, &str)] = &[
    (
        "Content-Security-Policy",
        "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; \
         base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
    ),
    ("X-Content-Type-Options", "nosniff"),
    ("X-Frame-Options", "DENY"),
    ("Referrer-Policy", "no-referrer"),
    ("Cache-Control", "no-store"),
];

/// What has arrived since the server started.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UploadProgress {
    /// The file arriving now: name, bytes so far, total.
    pub current: Option<(String, u64, u64)>,
    /// Files saved, oldest first.
    pub saved: Vec<String>,
    /// Files deleted from the page, oldest first.
    pub deleted: Vec<String>,
    /// The last upload that failed, and why.
    pub failed: Option<(String, String)>,
}

struct State {
    dir: PathBuf,
    code: String,
    failures: AtomicU32,
    progress: Mutex<UploadProgress>,
    stop: AtomicBool,
}

impl State {
    fn update(&self, f: impl FnOnce(&mut UploadProgress)) {
        if let Ok(mut p) = self.progress.lock() {
            f(&mut p);
        }
    }
}

/// A running upload page.
pub struct UploadServer {
    addr: SocketAddr,
    state: Arc<State>,
    server: Option<Arc<Server>>,
    threads: Vec<JoinHandle<()>>,
}

impl UploadServer {
    /// Starts serving on `bind` (or any free port there), saving into `dir`,
    /// with a fresh code.
    pub fn start(bind: SocketAddr, dir: PathBuf) -> Result<UploadServer, RemoteError> {
        fs::create_dir_all(&dir)?;
        let listener = TcpListener::bind(bind)
            .or_else(|_| TcpListener::bind(SocketAddr::new(bind.ip(), 0)))
            .map_err(|source| RemoteError::Bind { addr: bind, source })?;
        let addr = listener.local_addr()?;
        let server = Arc::new(
            Server::from_listener(listener, None).map_err(|e| RemoteError::Http(e.to_string()))?,
        );
        let state = Arc::new(State {
            dir,
            code: new_code()?,
            failures: AtomicU32::new(0),
            progress: Mutex::new(UploadProgress::default()),
            stop: AtomicBool::new(false),
        });
        let mut up = UploadServer {
            addr,
            state: state.clone(),
            server: Some(server.clone()),
            threads: Vec::new(),
        };
        for i in 0..WORKERS {
            let (server, state) = (server.clone(), state.clone());
            let spawned = thread::Builder::new()
                .name(format!("upload-{i}"))
                .spawn(move || worker(&server, &state));
            match spawned {
                Ok(h) => up.threads.push(h),
                Err(e) => {
                    up.stop();
                    return Err(e.into());
                }
            }
        }
        log::info!(
            "Wi-Fi upload on {addr}, saving to {}",
            up.state.dir.display()
        );
        Ok(up)
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The code the page asks for.
    pub fn code(&self) -> &str {
        &self.state.code
    }

    pub fn folder(&self) -> &Path {
        &self.state.dir
    }

    pub fn progress(&self) -> UploadProgress {
        self.state
            .progress
            .lock()
            .map(|p| p.clone())
            .unwrap_or_default()
    }

    /// Stops serving; an upload in progress is abandoned and its partial
    /// file removed.
    pub fn stop(&mut self) {
        self.state.stop.store(true, Ordering::Release);
        if let Some(server) = &self.server {
            for _ in 0..WORKERS {
                server.unblock();
            }
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        self.server = None;
    }
}

impl Drop for UploadServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Two random digits.
fn new_code() -> Result<String, RemoteError> {
    let mut b = [0u8; 4];
    getrandom::fill(&mut b).map_err(|e| RemoteError::Random(e.to_string()))?;
    Ok(format!("{:02}", u32::from_le_bytes(b) % 100))
}

fn worker(server: &Server, state: &State) {
    while !state.stop.load(Ordering::Acquire) {
        match server.recv_timeout(RECV_POLL) {
            Ok(Some(rq)) => handle(rq, state),
            Ok(None) => {}
            Err(e) => {
                log::error!("Wi-Fi upload: server failed: {e}");
                break;
            }
        }
    }
}

fn respond(rq: Request, status: u16, content_type: &str, body: impl Into<Vec<u8>>) {
    let mut r = Response::from_data(body.into()).with_status_code(status);
    let headers = SECURITY_HEADERS
        .iter()
        .copied()
        .chain([("Content-Type", content_type)]);
    for (n, v) in headers {
        if let Ok(h) = Header::from_bytes(n.as_bytes(), v.as_bytes()) {
            r.add_header(h);
        }
    }
    if let Err(e) = rq.respond(r) {
        log::debug!("Wi-Fi upload: response not delivered: {e}");
    }
}

fn respond_json(rq: Request, status: u16, value: serde_json::Value) {
    respond(rq, status, "application/json", value.to_string());
}

fn error(rq: Request, status: u16, message: &str) {
    respond_json(rq, status, serde_json::json!({ "error": message }));
}

fn handle(mut rq: Request, state: &State) {
    if !rq.remote_addr().is_some_and(|a| is_allowed_peer(a.ip())) {
        return error(rq, 403, "only devices on your local network can upload");
    }
    let method = rq.method().clone();
    let url = rq.url().to_owned();
    let (path, query) = parse_url(&url);
    match (&method, path) {
        (Method::Get, "/" | "/index.html") => {
            return respond(rq, 200, "text/html; charset=utf-8", UPLOAD_HTML);
        }
        (Method::Get, "/upload.js") => return respond(rq, 200, "text/javascript", UPLOAD_JS),
        (Method::Get, "/upload.css") => return respond(rq, 200, "text/css", UPLOAD_CSS),
        _ => {}
    }
    if state.failures.load(Ordering::Acquire) >= MAX_FAILURES {
        return error(
            rq,
            429,
            "Too many wrong codes. Turn Wi-Fi upload off and on in the headset for a new one.",
        );
    }
    let given = rq
        .headers()
        .iter()
        .find(|h| h.field.equiv("X-Upload-Code"))
        .map(|h| h.value.as_str().trim().to_owned())
        .unwrap_or_default();
    if !constant_time_eq(given.as_bytes(), state.code.as_bytes()) {
        state.failures.fetch_add(1, Ordering::AcqRel);
        return error(rq, 401, "Wrong code. It is shown in the headset.");
    }
    match (&method, path) {
        (Method::Get, "/api/space") => {
            let folder = state
                .dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            respond_json(
                rq,
                200,
                serde_json::json!({ "free": free_space(&state.dir), "folder": folder }),
            )
        }
        (Method::Get, "/api/files") => {
            respond_json(rq, 200, serde_json::json!({ "files": list(&state.dir) }))
        }
        (Method::Delete, "/api/files") => {
            let name = query
                .iter()
                .find(|(k, _)| k == "name")
                .map(|(_, v)| v.as_str())
                .unwrap_or("");
            match delete(name, state) {
                Ok(()) => respond_json(rq, 200, serde_json::json!({ "deleted": name })),
                Err((status, msg)) => error(rq, status, &msg),
            }
        }
        (Method::Put, "/api/upload") => {
            let name = query
                .iter()
                .find(|(k, _)| k == "name")
                .map(|(_, v)| v.as_str());
            match upload(&mut rq, name, state) {
                Ok(saved) => respond_json(rq, 200, serde_json::json!({ "saved": saved })),
                Err((status, msg)) => error(rq, status, &msg),
            }
        }
        _ => error(rq, 404, "not found"),
    }
}

/// Receives one file into the folder; returns the name it was saved as.
fn upload(rq: &mut Request, name: Option<&str>, state: &State) -> Result<String, (u16, String)> {
    let name = name.and_then(clean_name).ok_or((
        400,
        "Only video and subtitle files can be uploaded.".to_string(),
    ))?;
    let len = rq.body_length().ok_or((
        411,
        "The browser did not say how big the file is.".to_string(),
    ))? as u64;
    if let Some(free) = free_space(&state.dir)
        && len.saturating_add(RESERVE) > free
    {
        return Err((
            507,
            format!(
                "Not enough space on the headset: {} needed, {} free.",
                gb(len),
                gb(free.saturating_sub(RESERVE))
            ),
        ));
    }
    let part = state.dir.join(format!(".partial-{name}"));
    state.update(|p| p.current = Some((name.clone(), 0, len)));
    let received = receive(rq.as_reader(), len, &part, &name, state);
    state.update(|p| p.current = None);
    let saved = received.and_then(|()| {
        let dest = unique(&state.dir, &name);
        fs::rename(&part, &dest).map_err(|e| format!("saving: {e}"))?;
        Ok(dest
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or(name.clone()))
    });
    match saved {
        Ok(saved) => {
            log::info!("Wi-Fi upload: saved {saved} ({})", gb(len));
            state.update(|p| p.saved.push(saved.clone()));
            Ok(saved)
        }
        Err(e) => {
            let _ = fs::remove_file(&part);
            log::warn!("Wi-Fi upload: {name}: {e}");
            state.update(|p| p.failed = Some((name.clone(), e.clone())));
            Err((500, e))
        }
    }
}

fn receive(
    mut body: impl Read,
    len: u64,
    part: &Path,
    name: &str,
    state: &State,
) -> Result<(), String> {
    let mut f = File::create(part).map_err(|e| format!("cannot create the file: {e}"))?;
    let mut buf = vec![0u8; CHUNK];
    let mut done = 0u64;
    while done < len {
        if state.stop.load(Ordering::Acquire) {
            return Err("Wi-Fi upload was turned off in the headset.".into());
        }
        let want = (len - done).min(CHUNK as u64) as usize;
        let n = body
            .read(&mut buf[..want])
            .map_err(|e| format!("connection lost: {e}"))?;
        if n == 0 {
            return Err(format!(
                "connection closed after {} of {}",
                gb(done),
                gb(len)
            ));
        }
        f.write_all(&buf[..n])
            .map_err(|e| format!("writing failed: {e}"))?;
        done += n as u64;
        state.update(|p| p.current = Some((name.to_owned(), done, len)));
    }
    f.sync_all().map_err(|e| format!("writing failed: {e}"))
}

/// Videos and subtitles right in the folder (not hidden, not in
/// subfolders), newest first.
fn list(dir: &Path) -> Vec<serde_json::Value> {
    let mut files: Vec<(String, u64, u64)> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let meta = e.metadata().ok()?;
            let listed = meta.is_file()
                && !name.starts_with('.')
                && (is_video_name(&name) || is_subtitle_name(&name));
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            listed.then_some((name, meta.len(), modified))
        })
        .collect();
    files.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    files
        .into_iter()
        .map(|(name, size, modified)| {
            serde_json::json!({ "name": name, "size": size, "modified": modified })
        })
        .collect()
}

/// Deletes one listed file. `name` must be exactly a listed name: nothing
/// outside the folder can be reached.
fn delete(name: &str, state: &State) -> Result<(), (u16, String)> {
    let listed = !name.is_empty()
        && !name.starts_with('.')
        && !name.contains(['/', '\\'])
        && (is_video_name(name) || is_subtitle_name(name));
    let path = state.dir.join(name);
    if !listed || !path.is_file() {
        return Err((404, "That file is not on the headset.".into()));
    }
    match fs::remove_file(&path) {
        Ok(()) => {
            log::info!("Wi-Fi upload: deleted {name}");
            state.update(|p| p.deleted.push(name.to_owned()));
            Ok(())
        }
        // Android's shared storage answers "no such file" (not "permission
        // denied") for a file another app wrote, though it lists it.
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
            ) =>
        {
            Err((
                403,
                "The headset only lets FramePlayer delete videos it received itself. Delete \
                 this one in the headset's Files app."
                    .into(),
            ))
        }
        Err(e) => Err((500, format!("Could not delete it: {e}"))),
    }
}

/// A safe file name from what the browser sent, or `None` if it is not a
/// video or subtitle file.
pub(crate) fn clean_name(raw: &str) -> Option<String> {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
        .collect();
    let cleaned = cleaned
        .trim_start_matches(['.', ' '])
        .trim_end_matches(['.', ' ']);
    let (stem, ext) = cleaned.rsplit_once('.')?;
    let mut stem = stem.trim().to_string();
    let budget = MAX_NAME_BYTES.saturating_sub(ext.len() + 1);
    while stem.len() > budget {
        stem.pop();
    }
    let name = format!("{}.{ext}", stem.trim_end());
    (!stem.trim().is_empty() && (is_video_name(&name) || is_subtitle_name(&name))).then_some(name)
}

/// `dir/name`, or `dir/name (2)`, ... if taken.
fn unique(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = name.rsplit_once('.').unwrap_or((name, ""));
    (2..10_000)
        .map(|i| dir.join(format!("{stem} ({i}).{ext}")))
        .find(|p| !p.exists())
        .unwrap_or(first)
}

/// Bytes free for this app in the folder's filesystem.
fn free_space(dir: &Path) -> Option<u64> {
    let c = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: valid NUL-terminated path and an out struct statvfs fills.
    unsafe {
        let mut s: libc::statvfs = std::mem::zeroed();
        (libc::statvfs(c.as_ptr(), &mut s) == 0).then(|| s.f_bavail as u64 * s.f_frsize as u64)
    }
}

fn gb(bytes: u64) -> String {
    if bytes >= 1 << 30 {
        format!("{:.1} GB", bytes as f64 / (1u64 << 30) as f64)
    } else {
        format!("{} MB", bytes >> 20)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_cleaned() {
        assert_eq!(
            clean_name("eye_cleaning.mp4").as_deref(),
            Some("eye_cleaning.mp4")
        );
        assert_eq!(
            clean_name("C:\\Users\\me\\a/b<c>?.mkv").as_deref(),
            Some("bc.mkv")
        );
        assert_eq!(
            clean_name("../../.hidden.mp4").as_deref(),
            Some("hidden.mp4")
        );
        assert_eq!(clean_name("subs.srt").as_deref(), Some("subs.srt"));
        assert_eq!(clean_name("evil.sh"), None);
        assert_eq!(clean_name(".mp4"), None);
        assert_eq!(clean_name("noext"), None);
        let long = format!("{}.mp4", "x".repeat(400));
        assert_eq!(clean_name(&long).map(|n| n.len()), Some(MAX_NAME_BYTES));
    }

    #[test]
    fn taken_names_get_a_number() {
        let dir = std::env::temp_dir().join(format!("fp-upload-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.mp4"), b"x").unwrap();
        fs::write(dir.join("a (2).mp4"), b"x").unwrap();
        assert_eq!(unique(&dir, "a.mp4"), dir.join("a (3).mp4"));
        assert_eq!(unique(&dir, "b.mp4"), dir.join("b.mp4"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn upload_end_to_end() {
        let dir = std::env::temp_dir().join(format!("fp-upload-e2e-{}", std::process::id()));
        let mut up = UploadServer::start("127.0.0.1:0".parse().unwrap(), dir.clone()).unwrap();
        let addr = up.addr();
        let put = |code: &str, body: &[u8]| -> String {
            let mut s = std::net::TcpStream::connect(addr).unwrap();
            write!(
                s,
                "PUT /api/upload?name=clip%201.mp4 HTTP/1.1\r\nHost: x\r\nX-Upload-Code: {code}\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            s.write_all(body).unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).unwrap();
            out
        };
        assert!(put("000000x", b"abc").starts_with("HTTP/1.1 401"));
        let ok = put(up.code(), b"hello video");
        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
        assert!(ok.contains("\"saved\":\"clip 1.mp4\""), "{ok}");
        assert_eq!(fs::read(dir.join("clip 1.mp4")).unwrap(), b"hello video");
        assert!(!dir.join(".partial-clip 1.mp4").exists());
        assert_eq!(up.progress().saved, vec!["clip 1.mp4".to_string()]);
        let call = |method: &str, path: &str| -> String {
            let mut s = std::net::TcpStream::connect(addr).unwrap();
            write!(
                s,
                "{method} {path} HTTP/1.1\r\nHost: x\r\nX-Upload-Code: {}\r\n\
                 Content-Length: 0\r\nConnection: close\r\n\r\n",
                up.code()
            )
            .unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).unwrap();
            out
        };
        let listed = call("GET", "/api/files");
        assert!(listed.contains("\"name\":\"clip 1.mp4\""), "{listed}");
        assert!(call("DELETE", "/api/files?name=..%2Fx.mp4").starts_with("HTTP/1.1 404"));
        assert!(call("DELETE", "/api/files?name=clip%201.mp4").starts_with("HTTP/1.1 200"));
        assert!(!dir.join("clip 1.mp4").exists());
        assert_eq!(up.progress().deleted, vec!["clip 1.mp4".to_string()]);
        // One wrong code so far; four more and even the right one is refused.
        for _ in 1..MAX_FAILURES {
            assert!(put("xx", b"").starts_with("HTTP/1.1 401"));
        }
        assert!(call("GET", "/api/files").starts_with("HTTP/1.1 429"));
        up.stop();
        fs::remove_dir_all(&dir).unwrap();
    }
}
