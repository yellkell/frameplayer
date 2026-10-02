//! The Handy: cloud REST API v2 and (optionally) Bluetooth LE.
//!
//! Cloud flow (HSSP, "Handy Synchronised Script Protocol"):
//! 1. `GET /connected` with the `X-Connection-Key` header.
//! 2. Estimate the server clock: N round trips to `GET /servertime`; each sample gives
//!    `offset = serverTime + rtt/2 - local_receive_time`. The fastest half of the samples is
//!    averaged ([`estimate_server_offset`]).
//! 3. Upload the funscript to Handy's hosting service, `PUT /mode {mode: 1}`, then
//!    `PUT /hssp/setup {url}` so the device downloads and caches it.
//! 4. On every resync: `PUT /hssp/play {estimatedServerTime, startTime}` or `PUT /hssp/stop`.
//!
//! HSSP cannot follow a playback rate other than 1.0, so for those (and when the upload fails)
//! the engine streams instead and we use HDSP (`PUT /hdsp/xpt`, "go to position over
//! duration"). HAMP (alternating motion) helpers are provided for manual mode.
//!
//! [verify] Endpoint paths and bodies follow the public Handy API v2 documentation
//! (handyfeeling.com/api/handy/v2/docs). Handy has since published API v3 ("HSP" streaming);
//! check whether v2 remains available for firmware 4 devices before release.

use crate::device::{AxisTarget, DeviceInfo, HapticDevice, StreamStyle};
use crate::funscript::{Axis, ScriptSet};
use crate::{HapticsError, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use url::Url;

/// Default base URL of the Handy REST API v2.
pub const HANDY_API_V2: &str = "https://www.handyfeeling.com/api/handy/v2/";
/// Default upload endpoint of Handy's temporary script hosting. [verify]
pub const HANDY_HOSTING_UPLOAD: &str = "https://www.handyfeeling.com/api/hosting/v2/upload";

/// Device operating modes (`PUT /mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum HandyMode {
    /// Alternating motion (manual speed/stroke).
    Hamp = 0,
    /// Synchronised script playback.
    Hssp = 1,
    /// Direct position.
    Hdsp = 2,
    Maintenance = 3,
    /// Buffered script (streaming points).
    Hbsp = 4,
}

impl HandyMode {
    pub fn from_code(c: i64) -> Option<Self> {
        Some(match c {
            0 => HandyMode::Hamp,
            1 => HandyMode::Hssp,
            2 => HandyMode::Hdsp,
            3 => HandyMode::Maintenance,
            4 => HandyMode::Hbsp,
            _ => return None,
        })
    }
}

/// One `/servertime` round trip, local times in ms on any monotonic clock.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimeSample {
    pub sent_ms: f64,
    pub server_ms: f64,
    pub received_ms: f64,
}

impl TimeSample {
    pub fn rtt(&self) -> f64 {
        self.received_ms - self.sent_ms
    }
    /// `server - local` assuming the reply was stamped halfway through the round trip.
    pub fn offset(&self) -> f64 {
        self.server_ms + self.rtt() / 2.0 - self.received_ms
    }
}

/// Estimate `server_time - local_time` from round-trip samples: discard samples with a
/// negative round trip, keep the fastest half (slow samples carry asymmetric queueing delay),
/// and average their offsets.
pub fn estimate_server_offset(samples: &[TimeSample]) -> Option<f64> {
    let mut good: Vec<&TimeSample> = samples.iter().filter(|s| s.rtt() >= 0.0).collect();
    if good.is_empty() {
        return None;
    }
    good.sort_by(|a, b| a.rtt().total_cmp(&b.rtt()));
    let keep = good.len().div_ceil(2);
    let sum: f64 = good[..keep].iter().map(|s| s.offset()).sum();
    Some(sum / keep as f64)
}

/// Cloud connection settings.
#[derive(Debug, Clone)]
pub struct HandyConfig {
    /// The connection key shown in the Handy app / onboarding.
    pub connection_key: String,
    pub api_base: Url,
    pub upload_url: Url,
    /// Number of `/servertime` round trips per sync.
    pub time_sync_samples: usize,
    /// Lookahead for HDSP streaming (cloud round trip + device). [verify] tune on real network.
    pub latency_ms: u32,
    pub request_timeout: Duration,
}

