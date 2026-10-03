//! The Handy through its cloud API v2, playing scripts with HSSP (Handy
//! Synchronised Script Protocol).
//!
//! The Handy downloads the whole script from a URL and plays it against the
//! cloud server's clock, so this is a [`SyncMode::Script`] device: the
//! engine uploads the script and tells the Handy where to start.
//!
//! Sequence:
//!
//! 1. `GET {api}/connected` → `{"connected": true}`; refuse to connect
//!    otherwise.
//! 2. Server-time sync: several `GET {api}/servertime` → `{"serverTime":
//!    ms}`. Each round estimates `offset = serverTime + rtt/2 - local
//!    receive time`; the offsets of the faster half of the rounds are
//!    averaged.
//! 3. `PUT {api}/mode {"mode": 1}` (HSSP).
//! 4. On script load: upload the script as CSV (`time_ms,pos` lines) as a
//!    multipart file field (`syncFile` by default) to the upload endpoint,
//!    which answers JSON with a `url`; then `PUT {api}/hssp/setup {"url":
//!    url}`.
//! 5. On play, seek, or speed change: `PUT {api}/hssp/play
//!    {"estimatedServerTime": local now + offset, "startTime":
//!    script_ms}`; on pause `PUT {api}/hssp/stop`.
//!
//! [`Device::move_to`] uses HDSP instead (`PUT {api}/mode {"mode": 2}`,
//! then `PUT {api}/hdsp/xpt {"position": percent, "duration": ms,
//! "stopOnTarget": true, "immediateResponse": true}`); the next play
//! switches back to HSSP and re-runs the setup.
//!
//! Every request to the API carries the `X-Connection-Key` header. The
//! upload request does not (it goes to a separate hosting service).
//!
//! Assumptions that could not be verified offline, all configurable in
//! [`HandyConfig`]:
//!
//! - base URL `https://www.handyfeeling.com/api/handy/v2` and upload URL
//!   `https://www.handyfeeling.com/api/sync/upload`;
//! - the upload's multipart field name: `syncFile` is what the
//!   `/api/sync/upload` hosting endpoint has been documented to take; the
//!   newer temporary hosting service
//!   (`https://scripts01.handyfeeling.com/api/script/v0/temp/upload`) takes
//!   `file`. Set [`HandyConfig::upload_field`] together with
//!   [`HandyConfig::upload_url`];
//! - modes are numbered HAMP 0, HSSP 1, HDSP 2, and `/hdsp/xpt` takes a
//!   position in percent with a duration in milliseconds;
//! - a response is an error when its HTTP status is not 2xx, when the JSON
//!   body has an `error` object (its `message` is reported, and
//!   `"connected": false` in it marks the device offline), or when it has a
//!   negative `result`;
//! - the CSV has no header line and integer positions 0–100;
//! - HSSP plays only at 1x. At any other speed the Handy is stopped and
//!   [`Device::notice`] says why; it resumes when the speed returns to 1x.
//!
//! All HTTP runs on a dedicated thread; the [`Device`] methods only queue
//! work, and newer commands supersede queued ones.

use crate::axis::Axis;
use crate::device::{Device, SyncMode};
use crate::error::{Error, Result};
use crate::script::Script;
use fp_core::playback::now_ms;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

/// Default API base URL.
pub const DEFAULT_API_BASE: &str = "https://www.handyfeeling.com/api/handy/v2";

/// Default script upload (hosting) endpoint.
pub const DEFAULT_UPLOAD_URL: &str = "https://www.handyfeeling.com/api/sync/upload";

/// Default multipart field name for the script upload.
pub const DEFAULT_UPLOAD_FIELD: &str = "syncFile";

/// HSSP mode number for `PUT /mode`.
const MODE_HSSP: u8 = 1;
/// HDSP mode number for `PUT /mode`.
const MODE_HDSP: u8 = 2;

