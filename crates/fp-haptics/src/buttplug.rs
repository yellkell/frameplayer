//! Buttplug.io devices through Intiface Central (or any Buttplug server)
//! over WebSocket, message protocol version 3.
//!
//! One [`ButtplugDevice`] is one server connection and drives every device
//! the server exposes:
//!
//! - `LinearCmd` features: feature 0 follows L0, features 1 and 2 follow L1
//!   and L2 (`Vectors` with `Index`, `Duration`, `Position`);
//! - `ScalarCmd` features with `ActuatorType` `Vibrate` or `Oscillate`
//!   follow V0, `Inflate` and `Constrict` follow V1 (`Scalar` = position);
//! - `RotateCmd` features follow R0, read as a speed around the centre:
//!   0.5 is still, 1.0 full speed clockwise, 0.0 full speed anticlockwise.
//!
//! Connection: `RequestServerInfo` (`MessageVersion` 3), then
//! `StartScanning` and `RequestDeviceList`. `DeviceAdded` and
//! `DeviceRemoved` keep the device list current. When the server sets
//! `MaxPingTime`, a `Ping` is sent at half that interval. `stop` sends
//! `StopDeviceCmd` to every device.
//!
//! Only plain `ws://` is supported (Intiface listens on
//! `ws://127.0.0.1:12345` by default); `wss://` returns
//! [`Error::Unsupported`]. After the connection drops the device reports
//! disconnected; create a new one to reconnect.
//!
//! All I/O runs on a dedicated thread; the [`Device`] methods only queue
//! messages.

use crate::axis::Axis;
use crate::device::{AxisMove, Device};
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tungstenite::{Message, WebSocket};

/// Intiface Central's default address.
pub const DEFAULT_URL: &str = "ws://127.0.0.1:12345";

/// Protocol message version we speak.
pub const MESSAGE_VERSION: u32 = 3;

/// Configuration of a [`ButtplugDevice`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ButtplugConfig {
    /// Server URL.
    pub url: String,
    /// Client name shown in Intiface.
    pub client_name: String,
    /// Display name of this device in FramePlayer (also the settings key).
    pub name: String,
    /// Minimum time between commands, milliseconds.
    pub min_interval_ms: u32,
    /// Ask the server to scan for devices after connecting.
    pub scan: bool,
    /// Connection and handshake timeout, milliseconds.
    pub timeout_ms: u64,
}

impl Default for ButtplugConfig {
    fn default() -> Self {
        ButtplugConfig {
            url: DEFAULT_URL.into(),
            client_name: "FramePlayer".into(),
            name: "Intiface".into(),
            min_interval_ms: 50,
            scan: true,
            timeout_ms: 5000,
        }
    }
}

/// One actuator of a server device.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ButtplugFeature {
    /// Index within the device's attribute list for this message type.
    pub index: u32,
    /// `ActuatorType` (`Vibrate`, `Position`, `Rotate`, ...).
    pub actuator_type: String,
    /// `StepCount`, 0 when unknown.
    pub step_count: u32,
    /// `FeatureDescriptor`.
    pub descriptor: String,
}

/// A device the Buttplug server reports.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ButtplugServerDevice {
    /// `DeviceIndex`.
    pub index: u32,
    /// `DeviceName`.
    pub name: String,
    /// `DeviceDisplayName`, if the user set one in Intiface.
    pub display_name: Option<String>,
    /// `LinearCmd` features.
    pub linear: Vec<ButtplugFeature>,
    /// `ScalarCmd` features.
    pub scalar: Vec<ButtplugFeature>,
    /// `RotateCmd` features.
    pub rotate: Vec<ButtplugFeature>,
}

