//! The Web XR tab's browser: Chromium XR running without a visible window,
//! shown inside FramePlayer's panel and driven over the Chrome DevTools
//! Protocol.
//!
//! FramePlayer starts the browser itself, so Steam never shows its window;
//! the page reaches the panel as a screencast (JPEG frames) and the pointer,
//! wheel and keyboard go back as DevTools input events. The browser keeps
//! running while FramePlayer is away: it is started with
//! `--xr-host-handoff=<dir>` (FramePlayer's Chromium patch 0007), so when a
//! page enters VR it writes `xr-request` and waits for FramePlayer to release
//! the headset ([`xr_requested`], [`write_xr_ready`]); `frameplayer.sh` waits
//! for its `xr-ended`, then starts FramePlayer again, which reattaches to the
//! same browser through the profile's `DevToolsActivePort`.

use serde_json::{Value, json};
use std::io::ErrorKind;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use tungstenite::Message;

/// Exit code telling `frameplayer.sh` that a page in the web view has the
/// headset: wait for it to finish, then start FramePlayer again.
pub const YIELD_EXIT_CODE: i32 = 76;

/// Size of the page the browser lays out and streams.
pub const VIEW_W: u32 = 1280;
pub const VIEW_H: u32 = 720;

/// FramePlayer's Chromium XR build, under `$HOME`.
pub const CHROME: &str = "chromium-xr-frame/chromium/chrome";

/// Profile of the embedded browser (separate from the Chromium XR app's).
pub fn profile_dir() -> PathBuf {
    fp_core::dirs::data_dir().join("web")
}

/// Where the browser and FramePlayer hand the headset over (patch 0007).
pub fn handoff_dir() -> PathBuf {
    fp_core::dirs::data_dir().join("web-xr")
}

/// A page has asked for an immersive session and is waiting for the headset.
pub fn xr_requested() -> bool {
    handoff_dir().join("xr-request").exists()
}

/// Tells the browser FramePlayer has released the headset.
pub fn write_xr_ready() -> std::io::Result<()> {
    std::fs::write(handoff_dir().join("xr-ready"), "1")
}

/// One decoded screencast frame, RGBA.
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

#[derive(Default)]
struct Shared {
    frame: Option<Frame>,
    frame_seq: u64,
    url: String,
    connected: bool,
    error: Option<String>,
}

/// Keys forwarded to the page, besides typed text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Backspace,
    Delete,
    Enter,
    Tab,
    Escape,
    Left,
    Right,
    Up,
    Down,
}

impl Key {
    pub fn from_egui(k: egui::Key) -> Option<Key> {
        Some(match k {
            egui::Key::Backspace => Key::Backspace,
            egui::Key::Delete => Key::Delete,
            egui::Key::Enter => Key::Enter,
            egui::Key::Tab => Key::Tab,
            egui::Key::Escape => Key::Escape,
            egui::Key::ArrowLeft => Key::Left,
            egui::Key::ArrowRight => Key::Right,
            egui::Key::ArrowUp => Key::Up,
            egui::Key::ArrowDown => Key::Down,
            _ => return None,
        })
    }

    /// DOM `key`, `code` and Windows virtual key code, as DevTools wants them.
    fn dom(self) -> (&'static str, &'static str, u32) {
        match self {
            Key::Backspace => ("Backspace", "Backspace", 8),
            Key::Delete => ("Delete", "Delete", 46),
            Key::Enter => ("Enter", "Enter", 13),
            Key::Tab => ("Tab", "Tab", 9),
            Key::Escape => ("Escape", "Escape", 27),
            Key::Left => ("ArrowLeft", "ArrowLeft", 37),
            Key::Right => ("ArrowRight", "ArrowRight", 39),
            Key::Up => ("ArrowUp", "ArrowUp", 38),
            Key::Down => ("ArrowDown", "ArrowDown", 40),
        }
    }
}

/// The embedded browser.
pub struct WebView {
    tx: mpsc::Sender<String>,
    next_id: AtomicU64,
    shared: Arc<Mutex<Shared>>,
    /// Last frame uploaded to [`Self::texture`].
    shown_seq: u64,
    texture: Option<egui::TextureHandle>,
    last_mouse: Option<(i32, i32)>,
    buttons_down: bool,
}

