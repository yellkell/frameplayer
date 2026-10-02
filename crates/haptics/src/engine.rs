//! Timeline driver: follows the player clock and drives one [`HapticDevice`].
//!
//! The app pushes [`PlayerUpdate`]s (cheap, non-blocking, safe from the render thread) through
//! a `watch` channel; the engine task extrapolates the media clock between updates, detects
//! discontinuities (seek, pause, speed change, drift beyond a threshold) and resynchronises the
//! device. Script-synced devices get a single `sync_play` per resync; streaming devices get
//! lookahead targets every tick.

use crate::device::{AxisTarget, DeviceInfo, HapticDevice, StreamStyle};
use crate::funscript::{Axis, ScriptSet, DEFAULT_MAX_HEAT_SPEED};
use crate::HapticsError;
use fp_core::MediaTime;
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// What the player reports. Send it whenever anything changes and periodically while playing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlayerUpdate {
    pub position: MediaTime,
    pub playing: bool,
    /// Playback rate (1.0 = normal).
    pub speed: f64,
}

impl Default for PlayerUpdate {
    fn default() -> Self {
        PlayerUpdate {
            position: MediaTime::ZERO,
            playing: false,
            speed: 1.0,
        }
    }
}

/// Per-axis user settings, applied to the scripts before they reach the device.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AxisSettings {
    pub enabled: bool,
    pub invert: bool,
    /// Output range in 0–100 funscript units.
    pub min: f64,
    pub max: f64,
    /// Maximum speed in units/s (`None` = unlimited).
    pub speed_limit: Option<f64>,
}

impl Default for AxisSettings {
    fn default() -> Self {
        AxisSettings {
            enabled: true,
            invert: false,
            min: 0.0,
            max: 100.0,
            speed_limit: None,
        }
    }
}

/// Engine tuning.
#[derive(Debug, Clone, PartialEq)]
pub struct EngineConfig {
    /// Script offset in ms; positive delays the script relative to the video.
    pub offset_ms: i64,
    /// Settings per axis; axes not listed use [`AxisSettings::default`].
    pub axes: BTreeMap<Axis, AxisSettings>,
    /// Extrapolated-vs-reported clock difference that counts as a seek.
    pub drift_threshold_ms: f64,
    /// Drive a vibration axis from stroke speed when the video has no vibration script.
    pub vibe_from_stroke: bool,
    /// Added to the device's own latency for lookahead.
    pub extra_latency_ms: i32,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            offset_ms: 0,
            axes: BTreeMap::new(),
            drift_threshold_ms: 250.0,
            vibe_from_stroke: true,
            extra_latency_ms: 0,
        }
    }
}

impl EngineConfig {
    pub fn axis(&self, axis: Axis) -> AxisSettings {
        self.axes.get(&axis).copied().unwrap_or_default()
    }
}

/// How the device is currently being driven.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyncMode {
    #[default]
    Idle,
    ScriptSync,
    Streaming,
}

/// Observable engine status for the UI.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EngineState {
    pub device: String,
    pub connected: bool,
    pub mode: SyncMode,
    pub scripts_loaded: bool,
    pub last_error: Option<String>,
}

/// Apply per-axis settings (enable, invert, range, speed limit) to raw scripts.
pub fn process_scripts(raw: &ScriptSet, config: &EngineConfig) -> ScriptSet {
    let mut out = ScriptSet::new();
    for (&axis, script) in &raw.axes {
        let s = config.axis(axis);
        if !s.enabled {
            continue;
        }
        let mut script = script.clone();
        if s.invert {
            script.invert();
        }
        if s.min != 0.0 || s.max != 100.0 {
            script.remap_range(s.min, s.max);
        }
        if let Some(limit) = s.speed_limit {
            script.limit_speed(limit);
        }
        out.insert(axis, script);
    }
    out
}

/// Extrapolating media clock anchored at the last player report.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Clock {
    pub anchor_ms: f64,
    pub anchor: Instant,
    pub speed: f64,
    pub playing: bool,
}