impl HandyConfig {
    pub fn new(connection_key: impl Into<String>) -> Self {
        HandyConfig {
            connection_key: connection_key.into(),
            api_base: Url::parse(HANDY_API_V2).expect("static url"),
            upload_url: Url::parse(HANDY_HOSTING_UPLOAD).expect("static url"),
            time_sync_samples: 12,
            latency_ms: 200,
            request_timeout: Duration::from_secs(10),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ApiErrorBody {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    connected: Option<bool>,
}

/// Low-level Handy API v2 client.
#[derive(Debug, Clone)]
pub struct HandyClient {
    http: reqwest::Client,
    cfg: HandyConfig,
    epoch: Instant,
    offset_ms: Option<f64>,
}

impl HandyClient {
    pub fn new(cfg: HandyConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(cfg.request_timeout)
            .build()?;
        Ok(HandyClient {
            http,
            cfg,
            epoch: Instant::now(),
            offset_ms: None,
        })
    }

    pub fn config(&self) -> &HandyConfig {
        &self.cfg
    }

    fn local_ms(&self) -> f64 {
        self.epoch.elapsed().as_secs_f64() * 1000.0
    }

    /// The current server-time estimate, once [`HandyClient::sync_time`] has run.
    pub fn estimated_server_time(&self) -> Option<i64> {
        self.offset_ms.map(|o| (self.local_ms() + o).round() as i64)
    }

    fn url(&self, path: &str) -> Result<Url> {
        self.cfg
            .api_base
            .join(path)
            .map_err(|e| HapticsError::Protocol(e.to_string()))
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let mut req = self
            .http
            .request(method, self.url(path)?)
            .header("X-Connection-Key", &self.cfg.connection_key)
            .header("Accept", "application/json");
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let v: Value = resp.json().await.unwrap_or(Value::Null);
        if let Some(err) = v.get("error") {
            let e: ApiErrorBody = serde_json::from_value(err.clone()).unwrap_or(ApiErrorBody {
                message: Some(err.to_string()),
                name: None,
                connected: None,
            });
            if e.connected == Some(false) {
                return Err(HapticsError::NotConnected);
            }
            let msg = e
                .message
                .or(e.name)
                .unwrap_or_else(|| "unknown error".into());
            return Err(HapticsError::Device(format!("Handy API: {msg}")));
        }
        if !status.is_success() {
            return Err(HapticsError::Device(format!(
                "Handy API {path}: HTTP {status}"
            )));
        }
        Ok(v)
    }

    fn result_code(v: &Value, what: &str) -> Result<i64> {
        let r = v.get("result").and_then(Value::as_i64).unwrap_or(0);
        if r < 0 {
            Err(HapticsError::Device(format!(
                "Handy {what} failed (result {r})"
            )))
        } else {
            Ok(r)
        }
    }

    /// `GET /connected`.
    pub async fn connected(&self) -> Result<bool> {
        let v = self.call(reqwest::Method::GET, "connected", None).await?;
        Ok(v.get("connected").and_then(Value::as_bool).unwrap_or(false))
    }

    /// `GET /info` (firmware, hardware, model...).
    pub async fn info(&self) -> Result<Value> {
        self.call(reqwest::Method::GET, "info", None).await
    }

    /// `GET /mode`.
    pub async fn mode(&self) -> Result<Option<HandyMode>> {
        let v = self.call(reqwest::Method::GET, "mode", None).await?;
        Ok(v.get("mode")
            .and_then(Value::as_i64)
            .and_then(HandyMode::from_code))
    }

    /// `PUT /mode`.
    pub async fn set_mode(&self, mode: HandyMode) -> Result<()> {
        let v = self
            .call(
                reqwest::Method::PUT,
                "mode",
                Some(json!({ "mode": mode as u8 })),
            )
            .await?;
        Self::result_code(&v, "set mode").map(drop)
    }

    /// One `GET /servertime` round trip.
    pub async fn sample_server_time(&self) -> Result<TimeSample> {
        let sent_ms = self.local_ms();
        let v = self.call(reqwest::Method::GET, "servertime", None).await?;
        let received_ms = self.local_ms();
        let server_ms = v.get("serverTime").and_then(Value::as_f64).ok_or_else(|| {
            HapticsError::Protocol("servertime response without serverTime".into())
        })?;
        Ok(TimeSample {
            sent_ms,
            server_ms,
            received_ms,
        })
    }

    /// Run the round-trip sampling and store the estimated server clock offset.
    pub async fn sync_time(&mut self) -> Result<f64> {
        let mut samples = Vec::with_capacity(self.cfg.time_sync_samples);
        for _ in 0..self.cfg.time_sync_samples.max(1) {
            samples.push(self.sample_server_time().await?);
        }
        let off = estimate_server_offset(&samples)
            .ok_or(HapticsError::Protocol("no valid time samples".into()))?;
        self.offset_ms = Some(off);
        Ok(off)
    }

    /// Upload script contents to Handy's hosting service and return the download URL.
    pub async fn upload_script(&self, funscript_json: String) -> Result<String> {
        let part = reqwest::multipart::Part::text(funscript_json)
            .file_name("frameplayer.funscript")
            .mime_str("application/json")?;
        let form = reqwest::multipart::Form::new().part("file", part);
        let resp = self
            .http
            .post(self.cfg.upload_url.clone())
            .multipart(form)
            .send()
            .await?
            .error_for_status()?;
        let v: Value = resp.json().await?;
        v.get("url")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| HapticsError::Protocol(format!("upload response without url: {v}")))
    }

    /// `PUT /hssp/setup`. Returns `true` if the device reports it used its cached copy.
    pub async fn hssp_setup(&self, url: &str, sha256: Option<&str>) -> Result<bool> {
        let mut body = json!({ "url": url });
        if let Some(h) = sha256 {
            body["sha256"] = json!(h);
        }
        let v = self
            .call(reqwest::Method::PUT, "hssp/setup", Some(body))
            .await?;
        Ok(Self::result_code(&v, "HSSP setup")? == 1)
    }

    /// `PUT /hssp/play` with the estimated server time and script start time.
    pub async fn hssp_play(&self, start_time_ms: i64) -> Result<()> {
        let est = self
            .estimated_server_time()
            .ok_or(HapticsError::Protocol("server time not synced".into()))?;
        let v = self
            .call(
                reqwest::Method::PUT,
                "hssp/play",
                Some(json!({ "estimatedServerTime": est, "startTime": start_time_ms })),
            )
            .await?;
        Self::result_code(&v, "HSSP play").map(drop)
    }

    /// `PUT /hssp/stop`.
    pub async fn hssp_stop(&self) -> Result<()> {
        let v = self.call(reqwest::Method::PUT, "hssp/stop", None).await?;
        Self::result_code(&v, "HSSP stop").map(drop)
    }

    /// `PUT /hstp/offset`: device-side script offset in ms.
    pub async fn set_offset(&self, offset_ms: i64) -> Result<()> {
        let v = self
            .call(
                reqwest::Method::PUT,
                "hstp/offset",
                Some(json!({ "offset": offset_ms })),
            )
            .await?;
        Self::result_code(&v, "set offset").map(drop)
    }

    /// `PUT /slide`: stroke range in percent.
    pub async fn set_slide(&self, min: f64, max: f64) -> Result<()> {
        let v = self
            .call(
                reqwest::Method::PUT,
                "slide",
                Some(json!({ "min": min.clamp(0.0, 100.0), "max": max.clamp(0.0, 100.0) })),
            )
            .await?;
        Self::result_code(&v, "set slide").map(drop)
    }

    /// `PUT /hdsp/xpt`: move to `position_pct` (0–100) over `duration_ms`.
    pub async fn hdsp_xpt(
        &self,
        position_pct: f64,
        duration_ms: u32,
        stop_on_target: bool,
    ) -> Result<()> {
        let body = json!({
            "position": position_pct.clamp(0.0, 100.0),
            "duration": duration_ms,
            "stopOnTarget": stop_on_target,
            "immediateResponse": true,
        });
        let v = self
            .call(reqwest::Method::PUT, "hdsp/xpt", Some(body))
            .await?;
        Self::result_code(&v, "HDSP").map(drop)
    }

    /// `PUT /hamp/start`.
    pub async fn hamp_start(&self) -> Result<()> {
        self.call(reqwest::Method::PUT, "hamp/start", None)
            .await
            .map(drop)
    }

    /// `PUT /hamp/stop`.
    pub async fn hamp_stop(&self) -> Result<()> {
        self.call(reqwest::Method::PUT, "hamp/stop", None)
            .await
            .map(drop)
    }

    /// `PUT /hamp/velocity` in percent.
    pub async fn hamp_velocity(&self, velocity_pct: f64) -> Result<()> {
        self.call(
            reqwest::Method::PUT,
            "hamp/velocity",
            Some(json!({ "velocity": velocity_pct.clamp(0.0, 100.0) })),
        )
        .await
        .map(drop)
    }
}

/// [`HapticDevice`] backed by the Handy cloud API: HSSP when possible, HDSP otherwise.
pub struct HandyCloud {
    client: HandyClient,
    mode: Option<HandyMode>,
    /// (hash of uploaded JSON, hosted URL)
    uploaded: Option<(u64, String)>,
    setup_done: bool,
}

impl HandyCloud {
    pub fn new(cfg: HandyConfig) -> Result<Self> {
        Ok(HandyCloud {
            client: HandyClient::new(cfg)?,
            mode: None,
            uploaded: None,
            setup_done: false,
        })
    }

    pub fn client(&self) -> &HandyClient {
        &self.client
    }

    async fn ensure_mode(&mut self, mode: HandyMode) -> Result<()> {
        if self.mode != Some(mode) {
            self.client.set_mode(mode).await?;
            self.mode = Some(mode);
            if mode != HandyMode::Hssp {
                // [verify] whether leaving HSSP discards the loaded script; assume it does.
                self.setup_done = false;
            }
        }
        Ok(())
    }

    async fn ensure_setup(&mut self) -> Result<()> {
        self.ensure_mode(HandyMode::Hssp).await?;
        if !self.setup_done {
            let (_, url) = self
                .uploaded
                .clone()
                .ok_or(HapticsError::Protocol("no script uploaded".into()))?;
            self.client.hssp_setup(&url, None).await?;
            self.setup_done = true;
        }
        Ok(())
    }
}

fn hash_str(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

#[async_trait]
impl HapticDevice for HandyCloud {
    fn info(&self) -> DeviceInfo {
        DeviceInfo {
            name: "The Handy (cloud)".into(),
            axes: vec![(Axis::L0, StreamStyle::NextAction)],
            script_sync: true,
            script_sync_any_speed: false,
            latency_ms: self.client.cfg.latency_ms,
            update_interval_ms: 20,
        }
    }

    async fn connect(&mut self) -> Result<()> {
        if !self.client.connected().await? {
            return Err(HapticsError::Device(
                "The Handy is not connected to the cloud (check Wi-Fi and connection key)".into(),
            ));
        }
        self.mode = self.client.mode().await.ok().flatten();
        self.client.sync_time().await?;
        Ok(())
    }

    async fn prepare_script(&mut self, scripts: &ScriptSet) -> Result<bool> {
        let Some(script) = scripts.get(Axis::L0).filter(|s| !s.is_empty()) else {
            return Ok(false);
        };
        let json = script.to_funscript_json();
        let h = hash_str(&json);
        if self.uploaded.as_ref().map(|(x, _)| *x) != Some(h) {
            let url = self.client.upload_script(json).await?;
            self.uploaded = Some((h, url));
            self.setup_done = false;
        }
        self.ensure_setup().await?;
        Ok(true)
    }

    async fn sync_play(&mut self, script_time_ms: i64, _speed: f64) -> Result<()> {
        self.ensure_setup().await?;
        if self.client.offset_ms.is_none() {
            self.client.sync_time().await?;
        }
        self.client.hssp_play(script_time_ms).await
    }

    async fn sync_stop(&mut self) -> Result<()> {
        if self.mode == Some(HandyMode::Hssp) {
            self.client.hssp_stop().await?;
        }
        Ok(())
    }

    async fn send(&mut self, targets: &[AxisTarget]) -> Result<()> {
        let Some(t) = targets.iter().find(|t| t.axis == Axis::L0) else {
            return Ok(());
        };
        self.ensure_mode(HandyMode::Hdsp).await?;
        self.client
            .hdsp_xpt(t.position * 100.0, t.duration_ms, true)
            .await
    }

    async fn stop(&mut self) -> Result<()> {
        match self.mode {
            Some(HandyMode::Hssp) => self.client.hssp_stop().await,
            Some(HandyMode::Hamp) => self.client.hamp_stop().await,
            // HDSP has no stop: the device halts at the last target (stopOnTarget).
            _ => Ok(()),
        }
    }
}

/// Bluetooth LE link to The Handy (firmware 3+), speaking the `handyplug` protobuf messages
/// that buttplug.io uses for this device.
pub mod ble {
    use super::*;

    /// GATT service exposed by Handy firmware 3. [verify]
    pub const SERVICE_UUID: u128 = 0x1775244d_6b43_439b_877c_060f2d9bed07;
    /// Write characteristic for handyplug payloads. [verify]
    pub const TX_CHAR_UUID: u128 = 0x1775ff51_6b43_439b_877c_060f2d9bed07;

    fn put_varint(out: &mut Vec<u8>, mut v: u64) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return;
            }
            out.push(b | 0x80);
        }
    }