impl WebView {
    /// Attaches to the embedded browser if it is still running (FramePlayer
    /// came back after a page's VR session), else starts it at `url`.
    pub fn start(home: &Path, url: &str) -> Result<WebView, String> {
        let profile = profile_dir();
        let handoff = handoff_dir();
        std::fs::create_dir_all(&profile).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(&handoff).map_err(|e| e.to_string())?;
        restrict_to_owner(&handoff);
        let port = match read_port(&profile).filter(|&p| reachable(p)) {
            Some(port) => {
                log::info!("web view: reattaching to the browser on port {port}");
                port
            }
            None => {
                let _ = std::fs::remove_file(profile.join("DevToolsActivePort"));
                for f in ["xr-request", "xr-ready", "xr-ended"] {
                    let _ = std::fs::remove_file(handoff.join(f));
                }
                spawn_browser(home, &profile, &handoff, url)?;
                wait_for_port(&profile, Duration::from_secs(20))?
            }
        };
        let ws_url = page_ws_url(port)?;
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (tx, rx) = mpsc::channel::<String>();
        let thread_shared = shared.clone();
        let thread_tx = tx.clone();
        std::thread::Builder::new()
            .name("web-view".into())
            .spawn(move || run(&ws_url, rx, thread_tx, thread_shared))
            .map_err(|e| e.to_string())?;
        let view = WebView {
            tx,
            next_id: AtomicU64::new(1),
            shared,
            shown_seq: 0,
            texture: None,
            last_mouse: None,
            buttons_down: false,
        };
        view.send("Page.enable", json!({}));
        view.send(
            "Emulation.setDeviceMetricsOverride",
            json!({ "width": VIEW_W, "height": VIEW_H, "deviceScaleFactor": 1, "mobile": false }),
        );
        view.send(
            "Page.startScreencast",
            json!({ "format": "jpeg", "quality": 75, "maxWidth": VIEW_W, "maxHeight": VIEW_H }),
        );
        Ok(view)
    }

