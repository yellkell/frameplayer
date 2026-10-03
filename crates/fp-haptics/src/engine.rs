//! The haptics engine: keeps devices in step with video playback.
//!
//! [`HapticsEngine`] owns a worker thread that wakes about 100 times a
//! second. The player reports its state with [`HapticsEngine::set_status`]
//! (cheap: one short mutex lock, safe to call every frame); the worker
//! extrapolates the position with [`PlaybackStatus::position_at`] and:
//!
//! - for streaming devices, sends the next script action on each axis as a
//!   timed move (target position, time until that action divided by the
//!   playback speed), once per action and never faster than
//!   [`Device::min_interval_ms`];
//! - for script devices (The Handy in HSSP mode), uploads the script on
//!   load and calls [`Device::play_script`] on start, seek and speed
//!   changes;
//! - re-syncs immediately when the reported position jumps more than
//!   [`SEEK_THRESHOLD_S`] from where it was extrapolated to be, or the speed
//!   changes;
//! - stops every device on pause, stop, or when the scripts are cleared.
//!
//! The scheduling logic lives in `Core`, which is driven by explicit
//! timestamps so it can be tested without real time.

use crate::axis::Axis;
use crate::device::{AxisMove, Device, DeviceId, DeviceStatus, SyncMode};
use crate::error::Result;
use crate::script::Script;
use crate::settings::{AxisSettings, HapticsSettings};
use fp_core::playback::PlaybackStatus;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// A reported position further than this (seconds) from the extrapolated
/// one counts as a seek.
pub const SEEK_THRESHOLD_S: f64 = 0.5;

/// Default worker period (100 Hz).
pub const DEFAULT_TICK: Duration = Duration::from_millis(10);

/// Playback speeds below this count as paused.
const MIN_SPEED: f64 = 0.01;

/// Stroke speed (units per second) that maps to full vibration when the
/// vibration axis is derived from the stroke script.
const VIBRATION_FULL_SPEED: f32 = 400.0;

/// Source of the current time in Unix milliseconds, the same base as
/// [`PlaybackStatus::sampled_at_ms`]. Injectable for tests.
pub trait Clock: Send + Sync {
    /// Current time in Unix milliseconds.
    fn now_ms(&self) -> u64;
}

/// The system clock ([`fp_core::playback::now_ms`]).
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        fp_core::playback::now_ms()
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct AxisState {
    /// Index of the script action last sent on this axis.
    last_index: Option<usize>,
    /// Last commanded position.
    last_pos: Option<f32>,
}

struct Slot {
    id: DeviceId,
    name: String,
    dev: Box<dyn Device>,
    mode: SyncMode,
    axis_state: BTreeMap<Axis, AxisState>,
    last_send_ms: Option<u64>,
    /// Script devices: a play command is due.
    pending_play: bool,
    last_error: Option<String>,
    commands_sent: u64,
}

impl Slot {
    fn record(&mut self, r: Result<()>) {
        match r {
            Ok(()) => {
                self.last_error = None;
                self.commands_sent += 1;
            }
            Err(e) => self.last_error = Some(e.to_string()),
        }
    }

    fn resync(&mut self) {
        self.axis_state.clear();
        self.pending_play = true;
    }

    fn halt(&mut self, now: u64) {
        let r = self.dev.stop();
        self.record(r);
        self.last_send_ms = Some(now);
        self.resync();
    }

    fn load(&mut self, scripts: &BTreeMap<Axis, Script>, settings: &HapticsSettings) {
        if self.mode != SyncMode::Script {
            return;
        }
        let list: Vec<(Axis, Script)> = self
            .dev
            .axes()
            .into_iter()
            .filter(|a| settings.sends(&self.name, *a))
            .filter_map(|a| {
                let ax = settings.axis(a);
                scripts
                    .get(&a)
                    .map(|s| (a, s.map_positions(|p| ax.apply(p))))
            })
            .collect();
        let r = self.dev.load_script(&list);
        self.record(r);
        self.pending_play = true;
    }
}

/// Scheduling state, driven by explicit timestamps.
pub(crate) struct Core {
    slots: Vec<Slot>,
    scripts: BTreeMap<Axis, Script>,
    settings: HapticsSettings,
    status: PlaybackStatus,
    active: bool,
}

impl Core {
    pub(crate) fn new(settings: HapticsSettings) -> Core {
        Core {
            slots: Vec::new(),
            scripts: BTreeMap::new(),
            settings,
            status: PlaybackStatus::default(),
            active: false,
        }
    }

    pub(crate) fn add(&mut self, id: DeviceId, dev: Box<dyn Device>) {
        let mut slot = Slot {
            id,
            name: dev.name(),
            mode: dev.sync_mode(),
            dev,
            axis_state: BTreeMap::new(),
            last_send_ms: None,
            pending_play: true,
            last_error: None,
            commands_sent: 0,
        };
        if !self.scripts.is_empty() {
            slot.load(&self.scripts, &self.settings);
        }
        self.slots.push(slot);
    }