impl Clock {
    pub fn at(&self, now: Instant) -> f64 {
        if self.playing {
            self.anchor_ms
                + now.saturating_duration_since(self.anchor).as_secs_f64() * 1000.0 * self.speed
        } else {
            self.anchor_ms
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Snapshot {
    update: PlayerUpdate,
    at: Instant,
    seek_serial: u64,
}

/// Decides when a player report is a discontinuity that needs a device resync.
#[derive(Debug, Default)]
pub(crate) struct SyncTracker {
    pub clock: Option<Clock>,
    serial: u64,
}

impl SyncTracker {
    /// Feed a report; returns `true` if the device must be resynchronised.
    pub fn observe(
        &mut self,
        u: PlayerUpdate,
        at: Instant,
        seek_serial: u64,
        threshold_ms: f64,
    ) -> bool {
        let new = Clock {
            anchor_ms: u.position.as_secs_f64() * 1000.0,
            anchor: at,
            speed: u.speed,
            playing: u.playing,
        };
        let resync = match self.clock {
            None => true,
            Some(old) => {
                seek_serial != self.serial
                    || old.playing != new.playing
                    || (old.speed - new.speed).abs() > 1e-6
                    || (old.at(at) - new.anchor_ms).abs() > threshold_ms
            }
        };
        self.serial = seek_serial;
        self.clock = Some(new);
        resync
    }
}

/// Produces streaming targets from scripts at a given script time.
#[derive(Debug, Default)]
pub(crate) struct Streamer {
    last_next: BTreeMap<Axis, usize>,
    last_value: BTreeMap<Axis, f64>,
}

impl Streamer {
    pub fn reset(&mut self) {
        self.last_next.clear();
        self.last_value.clear();
    }

    /// `t` is script time in ms *including* lookahead; `speed` the playback rate.
    pub fn targets(
        &mut self,
        scripts: &ScriptSet,
        info: &DeviceInfo,
        t: f64,
        speed: f64,
        vibe_from_stroke: bool,
    ) -> Vec<AxisTarget> {
        let speed = if speed > 0.0 { speed } else { 1.0 };
        let interval = info.update_interval_ms.max(1);
        let mut out = Vec::new();
        for &(axis, style) in &info.axes {
            if let Some(script) = scripts.get(axis).filter(|s| !s.is_empty()) {
                match style {
                    StreamStyle::NextAction => {
                        let Some(i) = script.next_index_after(t) else {
                            continue;
                        };
                        if self.last_next.get(&axis) == Some(&i) {
                            continue;
                        }
                        self.last_next.insert(axis, i);
                        let a = script.actions[i];
                        let dur = ((a.at as f64 - t) / speed).round().max(1.0) as u32;
                        out.push(AxisTarget {
                            axis,
                            position: a.pos / 100.0,
                            duration_ms: dur,
                        });
                    }
                    StreamStyle::Interpolated => {
                        let Some(p) = script.position_at(t + interval as f64 * speed) else {
                            continue;
                        };
                        self.push_if_changed(&mut out, axis, p / 100.0, interval);
                    }
                }
            } else if vibe_from_stroke && matches!(axis, Axis::V0 | Axis::V1) {
                if let Some(stroke) = scripts.get(Axis::L0) {
                    let v = (stroke.speed_at(t) * speed / DEFAULT_MAX_HEAT_SPEED as f64)
                        .clamp(0.0, 1.0);
                    self.push_if_changed(&mut out, axis, v, interval);
                }
            }
        }
        out
    }

    fn push_if_changed(&mut self, out: &mut Vec<AxisTarget>, axis: Axis, v: f64, interval: u32) {
        if self
            .last_value
            .get(&axis)
            .is_some_and(|l| (l - v).abs() < 5e-4)
        {
            return;
        }
        self.last_value.insert(axis, v);
        out.push(AxisTarget {
            axis,
            position: v,
            duration_ms: interval,
        });
    }
}

enum Command {
    LoadScripts(ScriptSet),
    ClearScripts,
    SetConfig(EngineConfig),
    Shutdown(oneshot::Sender<()>),
}

/// Handle to the running haptics task. Dropping it stops the task (without a final `stop`);
/// prefer [`HapticsEngine::shutdown`].
pub struct HapticsEngine {
    status_tx: watch::Sender<Snapshot>,
    cmd_tx: mpsc::UnboundedSender<Command>,
    state_rx: watch::Receiver<EngineState>,
    task: Option<JoinHandle<()>>,
}

impl HapticsEngine {
    /// Spawn on the current tokio runtime. Panics outside a runtime; use
    /// [`HapticsEngine::spawn_on`] from non-async code.
    pub fn spawn<D: HapticDevice + 'static>(device: D, config: EngineConfig) -> Self {
        Self::spawn_on(&tokio::runtime::Handle::current(), device, config)
    }

    pub fn spawn_on<D: HapticDevice + 'static>(
        rt: &tokio::runtime::Handle,
        device: D,
        config: EngineConfig,
    ) -> Self {
        let (status_tx, status_rx) = watch::channel(Snapshot {
            update: PlayerUpdate::default(),
            at: now_in(rt),
            seek_serial: 0,
        });
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let info = device.info();
        let (state_tx, state_rx) = watch::channel(EngineState {
            device: info.name.clone(),
            ..Default::default()
        });
        let task = rt.spawn(run(device, config, status_rx, cmd_rx, state_tx));
        HapticsEngine {
            status_tx,
            cmd_tx,
            state_rx,
            task: Some(task),
        }
    }

    /// Report the player state. Never blocks.
    pub fn update(&self, u: PlayerUpdate) {
        self.status_tx.send_modify(|s| {
            s.update = u;
            s.at = Instant::now();
        });
    }

    /// Report an explicit seek (forces a resync even for tiny jumps).
    pub fn seek(&self, position: MediaTime) {
        self.status_tx.send_modify(|s| {
            s.update.position = position;
            s.at = Instant::now();
            s.seek_serial += 1;
        });
    }

    /// Replace the loaded scripts (raw; per-axis settings are applied by the engine).
    pub fn load_scripts(&self, scripts: ScriptSet) {
        let _ = self.cmd_tx.send(Command::LoadScripts(scripts));
    }

    pub fn clear_scripts(&self) {
        let _ = self.cmd_tx.send(Command::ClearScripts);
    }

    pub fn set_config(&self, config: EngineConfig) {
        let _ = self.cmd_tx.send(Command::SetConfig(config));
    }

    pub fn state(&self) -> EngineState {
        self.state_rx.borrow().clone()
    }

    pub fn subscribe_state(&self) -> watch::Receiver<EngineState> {
        self.state_rx.clone()
    }

    /// Stop the device and end the task.
    pub async fn shutdown(mut self) {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.send(Command::Shutdown(tx)).is_ok() {
            let _ = rx.await;
        }
        if let Some(t) = self.task.take() {
            let _ = t.await;
        }
    }
}

impl Drop for HapticsEngine {
    fn drop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }
}