    fn send(&self, method: &str, params: Value) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let _ = self
            .tx
            .send(json!({ "id": id, "method": method, "params": params }).to_string());
    }

    pub fn url(&self) -> String {
        self.shared
            .lock()
            .map(|s| s.url.clone())
            .unwrap_or_default()
    }

    pub fn error(&self) -> Option<String> {
        self.shared.lock().ok().and_then(|s| s.error.clone())
    }

    pub fn connected(&self) -> bool {
        self.shared.lock().is_ok_and(|s| s.connected)
    }

    /// Whether a screencast frame arrived since [`Self::texture`] last ran.
    pub fn fresh(&self) -> bool {
        self.shared
            .lock()
            .is_ok_and(|s| s.frame_seq != self.shown_seq)
    }

    /// The page as a texture, updated with the newest screencast frame.
    pub fn texture(&mut self, ctx: &egui::Context) -> Option<egui::TextureHandle> {
        let fresh = self.shared.lock().ok().and_then(|mut s| {
            (s.frame_seq != self.shown_seq)
                .then(|| (s.frame_seq, s.frame.take()))
                .and_then(|(seq, f)| f.map(|f| (seq, f)))
        });
        if let Some((seq, f)) = fresh {
            self.shown_seq = seq;
            let img = egui::ColorImage::from_rgba_unmultiplied([f.width, f.height], &f.rgba);
            match &mut self.texture {
                Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                None => {
                    self.texture =
                        Some(ctx.load_texture("web-view", img, egui::TextureOptions::LINEAR));
                }
            }
        }
        self.texture.clone()
    }

    pub fn navigate(&self, url: &str) {
        self.send("Page.navigate", json!({ "url": url }));
    }

    pub fn reload(&self) {
        self.send("Page.reload", json!({}));
    }

    pub fn back(&self) {
        self.send(
            "Runtime.evaluate",
            json!({ "expression": "history.back()" }),
        );
    }

    pub fn forward(&self) {
        self.send(
            "Runtime.evaluate",
            json!({ "expression": "history.forward()" }),
        );
    }

    /// Pointer at `(x, y)` in page pixels.
    pub fn mouse_move(&mut self, x: i32, y: i32) {
        if self.last_mouse == Some((x, y)) {
            return;
        }
        self.last_mouse = Some((x, y));
        let buttons = if self.buttons_down { 1 } else { 0 };
        self.send(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseMoved", "x": x, "y": y, "buttons": buttons }),
        );
    }

    pub fn mouse_button(&mut self, x: i32, y: i32, down: bool) {
        self.buttons_down = down;
        self.last_mouse = Some((x, y));
        let kind = if down {
            "mousePressed"
        } else {
            "mouseReleased"
        };
        self.send(
            "Input.dispatchMouseEvent",
            json!({ "type": kind, "x": x, "y": y, "button": "left",
                    "buttons": if down { 1 } else { 0 }, "clickCount": 1 }),
        );
    }

    /// Releases the button if a press is still held (the pointer may have
    /// left the page while it was down).
    pub fn mouse_release(&mut self, at: Option<(i32, i32)>) {
        if let Some((x, y)) = at.or(self.last_mouse).filter(|_| self.buttons_down) {
            self.mouse_button(x, y, false);
        }
    }

    pub fn wheel(&self, x: i32, y: i32, dx: f32, dy: f32) {
        self.send(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseWheel", "x": x, "y": y, "deltaX": dx, "deltaY": dy }),
        );
    }

    pub fn text(&self, text: &str) {
        self.send("Input.insertText", json!({ "text": text }));
    }

    pub fn key(&self, key: Key) {
        let (k, code, vk) = key.dom();
        let mut down = json!({ "type": "rawKeyDown", "key": k, "code": code,
                               "windowsVirtualKeyCode": vk });
        if key == Key::Enter {
            // "keyDown" with text, so forms submit as they do on a keyboard.
            down = json!({ "type": "keyDown", "key": k, "code": code,
                           "windowsVirtualKeyCode": vk, "text": "\r" });
        }
        self.send("Input.dispatchKeyEvent", down);
        self.send(
            "Input.dispatchKeyEvent",
            json!({ "type": "keyUp", "key": k, "code": code, "windowsVirtualKeyCode": vk }),
        );
    }

    /// Closes the browser (FramePlayer is quitting for good).
    pub fn close(&self) {
        if let Some(pid) = std::fs::read_to_string(profile_dir().join("frameplayer-browser.pid"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
        {
            let _ = std::process::Command::new("kill")
                .arg(pid.to_string())
                .status();
        }
        let _ = std::fs::remove_file(profile_dir().join("frameplayer-browser.pid"));
    }
}

/// Maps a point in a widget showing the page to page pixels.
pub fn to_page(pos: egui::Pos2, rect: egui::Rect) -> (i32, i32) {
    let u = ((pos.x - rect.min.x) / rect.width()).clamp(0.0, 1.0);
    let v = ((pos.y - rect.min.y) / rect.height()).clamp(0.0, 1.0);
    (
        (u * (VIEW_W as f32 - 1.0)).round() as i32,
        (v * (VIEW_H as f32 - 1.0)).round() as i32,
    )
}

fn restrict_to_owner(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

fn spawn_browser(home: &Path, profile: &Path, handoff: &Path, url: &str) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    let chrome = home.join(CHROME);
    if !chrome.exists() {
        return Err(format!(
            "Chromium XR is not installed ({} is missing)",
            chrome.display()
        ));
    }
    let mut cmd = std::process::Command::new(&chrome);
    cmd.args([
        format!("--user-data-dir={}", profile.display()),
        format!("--xr-host-handoff={}", handoff.display()),
        // The Linux OpenXR device is off by default.
        "--enable-features=OpenXR".into(),
        // Projection layers render black on the Frame (see docs/webxr).
        "--disable-blink-features=WebXRLayers".into(),
        // SteamVR refuses sessions with the seccomp filter on (docs/webxr).
        "--disable-seccomp-filter-sandbox".into(),
        "--ozone-platform=x11".into(),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--password-store=basic".into(),
        // The window is never on screen: keep rendering anyway.
        "--disable-backgrounding-occluded-windows".into(),
        "--disable-renderer-backgrounding".into(),
        "--disable-background-timer-throttling".into(),
        // Loopback only; the port is in the profile's DevToolsActivePort.
        "--remote-debugging-port=0".into(),
        format!("--window-size={VIEW_W},{VIEW_H}"),
        url.to_string(),
    ]);
    cmd.process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Steam preloads its overlay into everything it starts; it crashes
    // Chromium's zygote.
    if let Ok(pre) = std::env::var("LD_PRELOAD") {
        let keep: Vec<&str> = pre
            .split([' ', ':'])
            .filter(|l| !l.is_empty() && !l.ends_with("gameoverlayrenderer.so"))
            .collect();
        if keep.is_empty() {
            cmd.env_remove("LD_PRELOAD");
        } else {
            cmd.env("LD_PRELOAD", keep.join(":"));
        }
    }
    let child = cmd
        .spawn()
        .map_err(|e| format!("starting Chromium XR: {e}"))?;
    let _ = std::fs::write(
        profile.join("frameplayer-browser.pid"),
        child.id().to_string(),
    );
    log::info!(
        "web view: started Chromium XR (pid {}) at {url}",
        child.id()
    );
    Ok(())
}

/// The DevTools port from the profile's `DevToolsActivePort` (first line).
fn read_port(profile: &Path) -> Option<u16> {
    parse_port(&std::fs::read_to_string(profile.join("DevToolsActivePort")).ok()?)
}

fn parse_port(text: &str) -> Option<u16> {
    text.lines().next()?.trim().parse().ok().filter(|&p| p != 0)
}

fn reachable(port: u16) -> bool {
    TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(300),
    )
    .is_ok()
}