    pub(crate) fn remove(&mut self, id: DeviceId, now: u64) -> bool {
        match self.slots.iter().position(|s| s.id == id) {
            Some(i) => {
                let mut slot = self.slots.remove(i);
                slot.halt(now);
                true
            }
            None => false,
        }
    }

    pub(crate) fn load(&mut self, scripts: Vec<(Axis, Script)>, now: u64) {
        let mut map = BTreeMap::new();
        for (axis, script) in scripts {
            if !script.is_empty() {
                map.entry(axis).or_insert(script);
            }
        }
        self.scripts = map;
        if self.scripts.is_empty() && self.active {
            self.active = false;
            self.halt_all(now);
        }
        for slot in &mut self.slots {
            slot.resync();
            slot.load(&self.scripts, &self.settings);
        }
    }

    pub(crate) fn set_settings(&mut self, new: HapticsSettings, now: u64) {
        let old = std::mem::replace(&mut self.settings, new);
        let new = &self.settings;
        let reload = old.axes != new.axes || old.devices != new.devices;
        let resync = reload
            || old.offset_ms != new.offset_ms
            || old.speed_limit != new.speed_limit
            || old.stroke_to_vibration != new.stroke_to_vibration;
        for slot in &mut self.slots {
            let was_on = old.device(&slot.name).enabled;
            let is_on = new.device(&slot.name).enabled;
            if was_on && !is_on && self.active {
                slot.halt(now);
            }
            if reload {
                slot.load(&self.scripts, new);
            }
            if resync || (is_on && !was_on) {
                slot.resync();
            }
        }
    }

    pub(crate) fn set_status(&mut self, st: PlaybackStatus) {
        let old = &self.status;
        let jumped = old.playing
            && st.playing
            && ((old.position_at(st.sampled_at_ms) - st.position).abs() > SEEK_THRESHOLD_S
                || (old.speed - st.speed).abs() > 1e-3
                || old.location != st.location);
        self.status = st;
        if jumped {
            for slot in &mut self.slots {
                slot.resync();
            }
        }
    }

    fn halt_all(&mut self, now: u64) {
        for slot in &mut self.slots {
            if self.settings.device(&slot.name).enabled {
                slot.halt(now);
            } else {
                slot.resync();
            }
        }
    }

    pub(crate) fn tick(&mut self, now: u64) {
        let speed = self.status.speed;
        let playing = self.status.playing && speed > MIN_SPEED && !self.scripts.is_empty();
        if !playing {
            if self.active {
                self.active = false;
                self.halt_all(now);
            }
            return;
        }
        if !self.active {
            self.active = true;
            for slot in &mut self.slots {
                slot.resync();
            }
        }
        let t = (self.status.position_at(now) * 1000.0).round() as i64 + self.settings.offset_ms;
        for slot in &mut self.slots {
            if !self.settings.device(&slot.name).enabled {
                continue;
            }
            if let Some(last) = slot.last_send_ms {
                if now.saturating_sub(last) < u64::from(slot.dev.min_interval_ms()) {
                    continue;
                }
            }
            match slot.mode {
                SyncMode::Script => {
                    if slot.pending_play {
                        let r = slot.dev.play_script(t, speed);
                        slot.record(r);
                        slot.pending_play = false;
                        slot.last_send_ms = Some(now);
                    }
                }
                SyncMode::Stream => {
                    let moves = plan_moves(slot, &self.scripts, &self.settings, t, speed);
                    if !moves.is_empty() {
                        let r = slot.dev.move_axes(&moves);
                        slot.record(r);
                        slot.last_send_ms = Some(now);
                    }
                }
            }
        }
    }

    pub(crate) fn statuses(&self) -> Vec<DeviceStatus> {
        self.slots
            .iter()
            .map(|s| DeviceStatus {
                id: s.id,
                name: s.name.clone(),
                axes: s.dev.axes(),
                connected: s.dev.is_connected(),
                enabled: self.settings.device(&s.name).enabled,
                sync_mode: s.mode,
                notice: s.dev.notice(),
                last_error: s.last_error.clone(),
                commands_sent: s.commands_sent,
            })
            .collect()
    }

    pub(crate) fn shutdown(&mut self, now: u64) {
        for slot in &mut self.slots {
            slot.halt(now);
        }
        self.active = false;
    }
}

