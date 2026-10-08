//! Network side: fetching and verifying the manifest, and resumable,
//! checksum-verified payload downloads.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ed25519_dalek::VerifyingKey;
use semver::Version;
use sha2::{Digest, Sha256};

use crate::error::{Error, IoContext, Result};
use crate::manifest::{Channel, ReleaseManifest, Update, select_update, verify_manifest};
use crate::url::{check_download_url, file_name_from_url};

/// Largest manifest or signature we are willing to read.
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const CHUNK: usize = 64 * 1024;

/// Builds the default HTTP agent: rustls with the bundled Mozilla roots,
/// proxy from the environment (`HTTPS_PROXY`, `NO_PROXY`), status codes
/// returned rather than turned into errors, and no response compression so
/// byte ranges are raw file bytes.
pub fn default_agent() -> ureq::Agent {
    agent_with_proxy(ureq::Proxy::try_from_env())
}

/// Like [`default_agent`] but with an explicit proxy (or none).
pub fn agent_with_proxy(proxy: Option<ureq::Proxy>) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .proxy(proxy)
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .user_agent(concat!("frameplayer-updater/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent()
}

/// Checks for and downloads updates, holding the trusted release key, the
/// HTTP agent and the architecture to download for.
#[derive(Debug, Clone)]
pub struct Updater {
    key: VerifyingKey,
    agent: ureq::Agent,
    arch: String,
}

impl Updater {
    /// An updater trusting the release key compiled into this build
    /// ([`crate::RELEASE_PUBLIC_KEY`]), downloading builds for the running
    /// CPU architecture. Fails with [`Error::NoPublicKey`] when the build
    /// has no key.
    pub fn new() -> Result<Self> {
        Ok(Self::with_key(crate::release_key()?))
    }

    /// An updater trusting `key` (tests, forks, the PC installer).
    pub fn with_key(key: VerifyingKey) -> Self {
        Updater {
            key,
            agent: default_agent(),
            arch: std::env::consts::ARCH.to_string(),
        }
    }

    /// Replaces the HTTP agent.
    pub fn agent(mut self, agent: ureq::Agent) -> Self {
        self.agent = agent;
        self
    }

    /// Selects builds for `arch` instead of the running CPU (the PC-side
    /// installer asks for `aarch64`).
    pub fn arch(mut self, arch: impl Into<String>) -> Self {
        self.arch = arch.into();
        self
    }

    /// Fetches `manifest_url` and `manifest_url + ".sig"`, verifies the
    /// signature and manifest, and returns the verified manifest.
    pub fn fetch_manifest(&self, manifest_url: &str) -> Result<ReleaseManifest> {
        check_download_url(manifest_url)?;
        let bytes = self.get_small(manifest_url)?;
        let sig_url = format!("{manifest_url}.sig");
        let sig = self.get_small(&sig_url)?;
        let sig = String::from_utf8(sig)
            .map_err(|_| Error::MalformedSignature("signature file is not text".into()))?;
        verify_manifest(&bytes, &sig, &self.key)
    }

    /// Returns the update on offer for a user running `current_version` on
    /// `channel`, or `None` when up to date. See [`select_update`] for the
    /// rules.
    pub fn check(
        &self,
        manifest_url: &str,
        current_version: &Version,
        channel: Channel,
    ) -> Result<Option<Update>> {
        let manifest = self.fetch_manifest(manifest_url)?;
        select_update(&manifest, current_version, channel, &self.arch)
    }

    fn get_small(&self, url: &str) -> Result<Vec<u8>> {
        let resp = self.agent.get(url).call().map_err(|e| http_err(url, e))?;
        let status = resp.status().as_u16();
        if status != 200 {
            return Err(Error::HttpStatus {
                url: url.to_string(),
                status,
            });
        }
        resp.into_body()
            .into_with_config()
            .limit(MAX_MANIFEST_BYTES)
            .read_to_vec()
            .map_err(|e| http_err(url, e))
    }

    /// Downloads the update's zip into the directory `dest_dir`, verifying
    /// size and SHA-256 against the signed manifest, and returns the path of
    /// the finished file.
    ///
    /// Data is written to `<name>.part` first. If a `.part` file is left
    /// from an earlier attempt (cancelled, network drop), the download
    /// resumes with an HTTP `Range` request; servers that ignore the range
    /// get a clean restart. A finished file that already verifies is
    /// returned without touching the network.
    ///
    /// `progress(done, total)` is called after every chunk. Setting `cancel`
    /// stops the download with [`Error::Cancelled`] and keeps the partial
    /// file. On checksum mismatch the partial file is deleted.
    pub fn download(
        &self,
        update: &Update,
        dest_dir: &Path,
        progress: impl FnMut(u64, u64),
        cancel: &AtomicBool,
    ) -> Result<PathBuf> {
        download_with(&self.agent, update, dest_dir, progress, cancel)
    }
}

/// [`Updater::download`] with an explicit HTTP agent. Needs no key: the
/// size and digest it checks come from an [`Update`], which only exists
/// after a verified manifest.
pub fn download_with(
    agent: &ureq::Agent,
    update: &Update,
    dest_dir: &Path,
    mut progress: impl FnMut(u64, u64),
    cancel: &AtomicBool,
) -> Result<PathBuf> {
    {
        let art = &update.artifact;
        check_download_url(&art.url)?;
        fs::create_dir_all(dest_dir).ctx("cannot create", dest_dir)?;
        let name = file_name_from_url(&art.url)
            .unwrap_or_else(|| format!("frameplayer-{}-{}.zip", update.version, art.arch));
        let final_path = dest_dir.join(&name);
        let part_path = dest_dir.join(format!("{name}.part"));
        let expected = art.sha256.to_ascii_lowercase();

        if final_path.is_file() {
            let (len, digest) = hash_file(&final_path)?;
            if len == art.size && digest == expected {
                progress(len, art.size);
                return Ok(final_path);
            }
            fs::remove_file(&final_path).ctx("cannot remove", &final_path)?;
        }

        // Two passes at most: a resume attempt, then a clean restart if the
        // server's range answer was unusable.
        let mut restarted = false;
        let received = loop {
            match fetch_into(agent, &art.url, &part_path, art.size, &mut progress, cancel)? {
                Fetch::Done(n) => break n,
                Fetch::RestartNeeded if !restarted => {
                    restarted = true;
                    truncate(&part_path)?;
                }
                Fetch::RestartNeeded => {
                    return Err(Error::HttpStatus {
                        url: art.url.clone(),
                        status: 416,
                    });
                }
            }
        };
        if received != art.size {
            if received > art.size {
                let _ = fs::remove_file(&part_path);
            }
            return Err(Error::SizeMismatch {
                url: art.url.clone(),
                expected: art.size,
                actual: received,
            });
        }
        let (_, actual) = hash_file(&part_path)?;
        if actual != expected {
            let _ = fs::remove_file(&part_path);
            return Err(Error::ChecksumMismatch {
                path: part_path,
                expected,
                actual,
            });
        }
        fs::rename(&part_path, &final_path).ctx("cannot rename", &part_path)?;
        Ok(final_path)
    }
}

/// One HTTP attempt, appending to `part`. Returns the total bytes now in
/// `part`.
fn fetch_into(
    agent: &ureq::Agent,
    url: &str,
    part: &Path,
    size: u64,
    progress: &mut impl FnMut(u64, u64),
    cancel: &AtomicBool,
) -> Result<Fetch> {
    let mut have = match fs::metadata(part) {
        Ok(m) => m.len(),
        Err(_) => 0,
    };
    if have > size {
        truncate(part)?;
        have = 0;
    }
    if have == size {
        return Ok(Fetch::Done(have));
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    let mut req = agent.get(url);
    if have > 0 {
        req = req.header("Range", format!("bytes={have}-"));
    }
    let resp = req.call().map_err(|e| http_err(url, e))?;
    let status = resp.status().as_u16();
    let start = match status {
        200 => 0,
        206 => {
            let start = resp
                .headers()
                .get("content-range")
                .and_then(|v| v.to_str().ok())
                .and_then(content_range_start);
            if start != Some(have) {
                return Ok(Fetch::RestartNeeded);
            }
            have
        }
        416 if have > 0 => return Ok(Fetch::RestartNeeded),
        _ => {
            return Err(Error::HttpStatus {
                url: url.to_string(),
                status,
            });
        }
    };
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(part)
        .ctx("cannot open", part)?;
    file.set_len(start).ctx("cannot truncate", part)?;
    file.seek(SeekFrom::Start(start)).ctx("cannot seek", part)?;
    let mut done = start;
    progress(done, size);
    let mut body = resp.into_body().into_reader();
    let mut buf = vec![0u8; CHUNK];
    loop {
        if cancel.load(Ordering::Relaxed) {
            file.flush().ctx("cannot write", part)?;
            return Err(Error::Cancelled);
        }
        let n = match body.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                // Keep what we have; the next attempt resumes from here.
                let _ = file.flush();
                return Err(Error::io("download interrupted while reading", url, e));
            }
        };
        done += n as u64;
        if done > size {
            return Ok(Fetch::Done(done));
        }
        file.write_all(&buf[..n]).ctx("cannot write", part)?;
        progress(done, size);
    }
    file.sync_all().ctx("cannot sync", part)?;
    Ok(Fetch::Done(done))
}