fn wait_for_port(profile: &Path, timeout: Duration) -> Result<u16, String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(p) = read_port(profile).filter(|&p| reachable(p)) {
            return Ok(p);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err("Chromium XR did not start".into())
}

/// The WebSocket address of the first page target.
fn page_ws_url(port: u16) -> Result<String, String> {
    let body: String = ureq::get(&format!("http://127.0.0.1:{port}/json/list"))
        .call()
        .map_err(|e| format!("DevTools: {e}"))?
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("DevTools: {e}"))?;
    first_page_ws(&body).ok_or_else(|| "the browser has no page".into())
}

fn first_page_ws(json_list: &str) -> Option<String> {
    let list: Value = serde_json::from_str(json_list).ok()?;
    list.as_array()?
        .iter()
        .find(|t| t["type"] == "page")
        .and_then(|t| t["webSocketDebuggerUrl"].as_str())
        .map(str::to_string)
}

/// Message ids for screencast acks, apart from [`WebView::send`]'s. DevTools
/// rejects ids that don't fit a 32-bit int ("Message must have integer 'id'
/// property"), and without acks the screencast stops after a few pictures.
const FIRST_ACK_ID: u64 = 1 << 30;

fn next_ack_id(id: u64) -> u64 {
    if id >= i32::MAX as u64 {
        FIRST_ACK_ID
    } else {
        id + 1
    }
}