impl ButtplugServerDevice {
    fn from_json(v: &Value) -> Option<ButtplugServerDevice> {
        let index = v.get("DeviceIndex")?.as_u64()? as u32;
        let msgs = v.get("DeviceMessages");
        let features = |kind: &str| -> Vec<ButtplugFeature> {
            msgs.and_then(|m| m.get(kind))
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .enumerate()
                        .map(|(i, f)| ButtplugFeature {
                            index: i as u32,
                            actuator_type: f
                                .get("ActuatorType")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                            step_count: f.get("StepCount").and_then(Value::as_u64).unwrap_or(0)
                                as u32,
                            descriptor: f
                                .get("FeatureDescriptor")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned(),
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        Some(ButtplugServerDevice {
            index,
            name: v
                .get("DeviceName")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            display_name: v
                .get("DeviceDisplayName")
                .and_then(Value::as_str)
                .map(str::to_owned),
            linear: features("LinearCmd"),
            scalar: features("ScalarCmd"),
            rotate: features("RotateCmd"),
        })
    }

    /// The axes this device responds to.
    pub fn axes(&self) -> Vec<Axis> {
        let mut axes = Vec::new();
        for f in &self.linear {
            if let Some(a) = linear_axis(f.index) {
                axes.push(a);
            }
        }
        for f in &self.scalar {
            if let Some(a) = scalar_axis(&f.actuator_type) {
                axes.push(a);
            }
        }
        if !self.rotate.is_empty() {
            axes.push(Axis::R0);
        }
        axes.sort();
        axes.dedup();
        axes
    }

    /// Messages that perform `moves` on this device (empty when none
    /// apply). Ids are assigned by the caller.
    fn commands(&self, moves: &[AxisMove]) -> Vec<(&'static str, Value)> {
        let find = |axis: Axis| moves.iter().find(|m| m.axis == axis);
        let mut out = Vec::new();
        let vectors: Vec<Value> = self
            .linear
            .iter()
            .filter_map(|f| {
                let m = find(linear_axis(f.index)?)?;
                Some(json!({"Index": f.index, "Duration": m.duration_ms, "Position": f64::from(m.pos)}))
            })
            .collect();
        if !vectors.is_empty() {
            out.push((
                "LinearCmd",
                json!({"DeviceIndex": self.index, "Vectors": vectors}),
            ));
        }
        let scalars: Vec<Value> = self
            .scalar
            .iter()
            .filter_map(|f| {
                let m = find(scalar_axis(&f.actuator_type)?)?;
                Some(json!({"Index": f.index, "Scalar": f64::from(m.pos), "ActuatorType": f.actuator_type}))
            })
            .collect();
        if !scalars.is_empty() {
            out.push((
                "ScalarCmd",
                json!({"DeviceIndex": self.index, "Scalars": scalars}),
            ));
        }
        if let Some(m) = find(Axis::R0) {
            let rotations: Vec<Value> = self
                .rotate
                .iter()
                .map(|f| {
                    let v = (f64::from(m.pos) - 0.5) * 2.0;
                    json!({"Index": f.index, "Speed": v.abs().min(1.0), "Clockwise": v >= 0.0})
                })
                .collect();
            if !rotations.is_empty() {
                out.push((
                    "RotateCmd",
                    json!({"DeviceIndex": self.index, "Rotations": rotations}),
                ));
            }
        }
        out
    }
}

fn linear_axis(index: u32) -> Option<Axis> {
    [Axis::L0, Axis::L1, Axis::L2].get(index as usize).copied()
}

fn scalar_axis(actuator: &str) -> Option<Axis> {
    match actuator {
        "Vibrate" | "Oscillate" => Some(Axis::V0),
        "Inflate" | "Constrict" => Some(Axis::V1),
        _ => None,
    }
}

struct Shared {
    connected: AtomicBool,
    server_name: Mutex<String>,
    devices: Mutex<BTreeMap<u32, ButtplugServerDevice>>,
    last_error: Mutex<Option<String>>,
    next_id: AtomicU32,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Plain data; recover from poisoning.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    fn id(&self) -> u32 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Wraps `(type, fields)` pairs into one message array, assigning ids.
    fn frame(&self, msgs: Vec<(&str, Value)>) -> String {
        let arr: Vec<Value> = msgs
            .into_iter()
            .map(|(kind, mut body)| {
                if let Value::Object(o) = &mut body {
                    o.insert("Id".into(), json!(self.id()));
                }
                let mut wrapper = serde_json::Map::new();
                wrapper.insert(kind.to_owned(), body);
                Value::Object(wrapper)
            })
            .collect();
        Value::Array(arr).to_string()
    }

    /// Applies a server message; returns its (type, body).
    fn handle<'a>(&self, msg: &'a Value) -> Option<(&'a str, &'a Value)> {
        let (kind, body) = msg.as_object()?.iter().next()?;
        match kind.as_str() {
            "DeviceAdded" => {
                if let Some(d) = ButtplugServerDevice::from_json(body) {
                    lock(&self.devices).insert(d.index, d);
                }
            }
            "DeviceRemoved" => {
                if let Some(i) = body.get("DeviceIndex").and_then(Value::as_u64) {
                    lock(&self.devices).remove(&(i as u32));
                }
            }
            "DeviceList" => {
                let mut devices = lock(&self.devices);
                devices.clear();
                for d in body
                    .get("Devices")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(ButtplugServerDevice::from_json)
                {
                    devices.insert(d.index, d);
                }
            }
            "Error" => {
                let msg = body
                    .get("ErrorMessage")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                *lock(&self.last_error) = Some(msg.to_owned());
            }
            _ => {}
        }
        Some((kind.as_str(), body))
    }
}

/// A cloneable view of a Buttplug connection for the UI (server devices,
/// state), usable after the [`ButtplugDevice`] has been handed to the
/// engine.
#[derive(Clone)]
pub struct ButtplugHandle(Arc<Shared>);

impl ButtplugHandle {
    /// Whether the connection is open.
    pub fn is_connected(&self) -> bool {
        self.0.connected.load(Ordering::Relaxed)
    }

    /// The server's `ServerName`.
    pub fn server_name(&self) -> String {
        lock(&self.0.server_name).clone()
    }

    /// Devices the server currently reports.
    pub fn server_devices(&self) -> Vec<ButtplugServerDevice> {
        lock(&self.0.devices).values().cloned().collect()
    }

    /// The last error the server sent or the connection hit.
    pub fn last_error(&self) -> Option<String> {
        lock(&self.0.last_error).clone()
    }
}

enum Outgoing {
    Text(String),
    Shutdown,
}

/// A connection to a Buttplug server, driving all of its devices.
pub struct ButtplugDevice {
    config: ButtplugConfig,
    shared: Arc<Shared>,
    tx: mpsc::Sender<Outgoing>,
    worker: Option<JoinHandle<()>>,
}

type Socket = WebSocket<TcpStream>;

fn is_timeout(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io) if matches!(io.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut))
}