enum Fetch {
    Done(u64),
    RestartNeeded,
}

fn http_err(url: &str, e: ureq::Error) -> Error {
    Error::Http {
        url: url.to_string(),
        source: Box::new(e),
    }
}

fn truncate(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::io("cannot remove", path, e)),
    }
}

/// `bytes 100-199/200` → `Some(100)`.
fn content_range_start(v: &str) -> Option<u64> {
    let rest = v.trim().strip_prefix("bytes")?.trim_start();
    let (range, _total) = rest.split_once('/')?;
    let (start, _end) = range.split_once('-')?;
    start.trim().parse().ok()
}

/// Length and lower-case hex SHA-256 of a file.
pub fn hash_file(path: &Path) -> Result<(u64, String)> {
    let mut f = File::open(path).ctx("cannot open", path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut len = 0u64;
    loop {
        let n = f.read(&mut buf).ctx("cannot read", path)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        len += n as u64;
    }
    Ok((len, hex_lower(&hasher.finalize())))
}

/// Lower-case hex encoding.
pub fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Artifact, MANIFEST_NAME, sign_manifest};
    use ed25519_dalek::SigningKey;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::thread;

    /// What the test server should do for a path.
    #[derive(Clone)]
    enum Route {
        /// Serve bytes, honouring `Range` when `ranges` is true.
        Bytes { data: Vec<u8>, ranges: bool },
    }

    /// Request path and `Range` header of every request seen.
    type RequestLog = Arc<Mutex<Vec<(String, Option<String>)>>>;

    struct Server {
        base: String,
        log: RequestLog,
        routes: Arc<Mutex<HashMap<String, Route>>>,
        server: Arc<tiny_http::Server>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl Server {
        fn start() -> Server {
            let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
            let port = server.server_addr().to_ip().unwrap().port();
            let log: RequestLog = Arc::default();
            let routes: Arc<Mutex<HashMap<String, Route>>> = Arc::default();
            let (s, l, r) = (server.clone(), log.clone(), routes.clone());
            let handle = thread::spawn(move || {
                for req in s.incoming_requests() {
                    let range = req
                        .headers()
                        .iter()
                        .find(|h| h.field.equiv("Range"))
                        .map(|h| h.value.to_string());
                    l.lock()
                        .unwrap()
                        .push((req.url().to_string(), range.clone()));
                    let route = r.lock().unwrap().get(req.url()).cloned();
                    respond(req, route, range);
                }
            });
            Server {
                base: format!("http://127.0.0.1:{port}"),
                log,
                routes,
                server,
                handle: Some(handle),
            }
        }
        fn route(&self, path: &str, route: Route) {
            self.routes.lock().unwrap().insert(path.into(), route);
        }
        fn url(&self, path: &str) -> String {
            format!("{}{path}", self.base)
        }
        fn requests(&self) -> Vec<(String, Option<String>)> {
            self.log.lock().unwrap().clone()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.server.unblock();
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    fn respond(req: tiny_http::Request, route: Option<Route>, range: Option<String>) {
        use tiny_http::{Header, Response, StatusCode};
        match route {
            None => {
                let _ = req.respond(Response::empty(StatusCode(404)));
            }
            Some(Route::Bytes { data, ranges }) => {
                let start = range
                    .filter(|_| ranges)
                    .and_then(|r| r.strip_prefix("bytes=")?.strip_suffix('-')?.parse().ok());
                match start {
                    Some(s) if s >= data.len() => {
                        let _ = req.respond(Response::empty(StatusCode(416)));
                    }
                    Some(s) => {
                        let cr = format!("bytes {s}-{}/{}", data.len() - 1, data.len());
                        let resp = Response::from_data(data[s..].to_vec())
                            .with_status_code(206)
                            .with_header(Header::from_bytes("Content-Range", cr).unwrap());
                        let _ = req.respond(resp);
                    }
                    None => {
                        let _ = req.respond(Response::from_data(data));
                    }
                }
            }
        }
    }

    /// A one-shot raw server that promises `data.len()` bytes, sends the
    /// first `n`, then closes the socket (tiny_http keeps connections open,
    /// so it cannot simulate this).
    fn truncating_server(data: Vec<u8>, n: usize) -> (String, thread::JoinHandle<()>) {
        use std::io::{BufRead, BufReader};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap() > 2 {
                line.clear();
            }
            let mut w = stream;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                data.len()
            );
            w.write_all(head.as_bytes()).unwrap();
            w.write_all(&data[..n]).unwrap();
            w.flush().unwrap();
            let _ = w.shutdown(std::net::Shutdown::Both);
        });
        (format!("http://127.0.0.1:{port}"), handle)
    }

    fn payload(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 % 251) as u8).collect()
    }

    fn sha_hex(data: &[u8]) -> String {
        hex_lower(&Sha256::digest(data))
    }

    fn update_for(url: String, data: &[u8]) -> Update {
        Update {
            version: Version::new(1, 0, 0),
            channel: Channel::Stable,
            published: "2026-10-02T00:00:00Z".into(),
            notes: String::new(),
            artifact: Artifact {
                arch: "aarch64".into(),
                url,
                sha256: sha_hex(data),
                size: data.len() as u64,
            },
        }
    }

    fn updater(key: &SigningKey) -> Updater {
        Updater::with_key(key.verifying_key())
            .agent(agent_with_proxy(None))
            .arch("aarch64")
    }

    #[test]
    fn check_fetches_and_verifies_manifest() {
        let srv = Server::start();
        let key = SigningKey::from_bytes(&[9; 32]);
        let m = ReleaseManifest {
            name: MANIFEST_NAME.into(),
            version: Version::new(2, 0, 0),
            channel: Channel::Stable,
            published: "2026-10-02T00:00:00Z".into(),
            notes: "new".into(),
            artifacts: vec![Artifact {
                arch: "aarch64".into(),
                url: srv.url("/fp.zip"),
                sha256: "0".repeat(64),
                size: 1,
            }],
        };
        let bytes = serde_json::to_vec(&m).unwrap();
        let sig = sign_manifest(&bytes, &key);
        srv.route(
            "/manifest.json",
            Route::Bytes {
                data: bytes,
                ranges: false,
            },
        );
        srv.route(
            "/manifest.json.sig",
            Route::Bytes {
                data: sig.into_bytes(),
                ranges: false,
            },
        );

        let u = updater(&key);
        let url = srv.url("/manifest.json");
        let got = u
            .check(&url, &Version::new(1, 0, 0), Channel::Stable)
            .unwrap();
        assert_eq!(got.unwrap().version, Version::new(2, 0, 0));
        assert!(
            u.check(&url, &Version::new(2, 0, 0), Channel::Stable)
                .unwrap()
                .is_none()
        );

        // Same manifest, different trusted key: rejected.
        let other = updater(&SigningKey::from_bytes(&[8; 32]));
        assert!(matches!(
            other.check(&url, &Version::new(1, 0, 0), Channel::Stable),
            Err(Error::SignatureMismatch)
        ));
        // Missing signature: rejected.
        let unsigned = srv.url("/nosig.json");
        srv.route(
            "/nosig.json",
            Route::Bytes {
                data: b"{}".to_vec(),
                ranges: false,
            },
        );
        assert!(matches!(
            u.check(&unsigned, &Version::new(1, 0, 0), Channel::Stable),
            Err(Error::HttpStatus { status: 404, .. })
        ));
    }

    #[test]
    fn downloads_and_verifies() {
        let srv = Server::start();
        let data = payload(300_000);
        srv.route(
            "/fp-1.0.0.zip",
            Route::Bytes {
                data: data.clone(),
                ranges: true,
            },
        );
        let dir = tempfile::tempdir().unwrap();
        let up = update_for(srv.url("/fp-1.0.0.zip"), &data);
        let mut last = (0, 0);
        let path = updater(&SigningKey::from_bytes(&[1; 32]))
            .download(
                &up,
                dir.path(),
                |d, t| last = (d, t),
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(path, dir.path().join("fp-1.0.0.zip"));
        assert_eq!(fs::read(&path).unwrap(), data);
        assert_eq!(last, (300_000, 300_000));
        assert!(!dir.path().join("fp-1.0.0.zip.part").exists());

        // Second call: already complete and verified, no new request.
        let before = srv.requests().len();
        updater(&SigningKey::from_bytes(&[1; 32]))
            .download(&up, dir.path(), |_, _| {}, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(srv.requests().len(), before);
    }

    #[test]
    fn resumes_partial_download_with_range() {
        let srv = Server::start();
        let data = payload(200_000);
        srv.route(
            "/r.zip",
            Route::Bytes {
                data: data.clone(),
                ranges: true,
            },
        );
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("r.zip.part"), &data[..123_456]).unwrap();
        let up = update_for(srv.url("/r.zip"), &data);
        let path = updater(&SigningKey::from_bytes(&[1; 32]))
            .download(&up, dir.path(), |_, _| {}, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(fs::read(path).unwrap(), data);
        let reqs = srv.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].1.as_deref(), Some("bytes=123456-"));
    }

    #[test]
    fn restarts_when_server_ignores_range() {
        let srv = Server::start();
        let data = payload(50_000);
        srv.route(
            "/n.zip",
            Route::Bytes {
                data: data.clone(),
                ranges: false,
            },
        );
        let dir = tempfile::tempdir().unwrap();
        // Garbage prefix that would corrupt the result if appended to.
        fs::write(dir.path().join("n.zip.part"), vec![0xAA; 1000]).unwrap();
        let up = update_for(srv.url("/n.zip"), &data);
        let path = updater(&SigningKey::from_bytes(&[1; 32]))
            .download(&up, dir.path(), |_, _| {}, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(fs::read(path).unwrap(), data);
    }

    #[test]
    fn dropped_connection_then_resume() {
        let data = payload(100_000);
        let (base, h) = truncating_server(data.clone(), 40_000);
        let dir = tempfile::tempdir().unwrap();
        let mut up = update_for(format!("{base}/d.zip"), &data);
        let u = updater(&SigningKey::from_bytes(&[1; 32]));
        let err = u
            .download(&up, dir.path(), |_, _| {}, &AtomicBool::new(false))
            .unwrap_err();
        h.join().unwrap();
        assert!(
            matches!(err, Error::Io { .. } | Error::SizeMismatch { .. }),
            "{err:?}"
        );
        let part = dir.path().join("d.zip.part");
        assert_eq!(fs::read(&part).unwrap(), data[..40_000]);

        // The mirror comes back (same file name, new host) and supports
        // ranges: only the rest is fetched.
        let srv = Server::start();
        srv.route(
            "/d.zip",
            Route::Bytes {
                data: data.clone(),
                ranges: true,
            },
        );
        up.artifact.url = srv.url("/d.zip");
        let path = u
            .download(&up, dir.path(), |_, _| {}, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(fs::read(path).unwrap(), data);
        let reqs = srv.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].1.as_deref(), Some("bytes=40000-"));
    }

    #[test]
    fn checksum_mismatch_deletes_partial() {
        let srv = Server::start();
        let data = payload(10_000);
        let mut evil = data.clone();
        evil[5000] ^= 1;
        srv.route(
            "/c.zip",
            Route::Bytes {
                data: evil,
                ranges: true,
            },
        );
        let dir = tempfile::tempdir().unwrap();
        let up = update_for(srv.url("/c.zip"), &data);
        let err = updater(&SigningKey::from_bytes(&[1; 32]))
            .download(&up, dir.path(), |_, _| {}, &AtomicBool::new(false))
            .unwrap_err();
        assert!(matches!(err, Error::ChecksumMismatch { .. }), "{err:?}");
        assert!(!dir.path().join("c.zip.part").exists());
        assert!(!dir.path().join("c.zip").exists());
    }

    #[test]
    fn oversized_response_is_rejected() {
        let srv = Server::start();
        let data = payload(10_000);
        srv.route(
            "/big.zip",
            Route::Bytes {
                data: payload(20_000),
                ranges: true,
            },
        );
        let dir = tempfile::tempdir().unwrap();
        let up = update_for(srv.url("/big.zip"), &data);
        let err = updater(&SigningKey::from_bytes(&[1; 32]))
            .download(&up, dir.path(), |_, _| {}, &AtomicBool::new(false))
            .unwrap_err();
        assert!(matches!(err, Error::SizeMismatch { .. }), "{err:?}");
        assert!(!dir.path().join("big.zip.part").exists());
    }

    #[test]
    fn cancel_keeps_partial() {
        let srv = Server::start();
        let data = payload(1_000_000);
        srv.route(
            "/k.zip",
            Route::Bytes {
                data: data.clone(),
                ranges: true,
            },
        );
        let dir = tempfile::tempdir().unwrap();
        let up = update_for(srv.url("/k.zip"), &data);
        let cancel = AtomicBool::new(false);
        let err = updater(&SigningKey::from_bytes(&[1; 32]))
            .download(
                &up,
                dir.path(),
                |done, _| {
                    if done >= 100_000 {
                        cancel.store(true, Ordering::Relaxed);
                    }
                },
                &cancel,
            )
            .unwrap_err();
        assert!(matches!(err, Error::Cancelled));
        let kept = fs::metadata(dir.path().join("k.zip.part")).unwrap().len();
        assert!((100_000..1_000_000).contains(&kept), "{kept}");
        let path = updater(&SigningKey::from_bytes(&[1; 32]))
            .download(&up, dir.path(), |_, _| {}, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(fs::read(path).unwrap(), data);
    }

    #[test]
    fn insecure_download_url_rejected_before_request() {
        let up = update_for("http://example.com/x.zip".into(), b"x");
        let dir = tempfile::tempdir().unwrap();
        let err = updater(&SigningKey::from_bytes(&[1; 32]))
            .download(&up, dir.path(), |_, _| {}, &AtomicBool::new(false))
            .unwrap_err();
        assert!(matches!(err, Error::RejectedUrl { .. }));
    }

    #[test]
    fn content_range_parsing() {
        assert_eq!(content_range_start("bytes 100-199/200"), Some(100));
        assert_eq!(content_range_start("bytes 0-0/*"), Some(0));
        assert_eq!(content_range_start("items 1-2/3"), None);
    }
}