/// `Instant::now()` on `rt`'s clock (matters when the runtime's clock is paused in tests).
fn now_in(rt: &tokio::runtime::Handle) -> Instant {
    let _guard = rt.enter();
    Instant::now()
}

struct Driver<D> {
    device: D,
    info: DeviceInfo,
    config: EngineConfig,
    raw: ScriptSet,
    scripts: ScriptSet,
    device_has_script: bool,
    tracker: SyncTracker,
    streamer: Streamer,
    mode: SyncMode,
    connected: bool,
    stopped: bool,
    state_tx: watch::Sender<EngineState>,
}

const RECONNECT_EVERY: Duration = Duration::from_secs(5);

async fn run<D: HapticDevice>(
    device: D,
    config: EngineConfig,
    mut status_rx: watch::Receiver<Snapshot>,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    state_tx: watch::Sender<EngineState>,
) {
    let info = device.info();
    let mut d = Driver {
        device,
        info,
        config,
        raw: ScriptSet::new(),
        scripts: ScriptSet::new(),
        device_has_script: false,
        tracker: SyncTracker::default(),
        streamer: Streamer::default(),
        mode: SyncMode::Idle,
        connected: false,
        stopped: true,
        state_tx,
    };
    d.try_connect().await;
    let mut last_connect = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(
        d.info.update_interval_ms.max(5) as u64
    ));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(Command::LoadScripts(s)) => { d.raw = s; d.reload().await; }
                Some(Command::ClearScripts) => { d.raw = ScriptSet::new(); d.reload().await; }
                Some(Command::SetConfig(c)) => {
                    let rescript = c.axes != d.config.axes;
                    d.config = c;
                    if rescript { d.reload().await } else { d.resync().await }
                }
                Some(Command::Shutdown(done)) => {
                    d.halt().await;
                    let _ = d.device.disconnect().await;
                    let _ = done.send(());
                    break;
                }
                None => break,
            },
            changed = status_rx.changed() => {
                if changed.is_err() { break; }
                let s = *status_rx.borrow_and_update();
                if d.tracker.observe(s.update, s.at, s.seek_serial, d.config.drift_threshold_ms) {
                    d.resync().await;
                }
            }
            _ = tick.tick() => {
                if !d.connected {
                    if last_connect.elapsed() >= RECONNECT_EVERY {
                        last_connect = Instant::now();
                        if d.try_connect().await { d.reload().await; }
                    }
                } else if d.mode == SyncMode::Streaming {
                    d.stream_tick().await;
                }
            }
        }
    }
}