/// Configuration of a [`HandyDevice`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HandyConfig {
    /// The connection key shown in the Handy app.
    pub connection_key: String,
    /// API base URL, without a trailing slash.
    pub api_base: String,
    /// Script upload endpoint (multipart POST, answers `{"url": ...}`).
    pub upload_url: String,
    /// Name of the multipart field that carries the script file.
    pub upload_field: String,
    /// Display name (also the settings key).
    pub name: String,
    /// Round trips for the server-time sync.
    pub time_sync_rounds: u32,
    /// Per-request timeout, milliseconds.
    pub timeout_ms: u64,
    /// Minimum time between play commands, milliseconds.
    pub min_interval_ms: u32,
    /// Honour `HTTPS_PROXY` / `NO_PROXY` from the environment.
    pub use_system_proxy: bool,
}

impl Default for HandyConfig {
    fn default() -> Self {
        HandyConfig {
            connection_key: String::new(),
            api_base: DEFAULT_API_BASE.into(),
            upload_url: DEFAULT_UPLOAD_URL.into(),
            upload_field: DEFAULT_UPLOAD_FIELD.into(),
            name: "The Handy".into(),
            time_sync_rounds: 10,
            timeout_ms: 10_000,
            min_interval_ms: 250,
            use_system_proxy: true,
        }
    }
}

struct Api {
    agent: ureq::Agent,
    base: String,
    key: String,
    upload_url: String,
    upload_field: String,
}

/// Turns a response into JSON, or an error per the module's rules.
fn check(status: u16, body: &str) -> Result<Value> {
    let v: Value = if body.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(body).unwrap_or_else(|_| Value::String(body.to_owned()))
    };
    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| err.to_string());
        if err.get("connected").and_then(Value::as_bool) == Some(false) {
            return Err(Error::NotConnected(msg));
        }
        return Err(Error::Http(msg));
    }
    if !(200..300).contains(&status) {
        return Err(Error::Http(format!("status {status}: {body}")));
    }
    if let Some(r) = v.get("result").and_then(Value::as_i64) {
        if r < 0 {
            return Err(Error::Http(format!("result {r}: {body}")));
        }
    }
    Ok(v)
}

impl Api {
    fn new(config: &HandyConfig) -> Api {
        let mut b = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_millis(config.timeout_ms.max(100))))
            .http_status_as_error(false);
        if !config.use_system_proxy {
            b = b.proxy(None);
        }
        Api {
            agent: b.build().new_agent(),
            base: config.api_base.trim_end_matches('/').to_owned(),
            key: config.connection_key.trim().to_owned(),
            upload_url: config.upload_url.clone(),
            upload_field: config.upload_field.clone(),
        }
    }

    fn finish(
        resp: std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<Value> {
        let mut resp = resp?;
        let status = resp.status().as_u16();
        let body = resp.body_mut().read_to_string()?;
        check(status, &body)
    }

    fn get(&self, path: &str) -> Result<Value> {
        Api::finish(
            self.agent
                .get(format!("{}{path}", self.base))
                .header("X-Connection-Key", &self.key)
                .header("Accept", "application/json")
                .call(),
        )
    }

    fn put(&self, path: &str, body: &Value) -> Result<Value> {
        Api::finish(
            self.agent
                .put(format!("{}{path}", self.base))
                .header("X-Connection-Key", &self.key)
                .header("Accept", "application/json")
                .header("Content-Type", "application/json")
                .send(body.to_string()),
        )
    }

    /// Uploads a CSV script; returns the URL the Handy should fetch.
    fn upload(&self, csv: &str) -> Result<String> {
        let boundary = format!("----fp-haptics-{:x}", now_ms());
        let mut body = Vec::with_capacity(csv.len() + 256);
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"frameplayer.csv\"\r\nContent-Type: text/csv\r\n\r\n",
                field = self.upload_field.replace(['"', '\r', '\n'], "")
            )
            .as_bytes(),
        );
        body.extend_from_slice(csv.as_bytes());
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let v = Api::finish(
            self.agent
                .post(&self.upload_url)
                .header("Accept", "application/json")
                .header(
                    "Content-Type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .send(body),
        )?;
        v.get("url")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| Error::Protocol(format!("upload answer has no url: {v}")))
    }

    /// Estimates `server time - local time` in milliseconds.
    fn sync_time(&self, rounds: u32) -> Result<i64> {
        let mut samples: Vec<(i64, i64)> = Vec::new();
        for _ in 0..rounds.max(1) {
            let t0 = now_ms() as i64;
            let v = self.get("/servertime")?;
            let t1 = now_ms() as i64;
            let server = v
                .get("serverTime")
                .and_then(|s| s.as_i64().or_else(|| s.as_f64().map(|f| f.round() as i64)))
                .ok_or_else(|| Error::Protocol(format!("servertime answer: {v}")))?;
            let rtt = (t1 - t0).max(0);
            samples.push((rtt, server + rtt / 2 - t1));
        }
        samples.sort_by_key(|s| s.0);
        let best = &samples[..samples.len().div_ceil(2)];
        Ok(best.iter().map(|s| s.1).sum::<i64>() / best.len() as i64)
    }
}