/// The moves due for a streaming device at script time `t`.
fn plan_moves(
    slot: &mut Slot,
    scripts: &BTreeMap<Axis, Script>,
    settings: &HapticsSettings,
    t: i64,
    speed: f64,
) -> Vec<AxisMove> {
    let mut moves = Vec::new();
    for axis in slot.dev.axes() {
        if !settings.sends(&slot.name, axis) {
            continue;
        }
        let ax: AxisSettings = settings.axis(axis);
        let next = match scripts.get(&axis) {
            Some(sc) => sc
                .next_action_after(t)
                .map(|(i, a)| (i, ax.apply(a.pos), a.at)),
            None if axis == Axis::V0 && settings.stroke_to_vibration => {
                scripts.get(&Axis::L0).and_then(|sc| {
                    sc.next_action_after(t).map(|(i, a)| {
                        let level = (sc.speed_into(i) / VIBRATION_FULL_SPEED).clamp(0.0, 1.0);
                        (i, ax.apply(level), a.at)
                    })
                })
            }
            None => None,
        };
        let Some((index, target, at)) = next else {
            continue;
        };
        let state = slot.axis_state.entry(axis).or_default();
        if state.last_index == Some(index) {
            continue;
        }
        let duration = ((at - t) as f64 / speed)
            .round()
            .clamp(1.0, f64::from(u32::MAX)) as u32;
        let mut pos = target;
        if settings.speed_limit > 0.0 && axis.is_positional() {
            if let Some(prev) = state.last_pos {
                let max_delta = settings.speed_limit / 100.0 * duration as f32 / 1000.0;
                pos = prev + (pos - prev).clamp(-max_delta, max_delta);
            }
        }
        state.last_index = Some(index);
        state.last_pos = Some(pos);
        moves.push(AxisMove {
            axis,
            pos,
            duration_ms: duration,
        });
    }
    moves
}

enum Command {
    Add(DeviceId, Box<dyn Device>),
    Remove(DeviceId),
    Load(Vec<(Axis, Script)>),
    Settings(HapticsSettings),
    Shutdown,
}

struct Shared {
    /// (generation, status): the generation lets the worker skip unchanged
    /// statuses.
    status: Mutex<(u64, PlaybackStatus)>,
    devices: Mutex<Vec<DeviceStatus>>,
    settings: Mutex<HapticsSettings>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic while holding one of these locks cannot leave the plain data
    // inconsistent, so recover from poisoning.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Refreshes the shared device list from the worker's view. Membership is
/// owned by [`HapticsEngine::add_device`] and
/// [`HapticsEngine::remove_device`] (which update the list immediately), so
/// only entries already listed are updated: a tick racing with an add or
/// remove can neither drop a just-added device nor resurrect a removed one.
fn publish_statuses(shared: &Shared, statuses: Vec<DeviceStatus>) {
    let mut list = lock(&shared.devices);
    for status in statuses {
        if let Some(entry) = list.iter_mut().find(|d| d.id == status.id) {
            *entry = status;
        }
    }
}

/// Plays loaded scripts on connected devices in sync with the player.
///
/// All methods take `&self`; share the engine with `Arc` between the
/// player, the UI and the remote API. Dropping it stops every device and
/// joins the worker.
pub struct HapticsEngine {
    tx: mpsc::Sender<Command>,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    next_id: AtomicU64,
}

impl HapticsEngine {
    /// Starts an engine on the system clock at 100 Hz with default
    /// settings.
    ///
    /// # Panics
    ///
    /// If the OS cannot create the worker thread, like
    /// [`std::thread::spawn`]. Use [`HapticsEngine::with_clock`] to handle
    /// that as an error.
    pub fn new() -> HapticsEngine {
        match HapticsEngine::with_clock(Arc::new(SystemClock), DEFAULT_TICK) {
            Ok(engine) => engine,
            Err(e) => panic!("failed to start the haptics worker thread: {e}"),
        }
    }