impl<D: HapticDevice> Driver<D> {
    fn publish(&self, err: Option<String>) {
        self.state_tx.send_modify(|s| {
            s.connected = self.connected;
            s.mode = self.mode;
            s.scripts_loaded = !self.scripts.is_empty();
            if err.is_some() {
                s.last_error = err;
            }
        });
    }

    fn fail(&mut self, what: &str, e: HapticsError) {
        tracing::warn!(device = %self.info.name, "haptics {what} failed: {e}");
        if matches!(
            e,
            HapticsError::NotConnected | HapticsError::WebSocket(_) | HapticsError::Io(_)
        ) {
            self.connected = false;
            self.mode = SyncMode::Idle;
        }
        self.publish(Some(format!("{what}: {e}")));
    }

    async fn try_connect(&mut self) -> bool {
        match self.device.connect().await {
            Ok(()) => {
                self.connected = true;
                self.info = self.device.info();
                self.publish(None);
                true
            }
            Err(e) => {
                self.fail("connect", e);
                false
            }
        }
    }

    async fn reload(&mut self) {
        self.scripts = process_scripts(&self.raw, &self.config);
        self.device_has_script = false;
        if self.connected && self.info.script_sync && !self.scripts.is_empty() {
            match self.device.prepare_script(&self.scripts).await {
                Ok(v) => self.device_has_script = v,
                Err(e) => self.fail("script upload", e),
            }
        }
        self.publish(None);
        self.resync().await;
    }

    fn script_time(&self, now: Instant, lookahead_ms: f64) -> Option<(f64, f64, bool)> {
        let c = self.tracker.clock?;
        let speed = if c.speed > 0.0 { c.speed } else { 1.0 };
        let t = c.at(now) - self.config.offset_ms as f64
            + if c.playing { lookahead_ms * speed } else { 0.0 };
        Some((t, speed, c.playing))
    }

    async fn halt(&mut self) {
        if self.stopped || !self.connected {
            return;
        }
        let r = match self.mode {
            SyncMode::ScriptSync => self.device.sync_stop().await,
            _ => self.device.stop().await,
        };
        self.stopped = true;
        if let Err(e) = r {
            self.fail("stop", e);
        }
    }