enum Cmd {
    Load(Option<String>),
    Play {
        script_ms: i64,
        speed: f64,
        issued_ms: u64,
    },
    Stop,
    Move {
        pos: f32,
        duration_ms: u32,
    },
    Shutdown,
}

#[derive(Default)]
struct State {
    notice: Option<String>,
    last_error: Option<String>,
}

struct Shared {
    connected: AtomicBool,
    offset_ms: Mutex<i64>,
    state: Mutex<State>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Plain data; recover from poisoning.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The Handy over the cloud API (HSSP).
pub struct HandyDevice {
    config: HandyConfig,
    shared: Arc<Shared>,
    tx: mpsc::Sender<Cmd>,
    worker: Option<JoinHandle<()>>,
}

impl HandyDevice {
    /// Checks that the Handy is online, syncs the server clock and puts it
    /// in HSSP mode. Blocks for a few round trips; call it off the render
    /// thread.
    pub fn connect(config: HandyConfig) -> Result<HandyDevice> {
        if config.connection_key.trim().is_empty() {
            return Err(Error::Config("the Handy needs a connection key".into()));
        }
        let api = Api::new(&config);
        let v = api.get("/connected")?;
        if v.get("connected").and_then(Value::as_bool) != Some(true) {
            return Err(Error::NotConnected(
                "the Handy is not online; check its Wi-Fi and the connection key".into(),
            ));
        }
        let offset = api.sync_time(config.time_sync_rounds)?;
        api.put("/mode", &json!({ "mode": MODE_HSSP }))?;
        let shared = Arc::new(Shared {
            connected: AtomicBool::new(true),
            offset_ms: Mutex::new(offset),
            state: Mutex::new(State::default()),
        });
        let (tx, rx) = mpsc::channel();
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name("fp-haptics-handy".into())
            .spawn(move || Worker::new(api, worker_shared).run(rx))?;
        Ok(HandyDevice {
            config,
            shared,
            tx,
            worker: Some(worker),
        })
    }

    /// Estimated `server time - local time`, milliseconds.
    pub fn server_time_offset_ms(&self) -> i64 {
        *lock(&self.shared.offset_ms)
    }

    fn queue(&self, cmd: Cmd) -> Result<()> {
        self.tx
            .send(cmd)
            .map_err(|_| Error::NotConnected("Handy worker stopped".into()))
    }
}

struct Worker {
    api: Api,
    shared: Arc<Shared>,
    mode: Option<u8>,
    script_url: Option<String>,
    setup_done: bool,
}

impl Worker {
    fn new(api: Api, shared: Arc<Shared>) -> Worker {
        Worker {
            api,
            shared,
            mode: Some(MODE_HSSP),
            script_url: None,
            setup_done: false,
        }
    }