    fn put_key(out: &mut Vec<u8>, field: u32, wire: u8) {
        put_varint(out, ((field as u64) << 3) | wire as u64);
    }

    fn put_uint(out: &mut Vec<u8>, field: u32, v: u64) {
        if v != 0 {
            put_key(out, field, 0);
            put_varint(out, v);
        }
    }

    fn put_bytes(out: &mut Vec<u8>, field: u32, b: &[u8]) {
        put_key(out, field, 2);
        put_varint(out, b.len() as u64);
        out.extend_from_slice(b);
    }

    /// Encode `Payload { Messages: [Message { LinearCmd { Id, DeviceIndex: 0, Vectors: [{Index:
    /// 0, Duration, Position}] } }] }`.
    ///
    /// [verify] Field numbers follow buttplug's `handyplug.proto` (oneof `LinearCmd = 403`).
    pub fn encode_linear(id: u32, position: f64, duration_ms: u32) -> Vec<u8> {
        let mut sub = Vec::new();
        put_uint(&mut sub, 2, duration_ms as u64);
        let pos = position.clamp(0.0, 1.0);
        if pos != 0.0 {
            put_key(&mut sub, 3, 1);
            sub.extend_from_slice(&pos.to_le_bytes());
        }
        let mut cmd = Vec::new();
        put_uint(&mut cmd, 1, id as u64);
        put_bytes(&mut cmd, 3, &sub);
        let mut msg = Vec::new();
        put_bytes(&mut msg, 403, &cmd);
        let mut payload = Vec::new();
        put_bytes(&mut payload, 1, &msg);
        payload
    }