    /// Starts an engine with an explicit clock and worker period.
    pub fn with_clock(clock: Arc<dyn Clock>, tick: Duration) -> Result<HapticsEngine> {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            status: Mutex::new((0, PlaybackStatus::default())),
            devices: Mutex::new(Vec::new()),
            settings: Mutex::new(HapticsSettings::default()),
        });
        let worker_shared = Arc::clone(&shared);
        let tick = tick.max(Duration::from_millis(1));
        let worker = std::thread::Builder::new()
            .name("fp-haptics".into())
            .spawn(move || run_worker(rx, worker_shared, clock, tick))?;
        Ok(HapticsEngine {
            tx,
            shared,
            worker: Some(worker),
            next_id: AtomicU64::new(1),
        })
    }

    /// Replaces the loaded scripts. Empty scripts are ignored; when an axis
    /// appears twice the first wins.
    pub fn load(&self, scripts: Vec<(Axis, Script)>) {
        let _ = self.tx.send(Command::Load(scripts));
    }

    /// Unloads all scripts and stops the devices.
    pub fn clear(&self) {
        let _ = self.tx.send(Command::Load(Vec::new()));
    }

    /// Reports the player's state. Cheap; call it every frame or whenever
    /// something changes.
    pub fn set_status(&self, status: PlaybackStatus) {
        let mut g = lock(&self.shared.status);
        g.0 = g.0.wrapping_add(1);
        g.1 = status;
    }

    /// Adds a device; it starts following playback on the next tick.
    pub fn add_device(&self, device: Box<dyn Device>) -> DeviceId {
        let id = DeviceId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let status = DeviceStatus {
            id,
            name: device.name(),
            axes: device.axes(),
            connected: device.is_connected(),
            enabled: lock(&self.shared.settings).device(&device.name()).enabled,
            sync_mode: device.sync_mode(),
            notice: device.notice(),
            last_error: None,
            commands_sent: 0,
        };
        lock(&self.shared.devices).push(status);
        let _ = self.tx.send(Command::Add(id, device));
        id
    }

    /// Stops and drops a device. Returns false when the id is unknown.
    pub fn remove_device(&self, id: DeviceId) -> bool {
        let mut devices = lock(&self.shared.devices);
        let known = devices.iter().any(|d| d.id == id);
        devices.retain(|d| d.id != id);
        drop(devices);
        if known {
            let _ = self.tx.send(Command::Remove(id));
        }
        known
    }

    /// The devices and their state, refreshed every tick.
    pub fn devices(&self) -> Vec<DeviceStatus> {
        lock(&self.shared.devices).clone()
    }

    /// The current settings.
    pub fn settings(&self) -> HapticsSettings {
        lock(&self.shared.settings).clone()
    }

    /// Replaces the settings.
    pub fn set_settings(&self, settings: HapticsSettings) {
        self.update_settings(|s| *s = settings);
    }

    /// Changes the settings in place.
    pub fn update_settings(&self, f: impl FnOnce(&mut HapticsSettings)) {
        let mut g = lock(&self.shared.settings);
        f(&mut g);
        let _ = self.tx.send(Command::Settings(g.clone()));
    }

    /// Sets the global script offset in milliseconds (see
    /// [`HapticsSettings::offset_ms`]).
    pub fn set_offset_ms(&self, offset_ms: i64) {
        self.update_settings(|s| s.offset_ms = offset_ms);
    }

    /// Sets the output mapping of one axis.
    pub fn set_axis_settings(&self, axis: Axis, settings: AxisSettings) {
        self.update_settings(|s| {
            s.axes.insert(axis, settings);
        });
    }

    /// Sets the speed limit in funscript units per second (0 = none).
    pub fn set_speed_limit(&self, units_per_second: f32) {
        self.update_settings(|s| s.speed_limit = units_per_second.max(0.0));
    }

    /// Switches a device (by name) on or off.
    pub fn set_device_enabled(&self, name: &str, enabled: bool) {
        self.update_settings(|s| s.device_mut(name).enabled = enabled);
    }

    /// Switches one axis of a device (by name) on or off.
    pub fn set_device_axis_enabled(&self, name: &str, axis: Axis, enabled: bool) {
        self.update_settings(|s| {
            let d = s.device_mut(name);
            if enabled {
                d.disabled_axes.remove(&axis);
            } else {
                d.disabled_axes.insert(axis);
            }
        });
    }
}

impl Default for HapticsEngine {
    fn default() -> Self {
        HapticsEngine::new()
    }
}