    fn run(mut self, rx: mpsc::Receiver<Cmd>) {
        while let Ok(first) = rx.recv() {
            let mut batch = vec![first];
            while let Ok(c) = rx.try_recv() {
                batch.push(c);
            }
            let shutdown = batch.iter().any(|c| matches!(c, Cmd::Shutdown));
            // Only the newest load and the newest motion command matter.
            let mut load = None;
            let mut motion = None;
            for c in batch {
                match c {
                    Cmd::Load(csv) => load = Some(csv),
                    Cmd::Shutdown => {}
                    other => motion = Some(other),
                }
            }
            if let Some(csv) = load {
                let r = self.load(csv);
                self.report(r);
            }
            if shutdown {
                if self.mode == Some(MODE_HSSP) && self.setup_done {
                    let _ = self.api.put("/hssp/stop", &json!({}));
                }
                break;
            }
            if let Some(m) = motion {
                let r = self.motion(m);
                self.report(r);
            }
        }
    }

    fn report(&self, r: Result<()>) {
        let mut st = lock(&self.shared.state);
        match r {
            Ok(()) => {
                st.last_error = None;
                self.shared.connected.store(true, Ordering::Relaxed);
            }
            Err(e) => {
                if matches!(e, Error::NotConnected(_) | Error::Io(_)) {
                    self.shared.connected.store(false, Ordering::Relaxed);
                }
                st.last_error = Some(e.to_string());
            }
        }
    }

    fn set_notice(&self, notice: Option<String>) {
        lock(&self.shared.state).notice = notice;
    }

    fn ensure_mode(&mut self, mode: u8) -> Result<()> {
        if self.mode != Some(mode) {
            self.mode = None;
            self.setup_done = false;
            self.api.put("/mode", &json!({ "mode": mode }))?;
            self.mode = Some(mode);
            // Leaving HSSP drops its setup; it is redone on the next play.
            self.setup_done = false;
        }
        Ok(())
    }

    fn setup(&mut self) -> Result<()> {
        if self.setup_done {
            return Ok(());
        }
        let Some(url) = self.script_url.clone() else {
            return Err(Error::Protocol("no script loaded".into()));
        };
        self.ensure_mode(MODE_HSSP)?;
        self.api.put("/hssp/setup", &json!({ "url": url }))?;
        self.setup_done = true;
        Ok(())
    }

    fn load(&mut self, csv: Option<String>) -> Result<()> {
        self.setup_done = false;
        self.script_url = None;
        let Some(csv) = csv else {
            if self.mode == Some(MODE_HSSP) {
                self.api.put("/hssp/stop", &json!({}))?;
            }
            return Ok(());
        };
        let url = self.api.upload(&csv)?;
        self.script_url = Some(url);
        self.setup()
    }

    fn motion(&mut self, cmd: Cmd) -> Result<()> {
        match cmd {
            Cmd::Play {
                script_ms,
                speed,
                issued_ms,
            } => {
                if (speed - 1.0).abs() > 0.01 {
                    self.set_notice(Some(format!(
                        "HSSP cannot play at {speed:.2}x; the Handy is paused until playback returns to 1x"
                    )));
                    if self.mode == Some(MODE_HSSP) && self.setup_done {
                        self.api.put("/hssp/stop", &json!({}))?;
                    }
                    return Ok(());
                }
                self.set_notice(None);
                self.setup()?;
                let now = now_ms();
                let start = (script_ms + now.saturating_sub(issued_ms) as i64).max(0);
                let offset = *lock(&self.shared.offset_ms);
                self.api.put(
                    "/hssp/play",
                    &json!({ "estimatedServerTime": now as i64 + offset, "startTime": start }),
                )?;
                Ok(())
            }
            Cmd::Stop => {
                if self.mode == Some(MODE_HSSP) && self.setup_done {
                    self.api.put("/hssp/stop", &json!({}))?;
                }
                Ok(())
            }
            Cmd::Move { pos, duration_ms } => {
                self.ensure_mode(MODE_HDSP)?;
                self.api.put(
                    "/hdsp/xpt",
                    &json!({
                        "position": (f64::from(pos.clamp(0.0, 1.0)) * 100.0 * 100.0).round() / 100.0,
                        "duration": duration_ms,
                        "stopOnTarget": true,
                        "immediateResponse": true,
                    }),
                )?;
                Ok(())
            }
            Cmd::Load(_) | Cmd::Shutdown => Ok(()),
        }
    }
}

impl Device for HandyDevice {
    fn name(&self) -> String {
        self.config.name.clone()
    }