    async fn resync(&mut self) {
        self.streamer.reset();
        if !self.connected {
            return;
        }
        let Some((t, speed, playing)) = self.script_time(Instant::now(), 0.0) else {
            return;
        };
        if self.scripts.is_empty() || !playing {
            self.halt().await;
            if self.scripts.is_empty() {
                self.mode = SyncMode::Idle;
            }
            self.publish(None);
            return;
        }
        let use_sync = self.device_has_script
            && (self.info.script_sync_any_speed || (speed - 1.0).abs() < 1e-3);
        if use_sync {
            if self.mode == SyncMode::Streaming && !self.stopped {
                let _ = self.device.stop().await;
            }
            self.mode = SyncMode::ScriptSync;
            self.stopped = false;
            if let Err(e) = self.device.sync_play(t.round() as i64, speed).await {
                self.fail("sync play", e);
            }
        } else {
            if self.mode == SyncMode::ScriptSync && !self.stopped {
                let _ = self.device.sync_stop().await;
            }
            self.mode = SyncMode::Streaming;
            self.stream_tick().await;
        }
        self.publish(None);
    }

    async fn stream_tick(&mut self) {
        let lookahead =
            (self.info.latency_ms as i64 + self.config.extra_latency_ms as i64).max(0) as f64;
        let Some((t, speed, playing)) = self.script_time(Instant::now(), lookahead) else {
            return;
        };
        if !playing {
            self.halt().await;
            return;
        }
        let targets = self.streamer.targets(
            &self.scripts,
            &self.info,
            t,
            speed,
            self.config.vibe_from_stroke,
        );
        if targets.is_empty() {
            return;
        }
        self.stopped = false;
        if let Err(e) = self.device.send(&targets).await {
            self.fail("send", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::funscript::Script;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone, PartialEq)]
    enum Call {
        Connect,
        Prepare(usize),
        SyncPlay(i64, f64),
        SyncStop,
        Send(Vec<AxisTarget>),
        Stop,
        Disconnect,
    }

    struct Mock {
        info: DeviceInfo,
        calls: Arc<Mutex<Vec<Call>>>,
    }

    #[async_trait]
    impl HapticDevice for Mock {
        fn info(&self) -> DeviceInfo {
            self.info.clone()
        }
        async fn connect(&mut self) -> crate::Result<()> {
            self.calls.lock().unwrap().push(Call::Connect);
            Ok(())
        }
        async fn disconnect(&mut self) -> crate::Result<()> {
            self.calls.lock().unwrap().push(Call::Disconnect);
            Ok(())
        }
        async fn prepare_script(&mut self, s: &ScriptSet) -> crate::Result<bool> {
            self.calls.lock().unwrap().push(Call::Prepare(s.axes.len()));
            Ok(self.info.script_sync)
        }
        async fn sync_play(&mut self, t: i64, speed: f64) -> crate::Result<()> {
            self.calls.lock().unwrap().push(Call::SyncPlay(t, speed));
            Ok(())
        }
        async fn sync_stop(&mut self) -> crate::Result<()> {
            self.calls.lock().unwrap().push(Call::SyncStop);
            Ok(())
        }
        async fn send(&mut self, t: &[AxisTarget]) -> crate::Result<()> {
            self.calls.lock().unwrap().push(Call::Send(t.to_vec()));
            Ok(())
        }
        async fn stop(&mut self) -> crate::Result<()> {
            self.calls.lock().unwrap().push(Call::Stop);
            Ok(())
        }
    }

    fn info(style: StreamStyle, sync: bool) -> DeviceInfo {
        DeviceInfo {
            name: "mock".into(),
            axes: vec![(Axis::L0, style), (Axis::V0, StreamStyle::Interpolated)],
            script_sync: sync,
            script_sync_any_speed: false,
            latency_ms: 0,
            update_interval_ms: 10,
        }
    }

    fn scripts() -> ScriptSet {
        let mut s = ScriptSet::new();
        s.insert(
            Axis::L0,
            Script::from_actions([(0, 0.0), (1000, 100.0), (2000, 0.0), (3000, 100.0)]),
        );
        s
    }

    #[test]
    fn clock_extrapolates() {
        let t0 = Instant::now();
        let c = Clock {
            anchor_ms: 1000.0,
            anchor: t0,
            speed: 2.0,
            playing: true,
        };
        assert!((c.at(t0 + Duration::from_millis(500)) - 2000.0).abs() < 1e-6);
        let p = Clock {
            playing: false,
            ..c
        };
        assert_eq!(p.at(t0 + Duration::from_secs(5)), 1000.0);
    }

    #[test]
    fn tracker_detects_discontinuities() {
        let t0 = Instant::now();
        let mut tr = SyncTracker::default();
        let u = |ms: i64, playing: bool, speed: f64| PlayerUpdate {
            position: MediaTime::from_millis(ms),
            playing,
            speed,
        };
        assert!(
            tr.observe(u(0, true, 1.0), t0, 0, 250.0),
            "first report syncs"
        );
        assert!(!tr.observe(
            u(1000, true, 1.0),
            t0 + Duration::from_millis(1000),
            0,
            250.0
        ));
        assert!(
            !tr.observe(
                u(2100, true, 1.0),
                t0 + Duration::from_millis(2000),
                0,
                250.0
            ),
            "small drift"
        );
        assert!(
            tr.observe(
                u(9000, true, 1.0),
                t0 + Duration::from_millis(3000),
                0,
                250.0
            ),
            "jump = seek"
        );
        assert!(
            tr.observe(
                u(9000, false, 1.0),
                t0 + Duration::from_millis(3000),
                0,
                250.0
            ),
            "pause"
        );
        assert!(
            !tr.observe(
                u(9000, false, 1.0),
                t0 + Duration::from_millis(4000),
                0,
                250.0
            ),
            "still paused"
        );
        assert!(
            tr.observe(
                u(9000, false, 1.5),
                t0 + Duration::from_millis(4000),
                0,
                250.0
            ),
            "speed"
        );
        assert!(
            tr.observe(
                u(9000, false, 1.5),
                t0 + Duration::from_millis(4000),
                1,
                250.0
            ),
            "explicit seek"
        );
    }

    #[test]
    fn streamer_next_action_dedupes() {
        let mut st = Streamer::default();
        let i = info(StreamStyle::NextAction, false);
        let s = scripts();
        let t = st.targets(&s, &i, 250.0, 1.0, false);
        assert_eq!(
            t,
            vec![AxisTarget {
                axis: Axis::L0,
                position: 1.0,
                duration_ms: 750
            }]
        );
        assert!(
            st.targets(&s, &i, 500.0, 1.0, false).is_empty(),
            "same next action is not resent"
        );
        let t = st.targets(&s, &i, 1000.0, 2.0, false);
        assert_eq!(
            t,
            vec![AxisTarget {
                axis: Axis::L0,
                position: 0.0,
                duration_ms: 500
            }],
            "speed scales duration"
        );
        st.reset();
        assert_eq!(
            st.targets(&s, &i, 1500.0, 1.0, false).len(),
            1,
            "reset resends"
        );
    }

    #[test]
    fn streamer_interpolated_and_vibe_fallback() {
        let mut st = Streamer::default();
        let i = info(StreamStyle::Interpolated, false);
        let s = scripts();
        let t = st.targets(&s, &i, 490.0, 1.0, true);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].axis, Axis::L0);
        assert!((t[0].position - 0.5).abs() < 1e-9 && t[0].duration_ms == 10);
        assert_eq!(t[1].axis, Axis::V0);
        assert!(
            (t[1].position - 0.25).abs() < 1e-9,
            "100 u/s of 400 => 0.25"
        );
        // Unchanged vibe is not resent; position moved so L0 is.
        let t = st.targets(&s, &i, 500.0, 1.0, true);
        assert_eq!(t.iter().map(|x| x.axis).collect::<Vec<_>>(), vec![Axis::L0]);
        assert!(st.targets(&s, &i, 500.0, 1.0, false).is_empty());
    }

    #[test]
    fn processing_applies_settings() {
        let mut cfg = EngineConfig::default();
        cfg.axes.insert(
            Axis::L0,
            AxisSettings {
                invert: true,
                min: 20.0,
                max: 80.0,
                ..Default::default()
            },
        );
        cfg.axes.insert(
            Axis::R0,
            AxisSettings {
                enabled: false,
                ..Default::default()
            },
        );
        let mut raw = scripts();
        raw.insert(Axis::R0, Script::from_actions([(0, 1.0)]));
        let p = process_scripts(&raw, &cfg);
        assert!(p.get(Axis::R0).is_none());
        let l0 = p.get(Axis::L0).unwrap();
        assert_eq!(l0.actions[0].pos, 80.0);
        assert_eq!(l0.actions[1].pos, 20.0);
    }

    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn engine_script_sync_flow() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let dev = Mock {
            info: info(StreamStyle::NextAction, true),
            calls: calls.clone(),
        };
        let cfg = EngineConfig {
            offset_ms: 100,
            ..Default::default()
        };
        let eng = HapticsEngine::spawn(dev, cfg);
        eng.load_scripts(scripts());
        settle().await;
        eng.update(PlayerUpdate {
            position: MediaTime::from_millis(5000),
            playing: true,
            speed: 1.0,
        });
        settle().await;
        eng.update(PlayerUpdate {
            position: MediaTime::from_millis(5000),
            playing: false,
            speed: 1.0,
        });
        settle().await;
        // Speed != 1 => falls back to streaming.
        eng.update(PlayerUpdate {
            position: MediaTime::from_millis(500),
            playing: true,
            speed: 1.5,
        });
        settle().await;
        assert!(eng.state().connected);
        eng.shutdown().await;
        let c = calls.lock().unwrap().clone();
        assert_eq!(c[0], Call::Connect);
        assert!(c.contains(&Call::Prepare(1)));
        assert!(c.contains(&Call::SyncPlay(4900, 1.0)), "{c:?}");
        let play = c
            .iter()
            .position(|x| *x == Call::SyncPlay(4900, 1.0))
            .unwrap();
        assert_eq!(c[play + 1], Call::SyncStop);
        assert!(
            c.iter()
                .any(|x| matches!(x, Call::Send(t) if t[0].axis == Axis::L0)),
            "{c:?}"
        );
        assert_eq!(c.last(), Some(&Call::Disconnect));
    }

    #[tokio::test(start_paused = true)]
    async fn engine_streams_and_resyncs_on_seek() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut i = info(StreamStyle::NextAction, false);
        i.axes.truncate(1);
        let dev = Mock {
            info: i,
            calls: calls.clone(),
        };
        let eng = HapticsEngine::spawn(dev, EngineConfig::default());
        eng.load_scripts(scripts());
        eng.update(PlayerUpdate {
            position: MediaTime::from_millis(100),
            playing: true,
            speed: 1.0,
        });
        settle().await;
        tokio::time::sleep(Duration::from_millis(1000)).await;
        settle().await;
        eng.seek(MediaTime::from_millis(2500));
        settle().await;
        eng.update(PlayerUpdate {
            position: MediaTime::from_millis(2500),
            playing: false,
            speed: 1.0,
        });
        settle().await;
        eng.shutdown().await;
        let sends: Vec<AxisTarget> = calls
            .lock()
            .unwrap()
            .iter()
            .filter_map(|c| {
                if let Call::Send(t) = c {
                    Some(t[0])
                } else {
                    None
                }
            })
            .collect();
        // 100 ms -> next action at 1000 (pos 1.0, 900 ms); after it, 2000 (pos 0); after the
        // seek to 2500 the next action is 3000 (pos 1.0, 500 ms).
        assert_eq!(
            sends[0],
            AxisTarget {
                axis: Axis::L0,
                position: 1.0,
                duration_ms: 900
            }
        );
        assert_eq!(sends[1].position, 0.0);
        assert_eq!(sends.last().unwrap().position, 1.0);
        assert_eq!(sends.last().unwrap().duration_ms, 500);
        assert!(calls.lock().unwrap().contains(&Call::Stop));
    }
}