impl Drop for HapticsEngine {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn run_worker(
    rx: mpsc::Receiver<Command>,
    shared: Arc<Shared>,
    clock: Arc<dyn Clock>,
    tick: Duration,
) {
    let mut core = Core::new(lock(&shared.settings).clone());
    let mut seen_generation = 0u64;
    let mut next = Instant::now();
    // Returns false on shutdown.
    let apply = |core: &mut Core, cmd: Command| -> bool {
        let now = clock.now_ms();
        match cmd {
            Command::Add(id, dev) => core.add(id, dev),
            Command::Remove(id) => {
                core.remove(id, now);
            }
            Command::Load(scripts) => core.load(scripts, now),
            Command::Settings(s) => core.set_settings(s, now),
            Command::Shutdown => return false,
        }
        true
    };
    'run: loop {
        match rx.recv_timeout(next.saturating_duration_since(Instant::now())) {
            Ok(cmd) => {
                if !apply(&mut core, cmd) {
                    break 'run;
                }
                while let Ok(cmd) = rx.try_recv() {
                    if !apply(&mut core, cmd) {
                        break 'run;
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break 'run,
        }
        let now_instant = Instant::now();
        if now_instant < next {
            continue;
        }
        {
            let g = lock(&shared.status);
            if g.0 != seen_generation {
                seen_generation = g.0;
                core.set_status(g.1.clone());
            }
        }
        core.tick(clock.now_ms());
        publish_statuses(&shared, core.statuses());
        next += tick;
        if next < now_instant {
            next = now_instant + tick;
        }
    }
    core.shutdown(clock.now_ms());
    publish_statuses(&shared, core.statuses());
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::error::Error;
    use crate::script::Action;

    #[derive(Clone, Debug, PartialEq)]
    pub(crate) enum Event {
        Move(Vec<AxisMove>),
        Stop,
        Load(Vec<(Axis, Script)>),
        Play(i64, f64),
    }

    pub(crate) struct FakeDevice {
        pub name: String,
        pub axes: Vec<Axis>,
        pub interval: u32,
        pub mode: SyncMode,
        pub log: Arc<Mutex<Vec<Event>>>,
    }

    impl FakeDevice {
        pub(crate) fn new(axes: &[Axis]) -> (FakeDevice, Arc<Mutex<Vec<Event>>>) {
            let log = Arc::new(Mutex::new(Vec::new()));
            (
                FakeDevice {
                    name: "fake".into(),
                    axes: axes.to_vec(),
                    interval: 0,
                    mode: SyncMode::Stream,
                    log: Arc::clone(&log),
                },
                log,
            )
        }
    }

    impl Device for FakeDevice {
        fn name(&self) -> String {
            self.name.clone()
        }
        fn axes(&self) -> Vec<Axis> {
            self.axes.clone()
        }
        fn move_to(&mut self, axis: Axis, pos: f32, duration_ms: u32) -> Result<()> {
            self.move_axes(&[AxisMove {
                axis,
                pos,
                duration_ms,
            }])
        }
        fn move_axes(&mut self, moves: &[AxisMove]) -> Result<()> {
            lock(&self.log).push(Event::Move(moves.to_vec()));
            Ok(())
        }
        fn stop(&mut self) -> Result<()> {
            lock(&self.log).push(Event::Stop);
            Ok(())
        }
        fn is_connected(&self) -> bool {
            true
        }
        fn min_interval_ms(&self) -> u32 {
            self.interval
        }
        fn sync_mode(&self) -> SyncMode {
            self.mode
        }
        fn load_script(&mut self, scripts: &[(Axis, Script)]) -> Result<()> {
            lock(&self.log).push(Event::Load(scripts.to_vec()));
            Ok(())
        }
        fn play_script(&mut self, t: i64, speed: f64) -> Result<()> {
            if self.mode == SyncMode::Stream {
                return Err(Error::Unsupported("stream".into()));
            }
            lock(&self.log).push(Event::Play(t, speed));
            Ok(())
        }
    }

    const T0: u64 = 1_000_000;

    fn script(points: &[(i64, f32)]) -> Script {
        Script::new(points.iter().map(|&(t, p)| Action::new(t, p)).collect())
    }

    fn playing(position: f64, speed: f64, at: u64) -> PlaybackStatus {
        PlaybackStatus {
            location: "video.mp4".into(),
            duration: 3600.0,
            position,
            speed,
            playing: true,
            sampled_at_ms: at,
            ..Default::default()
        }
    }

    fn take(log: &Arc<Mutex<Vec<Event>>>) -> Vec<Event> {
        std::mem::take(&mut *lock(log))
    }

    fn mv(axis: Axis, pos: f32, duration_ms: u32) -> Event {
        Event::Move(vec![AxisMove {
            axis,
            pos,
            duration_ms,
        }])
    }

    fn core_with(dev: FakeDevice, scripts: Vec<(Axis, Script)>) -> Core {
        let mut core = Core::new(HapticsSettings::default());
        core.add(DeviceId(1), Box::new(dev));
        core.load(scripts, T0);
        core
    }

    fn stroke() -> Script {
        script(&[
            (0, 0.0),
            (500, 1.0),
            (1000, 0.0),
            (1500, 1.0),
            (10_000, 0.0),
        ])
    }

    #[test]
    fn plays_pauses_and_streams_each_action_once() {
        let (dev, log) = FakeDevice::new(&[Axis::L0]);
        let mut core = core_with(dev, vec![(Axis::L0, stroke())]);
        core.tick(T0);
        assert!(take(&log).is_empty(), "nothing while stopped");

        core.set_status(playing(0.0, 1.0, T0));
        core.tick(T0);
        assert_eq!(take(&log), vec![mv(Axis::L0, 1.0, 500)]);
        for k in 1..50 {
            core.tick(T0 + k * 10);
        }
        assert!(take(&log).is_empty(), "same action is not resent");
        core.tick(T0 + 500);
        assert_eq!(take(&log), vec![mv(Axis::L0, 0.0, 500)]);
        core.tick(T0 + 1010);
        assert_eq!(take(&log), vec![mv(Axis::L0, 1.0, 490)]);

        let mut paused = playing(1.02, 1.0, T0 + 1020);
        paused.playing = false;
        core.set_status(paused.clone());
        core.tick(T0 + 1020);
        assert_eq!(take(&log), vec![Event::Stop]);
        core.tick(T0 + 1030);
        assert!(take(&log).is_empty(), "stop is sent once");

        // Resume: the current action is sent again.
        core.set_status(playing(1.02, 1.0, T0 + 5000));
        core.tick(T0 + 5000);
        assert_eq!(take(&log), vec![mv(Axis::L0, 1.0, 480)]);

        // Past the end of the script nothing more is sent.
        core.set_status(playing(20.0, 1.0, T0 + 6000));
        core.tick(T0 + 6000);
        assert!(take(&log).is_empty());

        // Clearing the scripts stops the devices.
        core.set_status(playing(1.0, 1.0, T0 + 7000));
        core.tick(T0 + 7000);
        assert_eq!(take(&log).len(), 1);
        core.load(Vec::new(), T0 + 7010);
        core.tick(T0 + 7020);
        assert_eq!(take(&log), vec![Event::Stop]);
    }

    #[test]
    fn seek_resyncs_even_within_one_segment() {
        let (dev, log) = FakeDevice::new(&[Axis::L0]);
        let mut core = core_with(dev, vec![(Axis::L0, script(&[(0, 0.0), (10_000, 1.0)]))]);
        core.set_status(playing(1.0, 1.0, T0));
        core.tick(T0);
        assert_eq!(take(&log), vec![mv(Axis::L0, 1.0, 9000)]);
        // Small drift corrections are not seeks.
        core.set_status(playing(1.3, 1.0, T0 + 100));
        core.tick(T0 + 100);
        assert!(take(&log).is_empty());
        // A jump to 6 s targets the same action but must resend it.
        core.set_status(playing(6.0, 1.0, T0 + 200));
        core.tick(T0 + 200);
        assert_eq!(take(&log), vec![mv(Axis::L0, 1.0, 4000)]);
        // Backwards too.
        core.set_status(playing(2.0, 1.0, T0 + 300));
        core.tick(T0 + 300);
        assert_eq!(take(&log), vec![mv(Axis::L0, 1.0, 8000)]);
    }

    #[test]
    fn speed_scales_durations_and_change_resyncs() {
        let (dev, log) = FakeDevice::new(&[Axis::L0]);
        let mut core = core_with(dev, vec![(Axis::L0, stroke())]);
        core.set_status(playing(0.0, 2.0, T0));
        core.tick(T0);
        assert_eq!(take(&log), vec![mv(Axis::L0, 1.0, 250)]);
        // 100 ms of wall time at 2x is 200 ms of video.
        core.set_status(playing(0.2, 0.5, T0 + 100));
        core.tick(T0 + 100);
        assert_eq!(take(&log), vec![mv(Axis::L0, 1.0, 600)]);
    }

    #[test]
    fn offset_shifts_script_time() {
        let (dev, log) = FakeDevice::new(&[Axis::L0]);
        let mut core = core_with(dev, vec![(Axis::L0, stroke())]);
        core.set_settings(
            HapticsSettings {
                offset_ms: 100,
                ..Default::default()
            },
            T0,
        );
        core.set_status(playing(0.0, 1.0, T0));
        core.tick(T0);
        assert_eq!(take(&log), vec![mv(Axis::L0, 1.0, 400)]);
        core.set_settings(
            HapticsSettings {
                offset_ms: -200,
                ..Default::default()
            },
            T0 + 250,
        );
        // Video 250 ms - 200 ms = script 50 ms.
        core.tick(T0 + 250);
        assert_eq!(take(&log), vec![mv(Axis::L0, 1.0, 450)]);
    }

    #[test]
    fn range_invert_and_axis_switches() {
        let (dev, log) = FakeDevice::new(&[Axis::L0, Axis::R0]);
        let mut core = core_with(
            dev,
            vec![
                (Axis::L0, stroke()),
                (Axis::R0, script(&[(0, 0.5), (400, 1.0)])),
            ],
        );
        let mut s = HapticsSettings::default();
        *s.axis_mut(Axis::L0) = AxisSettings {
            enabled: true,
            min: 0.2,
            max: 0.8,
            invert: true,
        };
        core.set_settings(s.clone(), T0);
        core.set_status(playing(0.0, 1.0, T0));
        core.tick(T0);
        let ev = take(&log);
        let Event::Move(m) = &ev[0] else {
            panic!("{ev:?}")
        };
        assert_eq!(m.len(), 2, "both axes in one batch");
        assert_eq!(m[0].axis, Axis::L0);
        assert!((m[0].pos - 0.2).abs() < 1e-6);
        assert_eq!(m[1].axis, Axis::R0);
        assert_eq!(m[1].pos, 1.0);

        // Disabling R0 for this device, then L0 globally.
        s.device_mut("fake").disabled_axes.insert(Axis::R0);
        core.set_settings(s.clone(), T0 + 10);
        core.tick(T0 + 10);
        let ev = take(&log);
        assert_eq!(ev.len(), 1);
        let Event::Move(m) = &ev[0] else {
            panic!("{ev:?}")
        };
        assert_eq!(m.iter().map(|m| m.axis).collect::<Vec<_>>(), vec![Axis::L0]);
        s.axis_mut(Axis::L0).enabled = false;
        core.set_settings(s.clone(), T0 + 20);
        core.tick(T0 + 20);
        assert!(take(&log).is_empty());

        // Disabling the device while playing stops it.
        s.axis_mut(Axis::L0).enabled = true;
        core.set_settings(s.clone(), T0 + 30);
        core.tick(T0 + 30);
        assert_eq!(take(&log).len(), 1);
        s.device_mut("fake").enabled = false;
        core.set_settings(s.clone(), T0 + 40);
        assert_eq!(take(&log), vec![Event::Stop]);
        core.tick(T0 + 50);
        assert!(take(&log).is_empty());
        assert!(!core.statuses()[0].enabled);
    }

    #[test]
    fn respects_device_rate_limit() {
        let (mut dev, log) = FakeDevice::new(&[Axis::L0]);
        dev.interval = 100;
        // An action every 20 ms for 2 s.
        let pts: Vec<(i64, f32)> = (0..100).map(|i| (i * 20, (i % 2) as f32)).collect();
        let mut core = core_with(dev, vec![(Axis::L0, script(&pts))]);
        core.set_status(playing(0.0, 1.0, T0));
        let mut sent_at = Vec::new();
        for k in 0..200 {
            let now = T0 + k * 10;
            core.tick(now);
            if !take(&log).is_empty() {
                sent_at.push(now);
            }
        }
        assert!(
            sent_at.len() >= 15 && sent_at.len() <= 20,
            "{}",
            sent_at.len()
        );
        assert!(sent_at.windows(2).all(|w| w[1] - w[0] >= 100));
    }

    #[test]
    fn speed_limit_shortens_moves() {
        let (dev, log) = FakeDevice::new(&[Axis::L0]);
        let mut core = core_with(
            dev,
            vec![(Axis::L0, script(&[(0, 0.0), (100, 0.0), (200, 1.0)]))],
        );
        core.set_settings(
            HapticsSettings {
                speed_limit: 200.0,
                ..Default::default()
            },
            T0,
        );
        core.set_status(playing(0.0, 1.0, T0));
        core.tick(T0);
        assert_eq!(take(&log), vec![mv(Axis::L0, 0.0, 100)]);
        core.tick(T0 + 100);
        // 100 units in 100 ms is 1000 u/s; limited to 200 u/s → 20 units.
        let ev = take(&log);
        let Event::Move(m) = &ev[0] else {
            panic!("{ev:?}")
        };
        assert!((m[0].pos - 0.2).abs() < 1e-5, "{m:?}");
    }

    #[test]
    fn vibration_from_stroke_speed() {
        let (dev, log) = FakeDevice::new(&[Axis::V0]);
        // 0→100 in 500 ms is 200 u/s → half intensity.
        let mut core = core_with(dev, vec![(Axis::L0, stroke())]);
        core.set_status(playing(0.1, 1.0, T0));
        core.tick(T0);
        assert_eq!(take(&log), vec![mv(Axis::V0, 0.5, 400)]);
        core.set_settings(
            HapticsSettings {
                stroke_to_vibration: false,
                ..Default::default()
            },
            T0 + 10,
        );
        core.tick(T0 + 10);
        assert!(take(&log).is_empty());
    }

    #[test]
    fn script_devices_load_play_and_stop() {
        let (mut dev, log) = FakeDevice::new(&[Axis::L0]);
        dev.mode = SyncMode::Script;
        dev.interval = 200;
        let mut core = Core::new(HapticsSettings::default());
        core.add(DeviceId(7), Box::new(dev));
        assert!(take(&log).is_empty());
        core.load(vec![(Axis::L0, stroke()), (Axis::R1, stroke())], T0);
        let ev = take(&log);
        let [Event::Load(l)] = ev.as_slice() else {
            panic!("{ev:?}")
        };
        assert_eq!(l.len(), 1, "only the device's axes");
        assert_eq!(l[0].0, Axis::L0);

        core.set_status(playing(2.0, 1.0, T0));
        core.tick(T0);
        assert_eq!(take(&log), vec![Event::Play(2000, 1.0)]);
        core.tick(T0 + 50);
        assert!(take(&log).is_empty());

        // A seek inside the rate limit is deferred, not lost.
        core.set_status(playing(30.0, 1.0, T0 + 100));
        core.tick(T0 + 100);
        assert!(take(&log).is_empty());
        core.tick(T0 + 150);
        assert!(take(&log).is_empty());
        core.tick(T0 + 200);
        assert_eq!(take(&log), vec![Event::Play(30_100, 1.0)]);

        // Speed change replays; range settings re-upload.
        core.set_status(playing(31.0, 1.5, T0 + 1000));
        core.tick(T0 + 1000);
        assert_eq!(take(&log), vec![Event::Play(31_000, 1.5)]);
        let mut s = HapticsSettings::default();
        s.axis_mut(Axis::L0).max = 0.5;
        core.set_settings(s, T0 + 1100);
        let ev = take(&log);
        let [Event::Load(l)] = ev.as_slice() else {
            panic!("{ev:?}")
        };
        assert_eq!(l[0].1.actions()[1].pos, 0.5);
        core.tick(T0 + 1300);
        assert_eq!(take(&log), vec![Event::Play(31_450, 1.5)]);

        let mut paused = playing(31.0, 1.5, T0 + 1400);
        paused.playing = false;
        core.set_status(paused);
        core.tick(T0 + 1400);
        assert_eq!(take(&log), vec![Event::Stop]);

        assert!(core.remove(DeviceId(7), T0 + 1500));
        assert_eq!(take(&log), vec![Event::Stop]);
        assert!(!core.remove(DeviceId(7), T0 + 1500));
    }

    #[test]
    fn records_device_errors() {
        let (dev, _log) = FakeDevice::new(&[Axis::L0]);
        let mut core = core_with(dev, vec![(Axis::L0, stroke())]);
        // A streaming device asked to play a script reports an error, which
        // the status list shows.
        core.slots[0].mode = SyncMode::Script;
        core.set_status(playing(0.0, 1.0, T0));
        core.tick(T0);
        let st = core.statuses();
        assert!(
            st[0]
                .last_error
                .as_deref()
                .unwrap_or("")
                .contains("unsupported")
        );
        assert_eq!(st[0].commands_sent, 0);
    }

    /// Waits up to 5 s for `cond`.
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

    #[test]
    fn threaded_engine_end_to_end() {
        let engine = HapticsEngine::new();
        let (dev, log) = FakeDevice::new(&[Axis::L0]);
        let id = engine.add_device(Box::new(dev));
        assert_eq!(engine.devices().len(), 1);
        assert_eq!(engine.devices()[0].name, "fake");
        let pts: Vec<(i64, f32)> = (0..2000).map(|i| (i * 100, (i % 2) as f32)).collect();
        engine.load(vec![(Axis::L0, script(&pts))]);
        engine.set_status(PlaybackStatus {
            playing: true,
            speed: 1.0,
            position: 1.0,
            sampled_at_ms: fp_core::playback::now_ms(),
            ..Default::default()
        });
        assert!(wait_for(|| lock(&log)
            .iter()
            .any(|e| matches!(e, Event::Move(_)))));
        assert!(wait_for(|| engine.devices()[0].commands_sent > 0));

        engine.set_offset_ms(30);
        assert_eq!(engine.settings().offset_ms, 30);

        engine.set_status(PlaybackStatus {
            playing: false,
            position: 2.0,
            sampled_at_ms: fp_core::playback::now_ms(),
            ..Default::default()
        });
        assert!(wait_for(|| lock(&log).last() == Some(&Event::Stop)));

        assert!(engine.remove_device(id));
        assert!(!engine.remove_device(id));
        // Membership changes are visible at once and never undone by a tick.
        assert!(engine.devices().is_empty());
        std::thread::sleep(Duration::from_millis(30));
        assert!(engine.devices().is_empty());
        drop(engine);
    }

    #[test]
    fn manual_clock_engine_and_shutdown_stops_devices() {
        struct Fixed(AtomicU64);
        impl Clock for Fixed {
            fn now_ms(&self) -> u64 {
                self.0.load(Ordering::Relaxed)
            }
        }
        let clock = Arc::new(Fixed(AtomicU64::new(T0)));
        let engine = HapticsEngine::with_clock(clock.clone(), Duration::from_millis(2)).unwrap();
        let (dev, log) = FakeDevice::new(&[Axis::L0]);
        engine.add_device(Box::new(dev));
        engine.load(vec![(Axis::L0, stroke())]);
        engine.set_status(playing(0.0, 1.0, T0));
        assert!(wait_for(|| !lock(&log).is_empty()));
        // The clock is frozen, so exactly one move is sent however many
        // ticks run.
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(take(&log), vec![mv(Axis::L0, 1.0, 500)]);
        clock.0.store(T0 + 600, Ordering::Relaxed);
        assert!(wait_for(|| !lock(&log).is_empty()));
        assert_eq!(take(&log), vec![mv(Axis::L0, 0.0, 400)]);
        drop(engine);
        assert_eq!(take(&log), vec![Event::Stop]);
    }
}