    /// Encode a keepalive `Ping { Id }` (oneof field 102). [verify]
    pub fn encode_ping(id: u32) -> Vec<u8> {
        let mut ping = Vec::new();
        put_uint(&mut ping, 1, id as u64);
        let mut msg = Vec::new();
        put_bytes(&mut msg, 102, &ping);
        let mut payload = Vec::new();
        put_bytes(&mut payload, 1, &msg);
        payload
    }

    #[cfg(feature = "bluetooth")]
    mod transport {
        use super::*;
        use btleplug::api::{
            Central, Characteristic, Manager as _, Peripheral as _, ScanFilter, WriteType,
        };
        use btleplug::platform::{Manager, Peripheral};
        use uuid::Uuid;

        fn err(e: btleplug::Error) -> HapticsError {
            HapticsError::Device(format!("Bluetooth: {e}"))
        }

        pub struct Link {
            pub peripheral: Peripheral,
            pub tx: Characteristic,
        }

        pub async fn connect(scan_time: Duration) -> Result<Link> {
            let manager = Manager::new().await.map_err(err)?;
            let central = manager
                .adapters()
                .await
                .map_err(err)?
                .into_iter()
                .next()
                .ok_or_else(|| HapticsError::Device("no Bluetooth adapter".into()))?;
            let service = Uuid::from_u128(SERVICE_UUID);
            central
                .start_scan(ScanFilter {
                    services: vec![service],
                })
                .await
                .map_err(err)?;
            let deadline = tokio::time::Instant::now() + scan_time;
            let found = loop {
                let mut hit = None;
                for p in central.peripherals().await.map_err(err)? {
                    if let Ok(Some(props)) = p.properties().await {
                        let named = props
                            .local_name
                            .as_deref()
                            .is_some_and(|n| n.to_ascii_lowercase().contains("handy"));
                        if named || props.services.contains(&service) {
                            hit = Some(p);
                            break;
                        }
                    }
                }
                if let Some(p) = hit {
                    break p;
                }
                if tokio::time::Instant::now() >= deadline {
                    let _ = central.stop_scan().await;
                    return Err(HapticsError::Device(
                        "The Handy was not found over Bluetooth".into(),
                    ));
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            };
            let _ = central.stop_scan().await;
            found.connect().await.map_err(err)?;
            found.discover_services().await.map_err(err)?;
            let tx_uuid = Uuid::from_u128(TX_CHAR_UUID);
            let tx = found
                .characteristics()
                .into_iter()
                .find(|c| c.uuid == tx_uuid)
                .ok_or_else(|| {
                    HapticsError::Device(
                        "Handy BLE characteristic missing (firmware 3+ required)".into(),
                    )
                })?;
            Ok(Link {
                peripheral: found,
                tx,
            })
        }

        pub async fn write(link: &Link, bytes: &[u8]) -> Result<()> {
            link.peripheral
                .write(&link.tx, bytes, WriteType::WithoutResponse)
                .await
                .map_err(err)
        }

        pub async fn disconnect(link: &Link) -> Result<()> {
            link.peripheral.disconnect().await.map_err(err)
        }
    }