    fn axes(&self) -> Vec<Axis> {
        vec![Axis::L0]
    }

    fn move_to(&mut self, axis: Axis, pos: f32, duration_ms: u32) -> Result<()> {
        if axis != Axis::L0 {
            return Err(Error::Unsupported(format!("the Handy has no {axis} axis")));
        }
        self.queue(Cmd::Move { pos, duration_ms })
    }

    fn stop(&mut self) -> Result<()> {
        self.queue(Cmd::Stop)
    }

    fn is_connected(&self) -> bool {
        self.shared.connected.load(Ordering::Relaxed)
    }

    fn min_interval_ms(&self) -> u32 {
        self.config.min_interval_ms
    }

    fn sync_mode(&self) -> SyncMode {
        SyncMode::Script
    }

    fn load_script(&mut self, scripts: &[(Axis, Script)]) -> Result<()> {
        let csv = scripts
            .iter()
            .find(|(a, _)| *a == Axis::L0)
            .map(|(_, s)| s.to_csv());
        self.queue(Cmd::Load(csv))
    }

    fn play_script(&mut self, script_time_ms: i64, speed: f64) -> Result<()> {
        self.queue(Cmd::Play {
            script_ms: script_time_ms,
            speed,
            issued_ms: now_ms(),
        })
    }

    fn notice(&self) -> Option<String> {
        let st = lock(&self.shared.state);
        st.notice.clone().or_else(|| st.last_error.clone())
    }
}

impl Drop for HandyDevice {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Shutdown);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::script::Action;
    use std::time::Instant;

    #[derive(Clone, Debug)]
    struct Req {
        method: String,
        url: String,
        body: String,
        key: Option<String>,
        content_type: Option<String>,
    }