fn send_text(ws: &mut Socket, text: String) -> Result<()> {
    ws.send(Message::text(text))?;
    Ok(())
}

/// Reads server messages until `want(type, body)` returns `Some`, or the
/// deadline passes.
fn wait_for<T>(
    ws: &mut Socket,
    shared: &Shared,
    deadline: Instant,
    mut want: impl FnMut(&str, &Value) -> Option<Result<T>>,
) -> Result<T> {
    loop {
        if Instant::now() >= deadline {
            return Err(Error::Timeout("Buttplug server did not answer".into()));
        }
        let text = match ws.read() {
            Ok(Message::Text(t)) => t,
            Ok(Message::Close(_)) => {
                return Err(Error::WebSocket("server closed the connection".into()));
            }
            Ok(_) => continue,
            Err(e) if is_timeout(&e) => continue,
            Err(e) => return Err(e.into()),
        };
        let parsed: Value = serde_json::from_str(text.as_str())?;
        for msg in parsed.as_array().into_iter().flatten() {
            if let Some((kind, body)) = shared.handle(msg) {
                if let Some(r) = want(kind, body) {
                    return r;
                }
            }
        }
    }
}

impl ButtplugDevice {
    /// Connects, performs the handshake, starts scanning (if configured)
    /// and fetches the device list.
    pub fn connect(config: ButtplugConfig) -> Result<ButtplugDevice> {
        let uri: tungstenite::http::Uri = config
            .url
            .parse()
            .map_err(|e| Error::Config(format!("bad Buttplug URL {:?}: {e}", config.url)))?;
        match uri.scheme_str() {
            Some("ws") => {}
            Some("wss") => {
                return Err(Error::Unsupported(
                    "wss:// (TLS) Buttplug servers; use ws://".into(),
                ));
            }
            _ => {
                return Err(Error::Config(format!(
                    "{:?} is not a ws:// URL",
                    config.url
                )));
            }
        }
        let host = uri
            .host()
            .ok_or_else(|| Error::Config(format!("{:?} has no host", config.url)))?;
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let port = uri.port_u16().unwrap_or(80);
        let timeout = Duration::from_millis(config.timeout_ms.max(100));
        let addr = (host, port)
            .to_socket_addrs()
            .map_err(|e| Error::Config(format!("cannot resolve {host}: {e}")))?
            .next()
            .ok_or_else(|| Error::Config(format!("{host} resolves to no address")))?;
        let stream = TcpStream::connect_timeout(&addr, timeout)?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let (mut ws, _) = tungstenite::client(config.url.as_str(), stream)
            .map_err(|e| Error::WebSocket(format!("handshake with {}: {e}", config.url)))?;

        let shared = Arc::new(Shared {
            connected: AtomicBool::new(true),
            server_name: Mutex::new(String::new()),
            devices: Mutex::new(BTreeMap::new()),
            last_error: Mutex::new(None),
            next_id: AtomicU32::new(1),
        });
        let deadline = Instant::now() + timeout;

        let info_id = shared.next_id.load(Ordering::Relaxed);
        let frame = shared.frame(vec![(
            "RequestServerInfo",
            json!({"ClientName": config.client_name, "MessageVersion": MESSAGE_VERSION}),
        )]);
        send_text(&mut ws, frame)?;
        let max_ping = wait_for(&mut ws, &shared, deadline, |kind, body| {
            let id = body.get("Id").and_then(Value::as_u64);
            match kind {
                "ServerInfo" => {
                    *lock(&shared.server_name) = body
                        .get("ServerName")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    Some(Ok(body
                        .get("MaxPingTime")
                        .and_then(Value::as_u64)
                        .unwrap_or(0)))
                }
                "Error" if id == Some(u64::from(info_id)) || id == Some(0) => Some(Err(
                    Error::Protocol(format!("server refused handshake: {body}")),
                )),
                _ => None,
            }
        })?;

        let mut msgs = Vec::new();
        if config.scan {
            msgs.push(("StartScanning", json!({})));
        }
        let list_id = shared.next_id.load(Ordering::Relaxed) + msgs.len() as u32;
        msgs.push(("RequestDeviceList", json!({})));
        let frame = shared.frame(msgs);
        send_text(&mut ws, frame)?;
        wait_for(&mut ws, &shared, deadline, |kind, body| {
            let id = body.get("Id").and_then(Value::as_u64);
            match kind {
                "DeviceList" => Some(Ok(())),
                "Error" if id == Some(u64::from(list_id)) => Some(Err(Error::Protocol(format!(
                    "device list request failed: {body}"
                )))),
                _ => None,
            }
        })?;

        // Short read timeout from here on: the I/O loop alternates between
        // sending queued commands and reading server events.
        ws.get_ref()
            .set_read_timeout(Some(Duration::from_millis(5)))?;
        let (tx, rx) = mpsc::channel();
        let worker_shared = Arc::clone(&shared);
        let ping_every = (max_ping > 0).then(|| Duration::from_millis((max_ping / 2).max(1)));
        let worker = std::thread::Builder::new()
            .name("fp-haptics-buttplug".into())
            .spawn(move || io_loop(ws, rx, worker_shared, ping_every))?;
        Ok(ButtplugDevice {
            config,
            shared,
            tx,
            worker: Some(worker),
        })
    }