    /// [`HapticDevice`] for The Handy over Bluetooth LE (streams LinearCmd, no script sync).
    /// Without the `bluetooth` cargo feature `connect` fails with [`HapticsError::Unsupported`].
    pub struct HandyBle {
        scan_time: Duration,
        next_id: u32,
        #[cfg(feature = "bluetooth")]
        link: Option<transport::Link>,
    }

    impl Default for HandyBle {
        fn default() -> Self {
            Self::new(Duration::from_secs(10))
        }
    }

    impl HandyBle {
        pub fn new(scan_time: Duration) -> Self {
            HandyBle {
                scan_time,
                next_id: 1,
                #[cfg(feature = "bluetooth")]
                link: None,
            }
        }

        #[allow(unused_variables)]
        async fn write(&mut self, bytes: Vec<u8>) -> Result<()> {
            #[cfg(feature = "bluetooth")]
            {
                let link = self.link.as_ref().ok_or(HapticsError::NotConnected)?;
                transport::write(link, &bytes).await
            }
            #[cfg(not(feature = "bluetooth"))]
            Err(HapticsError::NotConnected)
        }
    }

    #[async_trait]
    impl HapticDevice for HandyBle {
        fn info(&self) -> DeviceInfo {
            DeviceInfo {
                name: "The Handy (Bluetooth)".into(),
                axes: vec![(Axis::L0, StreamStyle::NextAction)],
                script_sync: false,
                script_sync_any_speed: false,
                latency_ms: 50, // [verify]
                update_interval_ms: 10,
            }
        }