    struct TestServer {
        base: String,
        log: Arc<Mutex<Vec<Req>>>,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(t) = self.thread.take() {
                t.join().unwrap();
            }
        }
    }

    /// A fake Handy API on 127.0.0.1. Server time runs 5 s ahead.
    fn server(online: bool, fail_play: bool) -> TestServer {
        let srv = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = srv.server_addr().to_ip().unwrap();
        let log: Arc<Mutex<Vec<Req>>> = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (log2, stop2) = (Arc::clone(&log), Arc::clone(&stop));
        let thread = std::thread::spawn(move || {
            while !stop2.load(Ordering::Relaxed) {
                let Ok(Some(mut req)) = srv.recv_timeout(Duration::from_millis(20)) else {
                    continue;
                };
                let mut body = String::new();
                req.as_reader().read_to_string(&mut body).unwrap();
                let header = |name: &str| {
                    req.headers()
                        .iter()
                        .find(|h| h.field.to_string().eq_ignore_ascii_case(name))
                        .map(|h| h.value.as_str().to_owned())
                };
                let r = Req {
                    method: req.method().to_string(),
                    url: req.url().to_owned(),
                    body,
                    key: header("X-Connection-Key"),
                    content_type: header("Content-Type"),
                };
                let path = r.url.trim_start_matches("/api/handy/v2");
                let (status, answer) = match path {
                    "/connected" => (200, json!({ "connected": online })),
                    "/servertime" => (200, json!({ "serverTime": now_ms() + 5000 })),
                    "/upload" => (200, json!({ "url": "http://scripts.test/abc.csv" })),
                    "/hssp/play" if fail_play => (
                        400,
                        json!({"error": {"code": 1001, "name": "DeviceNotConnected",
                                          "message": "Device not connected", "connected": false}}),
                    ),
                    "/mode" | "/hssp/setup" | "/hssp/play" | "/hssp/stop" | "/hdsp/xpt" => {
                        (200, json!({ "result": 0 }))
                    }
                    _ => (404, json!({ "error": { "message": "not found" } })),
                };
                lock(&log2).push(r);
                let resp = tiny_http::Response::from_string(answer.to_string())
                    .with_status_code(status)
                    .with_header(
                        "Content-Type: application/json"
                            .parse::<tiny_http::Header>()
                            .unwrap(),
                    );
                let _ = req.respond(resp);
            }
        });
        TestServer {
            base: format!("http://{addr}"),
            log,
            stop,
            thread: Some(thread),
        }
    }

    fn config(srv: &TestServer) -> HandyConfig {
        HandyConfig {
            connection_key: "KEY123".into(),
            api_base: format!("{}/api/handy/v2/", srv.base),
            upload_url: format!("{}/upload", srv.base),
            time_sync_rounds: 4,
            timeout_ms: 5000,
            use_system_proxy: false,
            ..Default::default()
        }
    }

    fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    fn paths(srv: &TestServer) -> Vec<String> {
        lock(&srv.log)
            .iter()
            .map(|r| format!("{} {}", r.method, r.url.trim_start_matches("/api/handy/v2")))
            .collect()
    }

    fn count(srv: &TestServer, p: &str) -> usize {
        paths(srv).iter().filter(|x| *x == p).count()
    }

    fn last_body(srv: &TestServer, p: &str) -> Value {
        let log = lock(&srv.log);
        let r = log
            .iter()
            .rev()
            .find(|r| format!("{} {}", r.method, r.url.trim_start_matches("/api/handy/v2")) == p)
            .unwrap();
        serde_json::from_str(&r.body).unwrap()
    }

    #[test]
    fn hssp_flow() {
        let srv = server(true, false);
        let mut dev = HandyDevice::connect(config(&srv)).unwrap();
        assert_eq!(
            paths(&srv),
            vec![
                "GET /connected",
                "GET /servertime",
                "GET /servertime",
                "GET /servertime",
                "GET /servertime",
                "PUT /mode"
            ]
        );
        assert_eq!(last_body(&srv, "PUT /mode"), json!({"mode": 1}));
        let off = dev.server_time_offset_ms();
        assert!((4800..=5200).contains(&off), "offset {off}");
        assert_eq!(dev.sync_mode(), SyncMode::Script);
        assert_eq!(dev.axes(), vec![Axis::L0]);

        let script = Script::new(vec![
            Action::new(0, 0.0),
            Action::new(500, 1.0),
            Action::new(1000, 0.5),
        ]);
        dev.load_script(&[(Axis::R1, script.clone()), (Axis::L0, script)])
            .unwrap();
        assert!(wait_for(|| count(&srv, "PUT /hssp/setup") == 1));
        {
            let log = lock(&srv.log);
            let up = log.iter().find(|r| r.url == "/upload").unwrap();
            assert_eq!(up.method, "POST");
            assert!(up.body.contains("name=\"syncFile\"; filename="));
            assert!(up.body.contains("\r\n\r\n0,0\n500,100\n1000,50\n\r\n--"));
            assert!(
                up.content_type
                    .as_deref()
                    .unwrap()
                    .starts_with("multipart/form-data; boundary=")
            );
            assert_eq!(up.key, None, "key is not sent to the upload host");
            for r in log.iter().filter(|r| r.url.starts_with("/api/")) {
                assert_eq!(r.key.as_deref(), Some("KEY123"), "{}", r.url);
            }
        }
        assert_eq!(
            last_body(&srv, "PUT /hssp/setup"),
            json!({"url": "http://scripts.test/abc.csv"})
        );

        dev.play_script(10_000, 1.0).unwrap();
        assert!(wait_for(|| count(&srv, "PUT /hssp/play") == 1));
        let play = last_body(&srv, "PUT /hssp/play");
        let start = play["startTime"].as_i64().unwrap();
        assert!((10_000..10_000 + 2000).contains(&start), "{play}");
        let est = play["estimatedServerTime"].as_i64().unwrap();
        let expect = now_ms() as i64 + 5000;
        assert!((est - expect).abs() < 2000, "{est} vs {expect}");
        assert!(dev.notice().is_none());
        assert!(dev.is_connected());

        // Other speeds stop the Handy and say why.
        dev.play_script(12_000, 1.5).unwrap();
        assert!(wait_for(|| count(&srv, "PUT /hssp/stop") == 1));
        assert!(wait_for(|| dev
            .notice()
            .is_some_and(|n| n.contains("1.50x"))));

        // HDSP move, then back to HSSP (mode switch + setup again).
        dev.move_to(Axis::L0, 0.25, 300).unwrap();
        assert!(wait_for(|| count(&srv, "PUT /hdsp/xpt") == 1));
        assert_eq!(last_body(&srv, "PUT /mode"), json!({"mode": 2}));
        let xpt = last_body(&srv, "PUT /hdsp/xpt");
        assert_eq!(xpt["position"], 25.0);
        assert_eq!(xpt["duration"], 300);
        assert!(dev.move_to(Axis::R0, 0.5, 100).is_err());

        dev.play_script(0, 1.0).unwrap();
        assert!(wait_for(|| count(&srv, "PUT /hssp/play") == 2));
        assert_eq!(last_body(&srv, "PUT /mode"), json!({"mode": 1}));
        assert_eq!(count(&srv, "PUT /hssp/setup"), 2);
        assert!(dev.notice().is_none());

        dev.stop().unwrap();
        assert!(wait_for(|| count(&srv, "PUT /hssp/stop") == 2));

        // Unloading the script stops playback; playing then reports it.
        dev.load_script(&[]).unwrap();
        assert!(wait_for(|| count(&srv, "PUT /hssp/stop") == 3));
        dev.play_script(0, 1.0).unwrap();
        assert!(wait_for(|| dev
            .notice()
            .is_some_and(|n| n.contains("no script loaded"))));
        drop(dev);
    }

    #[test]
    fn offline_and_api_errors() {
        let srv = server(false, false);
        assert!(matches!(
            HandyDevice::connect(config(&srv)),
            Err(Error::NotConnected(_))
        ));
        assert!(matches!(
            HandyDevice::connect(HandyConfig::default()),
            Err(Error::Config(_))
        ));
        drop(srv);

        let srv = server(true, true);
        let mut dev = HandyDevice::connect(config(&srv)).unwrap();
        dev.load_script(&[(Axis::L0, Script::new(vec![Action::new(0, 0.0)]))])
            .unwrap();
        dev.play_script(0, 1.0).unwrap();
        assert!(wait_for(|| !dev.is_connected()));
        assert!(dev.notice().unwrap().contains("Device not connected"));
        drop(dev);

        // Nothing listening.
        let mut cfg = config(&srv);
        drop(srv);
        cfg.api_base = "http://127.0.0.1:9/api".into();
        cfg.timeout_ms = 1000;
        assert!(HandyDevice::connect(cfg).is_err());
    }

    #[test]
    fn response_rules() {
        assert_eq!(check(200, "").unwrap(), Value::Null);
        assert!(check(200, r#"{"result":-1}"#).is_err());
        assert!(check(500, r#"{"result":0}"#).is_err());
        assert!(matches!(
            check(200, r#"{"error":{"message":"x","connected":false}}"#),
            Err(Error::NotConnected(m)) if m == "x"
        ));
        assert!(matches!(
            check(200, r#"{"error":"boom"}"#),
            Err(Error::Http(_))
        ));
        assert_eq!(check(200, "plain").unwrap(), Value::String("plain".into()));
    }
}
