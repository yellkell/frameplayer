//! buttplug.io / Intiface Central client (Buttplug protocol message spec v3) over WebSocket.
//!
//! Wire format: every WebSocket text frame is a JSON array of messages, each an object with a
//! single key naming the message type, e.g. `[{"RequestServerInfo":{"Id":1,"ClientName":"x",
//! "MessageVersion":3}}]`. Client requests carry a non-zero `Id`; the server answers with the
//! same `Id` (`Ok`, `Error`, `ServerInfo`, `DeviceList`). Server-initiated events
//! (`DeviceAdded`, `DeviceRemoved`, `ScanningFinished`) use `Id: 0`. If the server's
//! `MaxPingTime` is non-zero the client must `Ping` more often than that or the server stops all
//! devices and disconnects.

use crate::device::{AxisTarget, DeviceInfo, HapticDevice, StreamStyle};
use crate::funscript::Axis;
use crate::{HapticsError, Result};
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Protocol (message spec) version we speak.
pub const MESSAGE_VERSION: u32 = 3;
/// Intiface Central's default WebSocket endpoint.
pub const DEFAULT_URL: &str = "ws://127.0.0.1:12345";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct IdOnly {
    pub id: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ErrorMsg {
    pub id: u32,
    pub error_message: String,
    /// 0 unknown, 1 init, 2 ping, 3 message, 4 device.
    pub error_code: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RequestServerInfo {
    pub id: u32,
    pub client_name: String,
    pub message_version: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ServerInfo {
    pub id: u32,
    pub server_name: String,
    pub message_version: u32,
    /// Milliseconds; 0 disables the ping requirement.
    pub max_ping_time: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActuatorType {
    Vibrate,
    Rotate,
    Oscillate,
    Constrict,
    Inflate,
    Position,
    #[serde(other)]
    Unknown,
}

/// Attributes of one actuator feature (`ScalarCmd`, `LinearCmd`, `RotateCmd` entries).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct FeatureAttributes {
    #[serde(default)]
    pub feature_descriptor: String,
    #[serde(default)]
    pub step_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actuator_type: Option<ActuatorType>,
}

/// The `DeviceMessages` map. Message types we don't use are kept in `other`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct DeviceMessages {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scalar_cmd: Option<Vec<FeatureAttributes>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linear_cmd: Option<Vec<FeatureAttributes>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotate_cmd: Option<Vec<FeatureAttributes>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_device_cmd: Option<Value>,
    #[serde(flatten)]
    pub other: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Device {
    pub device_name: String,
    pub device_index: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_message_timing_gap: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_display_name: Option<String>,
    #[serde(default)]
    pub device_messages: DeviceMessages,
}

impl Device {
    pub fn display_name(&self) -> &str {
        self.device_display_name
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.device_name)
    }
    pub fn linear_count(&self) -> usize {
        self.device_messages.linear_cmd.as_ref().map_or(0, Vec::len)
    }
    pub fn rotate_count(&self) -> usize {
        self.device_messages.rotate_cmd.as_ref().map_or(0, Vec::len)
    }
    /// Indices of scalar features with the given actuator type.
    pub fn scalar_indices(&self, ty: ActuatorType) -> Vec<(u32, ActuatorType)> {
        self.device_messages
            .scalar_cmd
            .iter()
            .flatten()
            .enumerate()
            .filter(|(_, f)| f.actuator_type == Some(ty))
            .map(|(i, _)| (i as u32, ty))
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct DeviceList {
    pub id: u32,
    pub devices: Vec<Device>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct DeviceAdded {
    pub id: u32,
    #[serde(flatten)]
    pub device: Device,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct DeviceRemoved {
    pub id: u32,
    pub device_index: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct DeviceCmd {
    pub id: u32,
    pub device_index: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ScalarSubcommand {
    pub index: u32,
    /// 0.0–1.0.
    pub scalar: f64,
    pub actuator_type: ActuatorType,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ScalarCmd {
    pub id: u32,
    pub device_index: u32,
    pub scalars: Vec<ScalarSubcommand>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct LinearSubcommand {
    pub index: u32,
    /// Milliseconds to reach `position`.
    pub duration: u32,
    /// 0.0–1.0.
    pub position: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct LinearCmd {
    pub id: u32,
    pub device_index: u32,
    pub vectors: Vec<LinearSubcommand>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RotateSubcommand {
    pub index: u32,
    /// 0.0–1.0.
    pub speed: f64,
    pub clockwise: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RotateCmd {
    pub id: u32,
    pub device_index: u32,
    pub rotations: Vec<RotateSubcommand>,
}

/// Every message we send or understand. Serialises in the externally-tagged
/// `{"Type": {...}}` form the protocol uses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ButtplugMessage {
    Ok(IdOnly),
    Error(ErrorMsg),
    Ping(IdOnly),
    RequestServerInfo(RequestServerInfo),
    ServerInfo(ServerInfo),
    StartScanning(IdOnly),
    StopScanning(IdOnly),
    ScanningFinished(IdOnly),
    RequestDeviceList(IdOnly),
    DeviceList(DeviceList),
    DeviceAdded(DeviceAdded),
    DeviceRemoved(DeviceRemoved),
    StopDeviceCmd(DeviceCmd),
    StopAllDevices(IdOnly),
    ScalarCmd(ScalarCmd),
    LinearCmd(LinearCmd),
    RotateCmd(RotateCmd),
}

impl ButtplugMessage {
    pub fn id(&self) -> u32 {
        use ButtplugMessage::*;
        match self {
            Ok(m) | Ping(m) | StartScanning(m) | StopScanning(m) | ScanningFinished(m)
            | RequestDeviceList(m) | StopAllDevices(m) => m.id,
            Error(m) => m.id,
            RequestServerInfo(m) => m.id,
            ServerInfo(m) => m.id,
            DeviceList(m) => m.id,
            DeviceAdded(m) => m.id,
            DeviceRemoved(m) => m.id,
            StopDeviceCmd(m) => m.id,
            ScalarCmd(m) => m.id,
            LinearCmd(m) => m.id,
            RotateCmd(m) => m.id,
        }
    }

    pub fn set_id(&mut self, id: u32) {
        use ButtplugMessage::*;
        match self {
            Ok(m) | Ping(m) | StartScanning(m) | StopScanning(m) | ScanningFinished(m)
            | RequestDeviceList(m) | StopAllDevices(m) => m.id = id,
            Error(m) => m.id = id,
            RequestServerInfo(m) => m.id = id,
            ServerInfo(m) => m.id = id,
            DeviceList(m) => m.id = id,
            DeviceAdded(m) => m.id = id,
            DeviceRemoved(m) => m.id = id,
            StopDeviceCmd(m) => m.id = id,
            ScalarCmd(m) => m.id = id,
            LinearCmd(m) => m.id = id,
            RotateCmd(m) => m.id = id,
        }
    }
}

/// Serialise messages into one protocol frame.
pub fn encode(msgs: &[ButtplugMessage]) -> String {
    serde_json::to_string(msgs).expect("buttplug messages always serialise")
}

/// Parse a protocol frame. Unknown message types are skipped (logged) rather than failing the
/// whole frame, so newer servers don't break us.
pub fn decode(frame: &str) -> Result<Vec<ButtplugMessage>> {
    let raw: Vec<Value> = serde_json::from_str(frame)
        .map_err(|e| HapticsError::Protocol(format!("bad frame: {e}")))?;
    Ok(raw
        .into_iter()
        .filter_map(
            |v| match serde_json::from_value::<ButtplugMessage>(v.clone()) {
                Ok(m) => Some(m),
                Err(e) => {
                    tracing::debug!("skipping unknown buttplug message {v}: {e}");
                    None
                }
            },
        )
        .collect())
}

/// Server-initiated events.
#[derive(Debug, Clone, PartialEq)]
pub enum ButtplugEvent {
    DeviceAdded(Device),
    DeviceRemoved(u32),
    ScanningFinished,
    Disconnected,
}

type Pending = Arc<Mutex<HashMap<u32, oneshot::Sender<ButtplugMessage>>>>;

/// Connected protocol client.
pub struct ButtplugClient {
    out_tx: mpsc::UnboundedSender<WsMessage>,
    pending: Pending,
    next_id: Arc<AtomicU32>,
    devices: Arc<Mutex<BTreeMap<u32, Device>>>,
    events: broadcast::Sender<ButtplugEvent>,
    server: ServerInfo,
    timeout: Duration,
    tasks: Vec<JoinHandle<()>>,
}

impl ButtplugClient {
    /// Connect, perform the `RequestServerInfo` handshake, start the ping task and fetch the
    /// current device list.
    pub async fn connect(url: &str, client_name: &str) -> Result<Self> {
        let (ws, _) = tokio_tungstenite::connect_async(url).await?;
        let (mut sink, mut stream) = ws.split();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<WsMessage>();
        let pending: Pending = Arc::default();
        let devices: Arc<Mutex<BTreeMap<u32, Device>>> = Arc::default();
        let (events, _) = broadcast::channel(64);

        let writer = tokio::spawn(async move {
            while let Some(m) = out_rx.recv().await {
                if sink.send(m).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        let (p, d, ev, otx) = (
            pending.clone(),
            devices.clone(),
            events.clone(),
            out_tx.clone(),
        );
        let reader = tokio::spawn(async move {
            while let Some(frame) = stream.next().await {
                let text = match frame {
                    Ok(WsMessage::Text(t)) => t,
                    Ok(WsMessage::Ping(data)) => {
                        let _ = otx.send(WsMessage::Pong(data));
                        continue;
                    }
                    Ok(WsMessage::Close(_)) | Err(_) => break,
                    Ok(_) => continue,
                };
                let Ok(msgs) = decode(&text) else {
                    tracing::warn!("invalid buttplug frame: {text}");
                    continue;
                };
                for m in msgs {
                    dispatch(m, &p, &d, &ev);
                }
            }
            p.lock().unwrap().clear();
            let _ = ev.send(ButtplugEvent::Disconnected);
        });

        let mut client = ButtplugClient {
            out_tx,
            pending,
            next_id: Arc::new(AtomicU32::new(1)),
            devices,
            events,
            server: ServerInfo {
                id: 0,
                server_name: String::new(),
                message_version: 0,
                max_ping_time: 0,
            },
            timeout: Duration::from_secs(5),
            tasks: vec![writer, reader],
        };

        let reply = client
            .request(ButtplugMessage::RequestServerInfo(RequestServerInfo {
                id: 0,
                client_name: client_name.to_string(),
                message_version: MESSAGE_VERSION,
            }))
            .await?;
        let ButtplugMessage::ServerInfo(info) = reply else {
            return Err(HapticsError::Protocol(format!(
                "expected ServerInfo, got {reply:?}"
            )));
        };
        if info.max_ping_time > 0 {
            let period = Duration::from_millis((info.max_ping_time / 2).max(1) as u64);
            let (tx, ids) = (client.out_tx.clone(), client.next_id.clone());
            client.tasks.push(tokio::spawn(async move {
                let mut iv = tokio::time::interval(period);
                loop {
                    iv.tick().await;
                    let id = ids.fetch_add(1, Ordering::Relaxed);
                    let frame = encode(&[ButtplugMessage::Ping(IdOnly { id })]);
                    if tx.send(WsMessage::Text(frame)).is_err() {
                        break;
                    }
                }
            }));
        }
        client.server = info;
        client.refresh_devices().await?;
        Ok(client)
    }

    pub fn server_info(&self) -> &ServerInfo {
        &self.server
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ButtplugEvent> {
        self.events.subscribe()
    }

    /// Cached device list, kept current by `DeviceAdded` / `DeviceRemoved` events.
    pub fn devices(&self) -> Vec<Device> {
        self.devices.lock().unwrap().values().cloned().collect()
    }

    /// Send a request (its `Id` is assigned here) and wait for the reply. `Error` replies
    /// become [`HapticsError::Device`].
    pub async fn request(&self, mut msg: ButtplugMessage) -> Result<ButtplugMessage> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        msg.set_id(id);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        if self.out_tx.send(WsMessage::Text(encode(&[msg]))).is_err() {
            self.pending.lock().unwrap().remove(&id);
            return Err(HapticsError::NotConnected);
        }
        let reply = match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => return Err(HapticsError::NotConnected),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                return Err(HapticsError::Timeout("buttplug reply"));
            }
        };
        match reply {
            ButtplugMessage::Error(e) => Err(HapticsError::Device(format!(
                "buttplug error {}: {}",
                e.error_code, e.error_message
            ))),
            other => Ok(other),
        }
    }

    async fn expect_ok(&self, msg: ButtplugMessage) -> Result<()> {
        match self.request(msg).await? {
            ButtplugMessage::Ok(_) => Ok(()),
            other => Err(HapticsError::Protocol(format!(
                "expected Ok, got {other:?}"
            ))),
        }
    }

    pub async fn refresh_devices(&self) -> Result<Vec<Device>> {
        match self
            .request(ButtplugMessage::RequestDeviceList(IdOnly { id: 0 }))
            .await?
        {
            ButtplugMessage::DeviceList(list) => {
                let mut d = self.devices.lock().unwrap();
                d.clear();
                for dev in &list.devices {
                    d.insert(dev.device_index, dev.clone());
                }
                Ok(list.devices)
            }
            other => Err(HapticsError::Protocol(format!(
                "expected DeviceList, got {other:?}"
            ))),
        }
    }

    pub async fn start_scanning(&self) -> Result<()> {
        self.expect_ok(ButtplugMessage::StartScanning(IdOnly { id: 0 }))
            .await
    }

    pub async fn stop_scanning(&self) -> Result<()> {
        self.expect_ok(ButtplugMessage::StopScanning(IdOnly { id: 0 }))
            .await
    }

    pub async fn ping(&self) -> Result<()> {
        self.expect_ok(ButtplugMessage::Ping(IdOnly { id: 0 }))
            .await
    }

    pub async fn linear(&self, device_index: u32, vectors: Vec<LinearSubcommand>) -> Result<()> {
        self.expect_ok(ButtplugMessage::LinearCmd(LinearCmd {
            id: 0,
            device_index,
            vectors,
        }))
        .await
    }

    pub async fn scalar(&self, device_index: u32, scalars: Vec<ScalarSubcommand>) -> Result<()> {
        self.expect_ok(ButtplugMessage::ScalarCmd(ScalarCmd {
            id: 0,
            device_index,
            scalars,
        }))
        .await
    }

    pub async fn rotate(&self, device_index: u32, rotations: Vec<RotateSubcommand>) -> Result<()> {
        self.expect_ok(ButtplugMessage::RotateCmd(RotateCmd {
            id: 0,
            device_index,
            rotations,
        }))
        .await
    }

    pub async fn stop_device(&self, device_index: u32) -> Result<()> {
        self.expect_ok(ButtplugMessage::StopDeviceCmd(DeviceCmd {
            id: 0,
            device_index,
        }))
        .await
    }

    pub async fn stop_all(&self) -> Result<()> {
        self.expect_ok(ButtplugMessage::StopAllDevices(IdOnly { id: 0 }))
            .await
    }

    /// Close the WebSocket and stop background tasks.
    pub async fn disconnect(mut self) {
        let _ = self.out_tx.send(WsMessage::Close(None));
        tokio::time::sleep(Duration::from_millis(20)).await;
        for t in self.tasks.drain(..) {
            t.abort();
        }
    }
}

impl Drop for ButtplugClient {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

fn dispatch(
    m: ButtplugMessage,
    pending: &Pending,
    devices: &Mutex<BTreeMap<u32, Device>>,
    ev: &broadcast::Sender<ButtplugEvent>,
) {
    match m {
        ButtplugMessage::DeviceAdded(a) => {
            devices
                .lock()
                .unwrap()
                .insert(a.device.device_index, a.device.clone());
            let _ = ev.send(ButtplugEvent::DeviceAdded(a.device));
        }
        ButtplugMessage::DeviceRemoved(r) => {
            devices.lock().unwrap().remove(&r.device_index);
            let _ = ev.send(ButtplugEvent::DeviceRemoved(r.device_index));
        }
        ButtplugMessage::ScanningFinished(_) => {
            let _ = ev.send(ButtplugEvent::ScanningFinished);
        }
        other => {
            let id = other.id();
            if let Some(tx) = pending.lock().unwrap().remove(&id) {
                let _ = tx.send(other);
            } else if let ButtplugMessage::Error(e) = other {
                tracing::warn!(
                    "buttplug server error: {} ({})",
                    e.error_message,
                    e.error_code
                );
            }
        }
    }
}

/// Settings for [`ButtplugDevice`].
#[derive(Debug, Clone)]
pub struct ButtplugConfig {
    pub url: String,
    pub client_name: String,
    /// Only drive devices whose name contains this (case-insensitive).
    pub name_filter: Option<String>,
    /// Scan this long on connect if no device is known yet.
    pub scan_time: Duration,
}

impl Default for ButtplugConfig {
    fn default() -> Self {
        ButtplugConfig {
            url: DEFAULT_URL.into(),
            client_name: "FramePlayer".into(),
            name_filter: None,
            scan_time: Duration::from_secs(5),
        }
    }
}

/// [`HapticDevice`] that fans targets out to every matching buttplug device: stroke (`L0`)
/// to `LinearCmd`, vibration (`V0`) to `ScalarCmd` vibrate actuators.
pub struct ButtplugDevice {
    cfg: ButtplugConfig,
    client: Option<ButtplugClient>,
}

impl ButtplugDevice {
    pub fn new(cfg: ButtplugConfig) -> Self {
        ButtplugDevice { cfg, client: None }
    }

    pub fn client(&self) -> Option<&ButtplugClient> {
        self.client.as_ref()
    }

    fn targets(&self) -> Vec<Device> {
        let Some(c) = &self.client else {
            return Vec::new();
        };
        c.devices()
            .into_iter()
            .filter(|d| match &self.cfg.name_filter {
                Some(f) => d
                    .display_name()
                    .to_ascii_lowercase()
                    .contains(&f.to_ascii_lowercase()),
                None => true,
            })
            .collect()
    }
}

#[async_trait]
impl HapticDevice for ButtplugDevice {
    fn info(&self) -> DeviceInfo {
        let devs = self.targets();
        let mut axes = Vec::new();
        if devs.is_empty() || devs.iter().any(|d| d.linear_count() > 0) {
            axes.push((Axis::L0, StreamStyle::NextAction));
        }
        if devs.is_empty()
            || devs
                .iter()
                .any(|d| !d.scalar_indices(ActuatorType::Vibrate).is_empty())
        {
            axes.push((Axis::V0, StreamStyle::Interpolated));
        }
        let name = match devs.as_slice() {
            [] => "Intiface".to_string(),
            [d] => d.display_name().to_string(),
            [d, ..] => format!("{} +{}", d.display_name(), devs.len() - 1),
        };
        DeviceInfo {
            name,
            axes,
            script_sync: false,
            script_sync_any_speed: false,
            latency_ms: 30, // [verify] BLE latency through Intiface varies per device.
            update_interval_ms: 20,
        }
    }

    async fn connect(&mut self) -> Result<()> {
        let client = ButtplugClient::connect(&self.cfg.url, &self.cfg.client_name).await?;
        self.client = Some(client);
        if self.targets().is_empty() {
            let c = self.client.as_ref().expect("just set");
            let mut ev = c.subscribe();
            c.start_scanning().await?;
            let _ = tokio::time::timeout(self.cfg.scan_time, async {
                while let Ok(e) = ev.recv().await {
                    if matches!(
                        e,
                        ButtplugEvent::DeviceAdded(_) | ButtplugEvent::ScanningFinished
                    ) {
                        break;
                    }
                }
            })
            .await;
            let _ = c.stop_scanning().await;
        }
        Ok(())
    }

    async fn disconnect(&mut self) -> Result<()> {
        if let Some(c) = self.client.take() {
            let _ = c.stop_all().await;
            c.disconnect().await;
        }
        Ok(())
    }

    async fn send(&mut self, targets: &[AxisTarget]) -> Result<()> {
        let devs = self.targets();
        let c = self.client.as_ref().ok_or(HapticsError::NotConnected)?;
        for t in targets {
            for d in &devs {
                match t.axis {
                    Axis::L0 if d.linear_count() > 0 => {
                        let vectors = (0..d.linear_count() as u32)
                            .map(|index| LinearSubcommand {
                                index,
                                duration: t.duration_ms,
                                position: t.position.clamp(0.0, 1.0),
                            })
                            .collect();
                        c.linear(d.device_index, vectors).await?;
                    }
                    Axis::V0 => {
                        let scalars: Vec<ScalarSubcommand> = d
                            .scalar_indices(ActuatorType::Vibrate)
                            .into_iter()
                            .map(|(index, actuator_type)| ScalarSubcommand {
                                index,
                                scalar: t.position.clamp(0.0, 1.0),
                                actuator_type,
                            })
                            .collect();
                        if !scalars.is_empty() {
                            c.scalar(d.device_index, scalars).await?;
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        match &self.client {
            Some(c) => c.stop_all().await,
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn serializes_spec_shapes() {
        let m = ButtplugMessage::RequestServerInfo(RequestServerInfo {
            id: 1,
            client_name: "FP".into(),
            message_version: 3,
        });
        assert_eq!(
            encode(&[m]),
            r#"[{"RequestServerInfo":{"Id":1,"ClientName":"FP","MessageVersion":3}}]"#
        );
        let m = ButtplugMessage::LinearCmd(LinearCmd {
            id: 2,
            device_index: 0,
            vectors: vec![LinearSubcommand {
                index: 0,
                duration: 500,
                position: 0.3,
            }],
        });
        assert_eq!(
            serde_json::to_value([m]).unwrap(),
            json!([{"LinearCmd": {"Id": 2, "DeviceIndex": 0, "Vectors": [{"Index": 0, "Duration": 500, "Position": 0.3}]}}])
        );
        let m = ButtplugMessage::ScalarCmd(ScalarCmd {
            id: 3,
            device_index: 1,
            scalars: vec![ScalarSubcommand {
                index: 0,
                scalar: 0.5,
                actuator_type: ActuatorType::Vibrate,
            }],
        });
        assert_eq!(
            serde_json::to_value([m]).unwrap(),
            json!([{"ScalarCmd": {"Id": 3, "DeviceIndex": 1, "Scalars": [{"Index": 0, "Scalar": 0.5, "ActuatorType": "Vibrate"}]}}])
        );
        let m = ButtplugMessage::RotateCmd(RotateCmd {
            id: 4,
            device_index: 2,
            rotations: vec![RotateSubcommand {
                index: 0,
                speed: 0.5,
                clockwise: true,
            }],
        });
        assert_eq!(
            serde_json::to_value([m]).unwrap(),
            json!([{"RotateCmd": {"Id": 4, "DeviceIndex": 2, "Rotations": [{"Index": 0, "Speed": 0.5, "Clockwise": true}]}}])
        );
        assert_eq!(
            encode(&[
                ButtplugMessage::StopDeviceCmd(DeviceCmd {
                    id: 5,
                    device_index: 0
                }),
                ButtplugMessage::Ping(IdOnly { id: 6 })
            ]),
            r#"[{"StopDeviceCmd":{"Id":5,"DeviceIndex":0}},{"Ping":{"Id":6}}]"#
        );
        assert_eq!(
            encode(&[ButtplugMessage::StartScanning(IdOnly { id: 7 })]),
            r#"[{"StartScanning":{"Id":7}}]"#
        );
        assert_eq!(
            encode(&[ButtplugMessage::RequestDeviceList(IdOnly { id: 8 })]),
            r#"[{"RequestDeviceList":{"Id":8}}]"#
        );
    }

    #[test]
    fn parses_server_messages() {
        let frame = r#"[
          {"ServerInfo":{"Id":1,"ServerName":"Intiface Server","MessageVersion":3,"MaxPingTime":100}},
          {"DeviceList":{"Id":2,"Devices":[{"DeviceName":"Test Vibrator","DeviceIndex":0,
              "DeviceMessageTimingGap":100,"DeviceDisplayName":"Rabbit",
              "DeviceMessages":{"ScalarCmd":[{"StepCount":20,"FeatureDescriptor":"Clitoral Stimulator","ActuatorType":"Vibrate"},
                                             {"StepCount":20,"FeatureDescriptor":"Squeeze","ActuatorType":"Constrict"}],
                                "StopDeviceCmd":{},"SensorReadCmd":[{"SensorType":"Battery"}]}}]}},
          {"DeviceAdded":{"Id":0,"DeviceName":"OSR2","DeviceIndex":3,
              "DeviceMessages":{"LinearCmd":[{"StepCount":100,"FeatureDescriptor":""}],"RotateCmd":[{"StepCount":10}],"StopDeviceCmd":{}}}},
          {"DeviceRemoved":{"Id":0,"DeviceIndex":0}},
          {"ScanningFinished":{"Id":0}},
          {"Error":{"Id":9,"ErrorMessage":"nope","ErrorCode":4}},
          {"FutureMessage":{"Id":0}},
          {"Ok":{"Id":5}}
        ]"#;
        let msgs = decode(frame).unwrap();
        assert_eq!(msgs.len(), 7, "unknown message skipped");
        assert!(matches!(&msgs[0], ButtplugMessage::ServerInfo(s) if s.max_ping_time == 100));
        let ButtplugMessage::DeviceList(l) = &msgs[1] else {
            panic!()
        };
        let d = &l.devices[0];
        assert_eq!(d.display_name(), "Rabbit");
        assert_eq!(
            d.scalar_indices(ActuatorType::Vibrate),
            vec![(0, ActuatorType::Vibrate)]
        );
        assert_eq!(
            d.scalar_indices(ActuatorType::Constrict),
            vec![(1, ActuatorType::Constrict)]
        );
        assert!(d.device_messages.other.contains_key("SensorReadCmd"));
        let ButtplugMessage::DeviceAdded(a) = &msgs[2] else {
            panic!()
        };
        assert_eq!(
            (
                a.device.device_index,
                a.device.linear_count(),
                a.device.rotate_count()
            ),
            (3, 1, 1)
        );
        assert_eq!(msgs[5].id(), 9);
        assert_eq!(msgs[6], ButtplugMessage::Ok(IdOnly { id: 5 }));
        // Round trip.
        let again = decode(&encode(&msgs)).unwrap();
        assert_eq!(again, msgs);
        assert!(decode("{}").is_err());
    }

    /// Minimal Intiface stand-in: answers the handshake, reports one linear device, adds a
    /// vibrator when scanning starts, acknowledges commands and records them.
    async fn mock_server() -> (String, Arc<Mutex<Vec<ButtplugMessage>>>) {
        let log: Arc<Mutex<Vec<ButtplugMessage>>> = Arc::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let log2 = log.clone();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let linear = Device {
                device_name: "OSR2".into(),
                device_index: 0,
                device_message_timing_gap: None,
                device_display_name: None,
                device_messages: DeviceMessages {
                    linear_cmd: Some(vec![FeatureAttributes {
                        feature_descriptor: String::new(),
                        step_count: 100,
                        actuator_type: None,
                    }]),
                    ..Default::default()
                },
            };
            let vibe = Device {
                device_name: "Vibe".into(),
                device_index: 1,
                device_message_timing_gap: None,
                device_display_name: None,
                device_messages: DeviceMessages {
                    scalar_cmd: Some(vec![FeatureAttributes {
                        feature_descriptor: String::new(),
                        step_count: 20,
                        actuator_type: Some(ActuatorType::Vibrate),
                    }]),
                    ..Default::default()
                },
            };
            while let Some(Ok(WsMessage::Text(t))) = ws.next().await {
                for m in decode(&t).unwrap() {
                    log2.lock().unwrap().push(m.clone());
                    let id = m.id();
                    let mut replies = vec![];
                    match m {
                        ButtplugMessage::RequestServerInfo(_) => {
                            replies.push(ButtplugMessage::ServerInfo(ServerInfo {
                                id,
                                server_name: "mock".into(),
                                message_version: 3,
                                max_ping_time: 60,
                            }))
                        }
                        ButtplugMessage::RequestDeviceList(_) => {
                            replies.push(ButtplugMessage::DeviceList(DeviceList {
                                id,
                                devices: vec![linear.clone()],
                            }))
                        }
                        ButtplugMessage::StartScanning(_) => {
                            replies.push(ButtplugMessage::Ok(IdOnly { id }));
                            replies.push(ButtplugMessage::DeviceAdded(DeviceAdded {
                                id: 0,
                                device: vibe.clone(),
                            }));
                        }
                        ButtplugMessage::RotateCmd(_) => {
                            replies.push(ButtplugMessage::Error(ErrorMsg {
                                id,
                                error_message: "no rotate".into(),
                                error_code: 4,
                            }))
                        }
                        _ => replies.push(ButtplugMessage::Ok(IdOnly { id })),
                    }
                    ws.send(WsMessage::Text(encode(&replies))).await.unwrap();
                }
            }
        });
        (format!("ws://{addr}"), log)
    }

    #[tokio::test]
    async fn client_against_mock_server() {
        let (url, log) = mock_server().await;
        let client = ButtplugClient::connect(&url, "test").await.unwrap();
        assert_eq!(client.server_info().max_ping_time, 60);
        assert_eq!(client.devices().len(), 1);
        let mut ev = client.subscribe();
        client.start_scanning().await.unwrap();
        let e = tokio::time::timeout(Duration::from_secs(2), ev.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(e, ButtplugEvent::DeviceAdded(ref d) if d.device_name == "Vibe"));
        assert_eq!(client.devices().len(), 2);
        let err = client
            .rotate(
                0,
                vec![RotateSubcommand {
                    index: 0,
                    speed: 1.0,
                    clockwise: false,
                }],
            )
            .await;
        assert!(matches!(err, Err(HapticsError::Device(m)) if m.contains("no rotate")));
        // Let the ping task fire a few times (MaxPingTime 60 => every 30 ms).
        tokio::time::sleep(Duration::from_millis(120)).await;
        client.disconnect().await;
        let log = log.lock().unwrap();
        assert!(
            matches!(log[0], ButtplugMessage::RequestServerInfo(ref r) if r.message_version == 3 && r.id == 1)
        );
        assert!(
            log.iter()
                .filter(|m| matches!(m, ButtplugMessage::Ping(_)))
                .count()
                >= 2
        );
        // Ids are unique and non-zero.
        let mut ids: Vec<u32> = log.iter().map(ButtplugMessage::id).collect();
        assert!(ids.iter().all(|i| *i > 0));
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), log.len());
    }

    #[tokio::test]
    async fn device_adapter_routes_axes() {
        let (url, log) = mock_server().await;
        let mut dev = ButtplugDevice::new(ButtplugConfig {
            url,
            ..Default::default()
        });
        dev.connect().await.unwrap();
        let info = dev.info();
        assert!(info.supports(Axis::L0));
        dev.connect_scan_vibe().await;
        assert!(dev.info().supports(Axis::V0));
        dev.send(&[
            AxisTarget {
                axis: Axis::L0,
                position: 0.75,
                duration_ms: 250,
            },
            AxisTarget {
                axis: Axis::V0,
                position: 0.4,
                duration_ms: 20,
            },
        ])
        .await
        .unwrap();
        dev.stop().await.unwrap();
        dev.disconnect().await.unwrap();
        let log = log.lock().unwrap();
        assert!(log.iter().any(|m| matches!(m, ButtplugMessage::LinearCmd(c)
            if c.device_index == 0 && c.vectors == vec![LinearSubcommand { index: 0, duration: 250, position: 0.75 }])));
        assert!(log.iter().any(|m| matches!(m, ButtplugMessage::ScalarCmd(c)
            if c.device_index == 1 && c.scalars[0].scalar == 0.4 && c.scalars[0].actuator_type == ActuatorType::Vibrate)));
        assert!(log
            .iter()
            .any(|m| matches!(m, ButtplugMessage::StopAllDevices(_))));
    }

    impl ButtplugDevice {
        async fn connect_scan_vibe(&self) {
            let c = self.client.as_ref().unwrap();
            let mut ev = c.subscribe();
            c.start_scanning().await.unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(2), ev.recv()).await;
        }
    }
}