        async fn connect(&mut self) -> Result<()> {
            #[cfg(feature = "bluetooth")]
            {
                self.link = Some(transport::connect(self.scan_time).await?);
                let id = self.next_id;
                self.next_id += 1;
                self.write(encode_ping(id)).await
            }
            #[cfg(not(feature = "bluetooth"))]
            {
                let _ = self.scan_time;
                Err(HapticsError::Unsupported(
                    "FramePlayer was built without the `bluetooth` feature; use the Handy cloud API or Intiface Central".into(),
                ))
            }
        }

        async fn disconnect(&mut self) -> Result<()> {
            #[cfg(feature = "bluetooth")]
            if let Some(link) = self.link.take() {
                return transport::disconnect(&link).await;
            }
            Ok(())
        }

        async fn send(&mut self, targets: &[AxisTarget]) -> Result<()> {
            let Some(t) = targets.iter().find(|t| t.axis == Axis::L0) else {
                return Ok(());
            };
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1).max(1);
            self.write(encode_linear(id, t.position, t.duration_ms))
                .await
        }

        async fn stop(&mut self) -> Result<()> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::funscript::Script;
    use axum::extract::State;
    use axum::routing::{get, post, put};
    use axum::{Json, Router};
    use std::sync::{Arc, Mutex};