    /// A handle for the UI that stays valid after this device is moved into
    /// the engine.
    pub fn handle(&self) -> ButtplugHandle {
        ButtplugHandle(Arc::clone(&self.shared))
    }

    fn queue(&self, msgs: Vec<(&str, Value)>) -> Result<()> {
        if msgs.is_empty() {
            return Ok(());
        }
        if !self.is_connected() {
            return Err(Error::NotConnected(self.config.url.clone()));
        }
        let frame = self.shared.frame(msgs);
        self.tx
            .send(Outgoing::Text(frame))
            .map_err(|_| Error::NotConnected(self.config.url.clone()))
    }
}

fn io_loop(
    mut ws: Socket,
    rx: mpsc::Receiver<Outgoing>,
    shared: Arc<Shared>,
    ping_every: Option<Duration>,
) {
    let mut last_ping = Instant::now();
    let fail = |e: String| {
        *lock(&shared.last_error) = Some(e);
    };
    'run: loop {
        loop {
            match rx.try_recv() {
                Ok(Outgoing::Text(t)) => {
                    if let Err(e) = send_text(&mut ws, t) {
                        fail(e.to_string());
                        break 'run;
                    }
                }
                Ok(Outgoing::Shutdown) | Err(TryRecvError::Disconnected) => {
                    let _ = ws.close(None);
                    let _ = ws.flush();
                    break 'run;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        if let Some(every) = ping_every {
            if last_ping.elapsed() >= every {
                last_ping = Instant::now();
                let frame = shared.frame(vec![("Ping", json!({}))]);
                if let Err(e) = send_text(&mut ws, frame) {
                    fail(e.to_string());
                    break 'run;
                }
            }
        }
        match ws.read() {
            Ok(Message::Text(t)) => match serde_json::from_str::<Value>(t.as_str()) {
                Ok(v) => {
                    for msg in v.as_array().into_iter().flatten() {
                        shared.handle(msg);
                    }
                }
                Err(e) => fail(format!("bad message from server: {e}")),
            },
            Ok(Message::Close(_)) => break 'run,
            Ok(_) => {}
            Err(e) if is_timeout(&e) => {}
            Err(e) => {
                fail(e.to_string());
                break 'run;
            }
        }
    }
    shared.connected.store(false, Ordering::Relaxed);
}

impl Device for ButtplugDevice {
    fn name(&self) -> String {
        self.config.name.clone()
    }

    fn axes(&self) -> Vec<Axis> {
        let mut axes: Vec<Axis> = lock(&self.shared.devices)
            .values()
            .flat_map(ButtplugServerDevice::axes)
            .collect();
        axes.sort();
        axes.dedup();
        axes
    }

    fn move_to(&mut self, axis: Axis, pos: f32, duration_ms: u32) -> Result<()> {
        self.move_axes(&[AxisMove {
            axis,
            pos,
            duration_ms,
        }])
    }

    fn move_axes(&mut self, moves: &[AxisMove]) -> Result<()> {
        let msgs: Vec<(&str, Value)> = lock(&self.shared.devices)
            .values()
            .flat_map(|d| d.commands(moves))
            .collect();
        self.queue(msgs)
    }

    fn stop(&mut self) -> Result<()> {
        let msgs: Vec<(&str, Value)> = lock(&self.shared.devices)
            .keys()
            .map(|i| ("StopDeviceCmd", json!({"DeviceIndex": i})))
            .collect();
        self.queue(msgs)
    }

    fn is_connected(&self) -> bool {
        self.shared.connected.load(Ordering::Relaxed)
    }

    fn min_interval_ms(&self) -> u32 {
        self.config.min_interval_ms
    }

    fn notice(&self) -> Option<String> {
        if !self.is_connected() {
            return lock(&self.shared.last_error)
                .clone()
                .or_else(|| Some("disconnected".into()));
        }
        let devices = lock(&self.shared.devices);
        if devices.is_empty() {
            Some("no devices; turn one on and let Intiface scan".into())
        } else {
            Some(
                devices
                    .values()
                    .map(|d| d.display_name.clone().unwrap_or_else(|| d.name.clone()))
                    .collect::<Vec<_>>()
                    .join(", "),
            )
        }
    }
}

impl Drop for ButtplugDevice {
    fn drop(&mut self) {
        let _ = self.tx.send(Outgoing::Shutdown);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    type Log = Arc<Mutex<Vec<Value>>>;

    fn linear_device(index: u32, name: &str) -> Value {
        json!({"DeviceName": name, "DeviceIndex": index, "DeviceMessageTimingGap": 0,
               "DeviceMessages": {"LinearCmd": [{"StepCount": 100, "FeatureDescriptor": "", "ActuatorType": "Position"}],
                                  "StopDeviceCmd": {}}})
    }

    /// A minimal Buttplug v3 server: answers the handshake, lists one
    /// linear device, records every client message, and runs `script`
    /// actions on its socket after the device list is sent.
    fn server(max_ping: u64, after_list: Vec<Value>) -> (String, Log, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let log2 = Arc::clone(&log);
        let h = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            let mut after = Some(after_list);
            loop {
                let text = match ws.read() {
                    Ok(Message::Text(t)) => t,
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(_) => continue,
                };
                let v: Value = serde_json::from_str(text.as_str()).unwrap();
                for msg in v.as_array().unwrap() {
                    lock(&log2).push(msg.clone());
                    let (kind, body) = msg.as_object().unwrap().iter().next().unwrap();
                    let id = body["Id"].clone();
                    let reply = match kind.as_str() {
                        "RequestServerInfo" => {
                            assert_eq!(body["MessageVersion"], 3);
                            json!([{"ServerInfo": {"Id": id, "ServerName": "Test Server",
                                    "MessageVersion": 3, "MaxPingTime": max_ping}}])
                        }
                        "RequestDeviceList" => json!([{"DeviceList": {"Id": id,
                            "Devices": [linear_device(0, "Stroker")]}}]),
                        "Bogus" => {
                            json!([{"Error": {"Id": id, "ErrorMessage": "nope", "ErrorCode": 3}}])
                        }
                        _ => json!([{"Ok": {"Id": id}}]),
                    };
                    ws.send(Message::text(reply.to_string())).unwrap();
                    if kind == "RequestDeviceList" {
                        for m in after.take().unwrap_or_default() {
                            ws.send(Message::text(json!([m]).to_string())).unwrap();
                        }
                    }
                }
            }
        });
        (url, log, h)
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

    fn kinds(log: &Log) -> Vec<String> {
        lock(log)
            .iter()
            .map(|m| m.as_object().unwrap().keys().next().unwrap().clone())
            .collect()
    }

    #[test]
    fn handshake_and_linear_commands() {
        let vibe = json!({"DeviceAdded": {"Id": 0, "DeviceName": "Vibe", "DeviceIndex": 3,
            "DeviceMessages": {"ScalarCmd": [{"StepCount": 20, "FeatureDescriptor": "", "ActuatorType": "Vibrate"},
                                             {"StepCount": 20, "FeatureDescriptor": "", "ActuatorType": "Constrict"}],
                               "RotateCmd": [{"StepCount": 20, "FeatureDescriptor": ""}]}}});
        let (url, log, server) = server(0, vec![vibe]);
        let mut dev = ButtplugDevice::connect(ButtplugConfig {
            url,
            ..Default::default()
        })
        .unwrap();
        let handle = dev.handle();
        assert_eq!(handle.server_name(), "Test Server");
        assert!(dev.is_connected());
        assert_eq!(
            kinds(&log),
            vec!["RequestServerInfo", "StartScanning", "RequestDeviceList"]
        );
        // The DeviceAdded event arrives through the I/O loop.
        assert!(wait_for(|| handle.server_devices().len() == 2));
        assert_eq!(dev.axes(), vec![Axis::L0, Axis::R0, Axis::V0, Axis::V1]);
        assert!(dev.notice().unwrap().contains("Stroker"));

        dev.move_to(Axis::L0, 0.3, 500).unwrap();
        assert!(wait_for(|| kinds(&log).contains(&"LinearCmd".to_string())));
        let lin = lock(&log)
            .iter()
            .find_map(|m| m.get("LinearCmd").cloned())
            .unwrap();
        assert_eq!(lin["DeviceIndex"], 0);
        assert_eq!(lin["Vectors"][0]["Index"], 0);
        assert_eq!(lin["Vectors"][0]["Duration"], 500);
        assert!((lin["Vectors"][0]["Position"].as_f64().unwrap() - 0.3).abs() < 1e-6);
        assert!(lin["Id"].as_u64().unwrap() > 0);

        dev.move_axes(&[
            AxisMove {
                axis: Axis::V0,
                pos: 0.75,
                duration_ms: 100,
            },
            AxisMove {
                axis: Axis::R0,
                pos: 0.25,
                duration_ms: 100,
            },
        ])
        .unwrap();
        assert!(wait_for(|| kinds(&log).contains(&"RotateCmd".to_string())));
        let scalar = lock(&log)
            .iter()
            .find_map(|m| m.get("ScalarCmd").cloned())
            .unwrap();
        assert_eq!(scalar["DeviceIndex"], 3);
        assert_eq!(scalar["Scalars"].as_array().unwrap().len(), 1);
        assert_eq!(scalar["Scalars"][0]["ActuatorType"], "Vibrate");
        assert_eq!(scalar["Scalars"][0]["Scalar"], 0.75);
        let rot = lock(&log)
            .iter()
            .find_map(|m| m.get("RotateCmd").cloned())
            .unwrap();
        assert_eq!(rot["Rotations"][0]["Speed"], 0.5);
        assert_eq!(rot["Rotations"][0]["Clockwise"], false);

        dev.stop().unwrap();
        assert!(wait_for(|| kinds(&log)
            .iter()
            .filter(|k| *k == "StopDeviceCmd")
            .count()
            == 2));
        drop(dev);
        server.join().unwrap();
        assert!(!handle.is_connected());
    }

    #[test]
    fn device_removed_and_ping() {
        let removed = json!({"DeviceRemoved": {"Id": 0, "DeviceIndex": 0}});
        let (url, log, server) = server(100, vec![removed]);
        let mut dev = ButtplugDevice::connect(ButtplugConfig {
            url,
            scan: false,
            ..Default::default()
        })
        .unwrap();
        assert!(wait_for(|| dev.handle().server_devices().is_empty()));
        assert!(dev.axes().is_empty());
        assert!(dev.notice().unwrap().contains("no devices"));
        // Nothing to drive: no message is sent.
        dev.move_to(Axis::L0, 0.5, 100).unwrap();
        assert!(wait_for(|| kinds(&log)
            .iter()
            .filter(|k| *k == "Ping")
            .count()
            >= 2));
        assert!(!kinds(&log).contains(&"StartScanning".to_string()));
        assert!(!kinds(&log).contains(&"LinearCmd".to_string()));
        drop(dev);
        server.join().unwrap();
    }

    #[test]
    fn server_errors_and_disconnects_are_reported() {
        let (url, _log, server) = server(0, vec![]);
        let dev = ButtplugDevice::connect(ButtplugConfig {
            url,
            ..Default::default()
        })
        .unwrap();
        let handle = dev.handle();
        dev.queue(vec![("Bogus", json!({}))]).unwrap();
        assert!(wait_for(|| handle.last_error().as_deref() == Some("nope")));
        drop(dev);
        server.join().unwrap();

        assert!(matches!(
            ButtplugDevice::connect(ButtplugConfig {
                url: "wss://example.invalid:12345".into(),
                ..Default::default()
            }),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            ButtplugDevice::connect(ButtplugConfig {
                url: "http://127.0.0.1:1".into(),
                ..Default::default()
            }),
            Err(Error::Config(_))
        ));
    }
}