/// The connection thread: sends queued commands, decodes screencast frames
/// and keeps the page's address.
fn run(
    url: &str,
    rx: mpsc::Receiver<String>,
    tx: mpsc::Sender<String>,
    shared: Arc<Mutex<Shared>>,
) {
    let fail = |msg: String| {
        log::warn!("web view: {msg}");
        if let Ok(mut s) = shared.lock() {
            s.connected = false;
            s.error = Some(msg);
        }
    };
    let host = url
        .strip_prefix("ws://")
        .and_then(|r| r.split('/').next())
        .unwrap_or("127.0.0.1:0");
    let stream = match TcpStream::connect(host) {
        Ok(s) => s,
        Err(e) => return fail(format!("connecting to the browser: {e}")),
    };
    let _ = stream.set_nodelay(true);
    let mut ws = match tungstenite::client(url, stream) {
        Ok((ws, _)) => ws,
        Err(e) => return fail(format!("DevTools handshake: {e}")),
    };
    let _ = ws
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(5)));
    if let Ok(mut s) = shared.lock() {
        s.connected = true;
        s.error = None;
    }
    let mut ack_id = FIRST_ACK_ID;
    loop {
        loop {
            match rx.try_recv() {
                Ok(cmd) => {
                    if let Err(e) = ws.send(Message::text(cmd)) {
                        return fail(format!("sending to the browser: {e}"));
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        }
        match ws.read() {
            Ok(Message::Text(t)) => {
                let Ok(v) = serde_json::from_str::<Value>(t.as_str()) else {
                    continue;
                };
                match v["method"].as_str() {
                    Some("Page.screencastFrame") => {
                        let p = &v["params"];
                        ack_id = next_ack_id(ack_id);
                        let id = ack_id;
                        let _ = tx.send(
                            json!({ "id": id, "method": "Page.screencastFrameAck",
                                    "params": { "sessionId": p["sessionId"] } })
                            .to_string(),
                        );
                        if let Some(frame) = p["data"].as_str().and_then(decode_frame)
                            && let Ok(mut s) = shared.lock()
                        {
                            s.frame = Some(frame);
                            s.frame_seq += 1;
                        }
                    }
                    Some("Page.frameNavigated") => {
                        let f = &v["params"]["frame"];
                        if f.get("parentId").is_none()
                            && let Some(u) = f["url"].as_str()
                            && let Ok(mut s) = shared.lock()
                        {
                            s.url = u.to_string();
                        }
                    }
                    _ => {
                        if let Some(err) = v.get("error") {
                            log::debug!("web view: DevTools error {err}");
                        }
                    }
                }
            }
            Ok(Message::Close(_)) => return fail("the browser closed".into()),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => return fail(format!("browser connection: {e}")),
        }
    }
}

fn decode_frame(b64: &str) -> Option<Frame> {
    use base64::Engine as _;
    let jpeg = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    let img = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg)
        .ok()?
        .to_rgba8();
    Some(Frame {
        width: img.width() as usize,
        height: img.height() as usize,
        rgba: img.into_raw(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ack_ids_fit_devtools_int() {
        assert!(FIRST_ACK_ID < i32::MAX as u64);
        assert_eq!(next_ack_id(FIRST_ACK_ID), FIRST_ACK_ID + 1);
        assert_eq!(next_ack_id(i32::MAX as u64), FIRST_ACK_ID);
    }

    #[test]
    fn devtools_port_file() {
        assert_eq!(parse_port("41723\n/devtools/browser/abc\n"), Some(41723));
        assert_eq!(parse_port("0\n"), None);
        assert_eq!(parse_port(""), None);
        assert_eq!(parse_port("x"), None);
    }

    #[test]
    fn picks_the_page_target() {
        let list = r#"[
            {"type": "iframe", "webSocketDebuggerUrl": "ws://127.0.0.1:1/devtools/page/F"},
            {"type": "page", "webSocketDebuggerUrl": "ws://127.0.0.1:1/devtools/page/P"}
        ]"#;
        assert_eq!(
            first_page_ws(list).as_deref(),
            Some("ws://127.0.0.1:1/devtools/page/P")
        );
        assert_eq!(first_page_ws("[]"), None);
    }

    #[test]
    fn widget_points_map_to_page_pixels() {
        let rect = egui::Rect::from_min_size(egui::pos2(100.0, 50.0), egui::vec2(640.0, 360.0));
        assert_eq!(to_page(rect.min, rect), (0, 0));
        assert_eq!(
            to_page(rect.max, rect),
            (VIEW_W as i32 - 1, VIEW_H as i32 - 1)
        );
        assert_eq!(to_page(rect.center(), rect), (640, 360));
        // Outside the widget clamps to the edge.
        assert_eq!(to_page(egui::pos2(0.0, 0.0), rect), (0, 0));
    }

    #[test]
    fn keys_map_to_dom_codes() {
        assert_eq!(Key::from_egui(egui::Key::Enter), Some(Key::Enter));
        assert_eq!(Key::from_egui(egui::Key::A), None);
        assert_eq!(Key::Backspace.dom(), ("Backspace", "Backspace", 8));
    }
}