    #[test]
    fn time_offset_math() {
        // Server is 1_000_000 ms ahead; symmetric 40 ms RTT.
        let s = TimeSample {
            sent_ms: 100.0,
            server_ms: 1_000_120.0,
            received_ms: 140.0,
        };
        assert_eq!(s.rtt(), 40.0);
        assert_eq!(s.offset(), 1_000_000.0);
        // Slow, asymmetric samples are discarded in favour of the fastest half.
        let samples = [
            s,
            TimeSample {
                sent_ms: 200.0,
                server_ms: 1_000_210.0,
                received_ms: 220.0,
            }, // 1_000_000
            TimeSample {
                sent_ms: 300.0,
                server_ms: 1_000_310.0,
                received_ms: 700.0,
            }, // slow
            TimeSample {
                sent_ms: 800.0,
                server_ms: 1_000_000.0,
                received_ms: 1300.0,
            }, // slow
            TimeSample {
                sent_ms: 50.0,
                server_ms: 0.0,
                received_ms: 10.0,
            }, // invalid
        ];
        assert_eq!(estimate_server_offset(&samples), Some(1_000_000.0));
        assert_eq!(estimate_server_offset(&[]), None);
        // Odd count keeps ceil(n/2).
        let three = [
            TimeSample {
                sent_ms: 0.0,
                server_ms: 10.0,
                received_ms: 10.0,
            },
            TimeSample {
                sent_ms: 0.0,
                server_ms: 18.0,
                received_ms: 20.0,
            },
            TimeSample {
                sent_ms: 0.0,
                server_ms: 0.0,
                received_ms: 100.0,
            },
        ];
        // offsets: 5, 8 (rtt 10, 20) => mean 6.5
        assert_eq!(estimate_server_offset(&three), Some(6.5));
    }

    #[test]
    fn handyplug_encoding() {
        let b = ble::encode_linear(7, 1.0, 500);
        // Payload.Messages (field 1, LEN)
        assert_eq!(b[0], 0x0a);
        // Message.LinearCmd field 403 => key (403<<3)|2 = 3226 => varint 0x9a 0x19
        assert_eq!(&b[2..4], &[0x9a, 0x19]);
        // LinearCmd.Id = 7
        assert_eq!(&b[5..7], &[0x08, 0x07]);
        // Vectors (field 3 LEN) containing Duration=500 (0x10 0xf4 0x03) and Position=1.0 double
        let tail = &b[7..];
        assert_eq!(tail[0], 0x1a);
        assert_eq!(&tail[2..5], &[0x10, 0xf4, 0x03]);
        assert_eq!(tail[5], 0x19);
        assert_eq!(&tail[6..14], &1.0f64.to_le_bytes());
        assert_eq!(tail.len(), 14);
        assert_eq!(
            ble::encode_ping(1),
            vec![0x0a, 0x05, 0xb2, 0x06, 0x02, 0x08, 0x01]
        );
    }

    #[cfg(not(feature = "bluetooth"))]
    #[tokio::test]
    async fn ble_without_feature_is_clear_error() {
        let mut d = ble::HandyBle::default();
        assert!(
            matches!(d.connect().await, Err(HapticsError::Unsupported(m)) if m.contains("bluetooth"))
        );
    }

    #[derive(Default)]
    struct MockState {
        calls: Mutex<Vec<(String, Value)>>,
    }

    type St = State<Arc<MockState>>;

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    }

    async fn record(st: &MockState, path: &str, body: Value) {
        st.calls.lock().unwrap().push((path.to_string(), body));
    }

    async fn mock_server() -> (String, Arc<MockState>) {
        let st = Arc::new(MockState::default());
        let app = Router::new()
            .route("/api/handy/v2/connected", get(|| async { Json(json!({"connected": true})) }))
            .route("/api/handy/v2/mode", get(|| async { Json(json!({"mode": 0, "result": 0})) }))
            .route(
                "/api/handy/v2/mode",
                put(|State(s): St, Json(b): Json<Value>| async move {
                    record(&s, "mode", b).await;
                    Json(json!({"result": 0}))
                }),
            )
            .route("/api/handy/v2/servertime", get(|| async { Json(json!({"serverTime": now_ms() + 5_000_000})) }))
            .route(
                "/api/handy/v2/hssp/setup",
                put(|State(s): St, Json(b): Json<Value>| async move {
                    record(&s, "setup", b).await;
                    Json(json!({"result": 0}))
                }),
            )
            .route(
                "/api/handy/v2/hssp/play",
                put(|State(s): St, Json(b): Json<Value>| async move {
                    record(&s, "play", b).await;
                    Json(json!({"result": 0}))
                }),
            )
            .route(
                "/api/handy/v2/hssp/stop",
                put(|State(s): St| async move {
                    record(&s, "stop", Value::Null).await;
                    Json(json!({"result": 0}))
                }),
            )
            .route(
                "/api/handy/v2/hdsp/xpt",
                put(|State(s): St, Json(b): Json<Value>| async move {
                    record(&s, "xpt", b).await;
                    Json(json!({"result": 0}))
                }),
            )
            .route(
                "/upload",
                post(|State(s): St, body: axum::body::Bytes| async move {
                    let text = String::from_utf8_lossy(&body).to_string();
                    record(&s, "upload", Value::String(text)).await;
                    Json(json!({"url": "https://example.invalid/s.funscript"}))
                }),
            )
            .route(
                "/api/handy/v2/hamp/stop",
                put(|| async { Json(json!({"error": {"name": "DeviceNotConnected", "message": "x", "connected": false}})) }),
            )
            .with_state(st.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), st)
    }

    #[tokio::test]
    async fn cloud_hssp_and_hdsp_flow() {
        let (base, st) = mock_server().await;
        let mut cfg = HandyConfig::new("KEY");
        cfg.api_base = Url::parse(&format!("{base}/api/handy/v2/")).unwrap();
        cfg.upload_url = Url::parse(&format!("{base}/upload")).unwrap();
        cfg.time_sync_samples = 4;
        let mut dev = HandyCloud::new(cfg).unwrap();
        dev.connect().await.unwrap();
        let est = dev.client().estimated_server_time().unwrap();
        assert!(
            (est - (now_ms() + 5_000_000)).abs() < 1000,
            "offset estimate off: {est}"
        );

        let mut set = ScriptSet::new();
        set.insert(Axis::L0, Script::from_actions([(0, 0.0), (500, 100.0)]));
        assert!(dev.prepare_script(&set).await.unwrap());
        assert!(
            dev.prepare_script(&set).await.unwrap(),
            "second prepare reuses upload"
        );
        dev.sync_play(1234, 1.0).await.unwrap();
        dev.sync_stop().await.unwrap();
        dev.send(&[AxisTarget {
            axis: Axis::L0,
            position: 0.25,
            duration_ms: 300,
        }])
        .await
        .unwrap();

        let calls = st.calls.lock().unwrap().clone();
        let names: Vec<&str> = calls.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec!["upload", "mode", "setup", "play", "stop", "mode", "xpt"]
        );
        assert!(calls[0].1.as_str().unwrap().contains("\"actions\""));
        assert_eq!(calls[1].1, json!({"mode": 1}));
        assert_eq!(calls[2].1["url"], "https://example.invalid/s.funscript");
        assert_eq!(calls[3].1["startTime"], 1234);
        let est = calls[3].1["estimatedServerTime"].as_i64().unwrap();
        assert!((est - (now_ms() + 5_000_000)).abs() < 1000);
        assert_eq!(calls[5].1, json!({"mode": 2}));
        assert_eq!(calls[6].1["position"], 25.0);
        assert_eq!(calls[6].1["duration"], 300);

        // API error objects with connected=false map to NotConnected.
        assert!(matches!(
            dev.client().hamp_stop().await,
            Err(HapticsError::NotConnected)
        ));
    }
}
