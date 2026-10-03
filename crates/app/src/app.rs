//! Glue between the pure [`Controller`] and the real world: the playback
//! engine, the services runtime, and (through small "pending" queues) the
//! XR frame loop. Used by both the XR loop and headless mode; owns no
//! OpenXR / Vulkan state.

use crate::config::Config;
use crate::controller::{
    Controller, Effect, OpenTarget, PlayerOp, SPRITE_KEY_BASE, THUMB_KEY_BASE,
};
use crate::haptics_bridge::HapticsFeed;
use crate::input_map::InputAction;
use crate::runtime::services::{ServiceEvent, ServiceRequest, Services};
use crate::state::Screen;
use crate::view_models;
use fp_core::MediaTime;
use fp_haptics::PlayerUpdate;
use fp_ui::{ToastKind, UiAction};
use fp_video::decode::convert::RgbaImage;
use fp_video::{OpenRequest, PlaybackState, Player, PlayerCommand, VideoOutput};
use std::time::{Duration, Instant};

/// How often the haptics feed is refreshed while nothing else changes.
const HAPTICS_FEED_INTERVAL: Duration = Duration::from_millis(33);
/// Minimum spacing of library re-queries triggered by background scans.
const REQUERY_THROTTLE: Duration = Duration::from_secs(2);

pub struct App {
    pub ctl: Controller,
    pub player: Player,
    pub video: VideoOutput,
    pub services: Services,
    start: Instant,
    last_tick: Instant,
    seek_serial: u64,
    last_feed: Option<(Instant, HapticsFeed)>,
    last_requery: Option<Instant>,
    requery_pending: bool,
    last_haptics_error: Option<String>,
    last_head: Option<glam::Quat>,
    /// Drained by the frame loop.
    pub toasts: Vec<(String, ToastKind)>,
    pub search_text: Option<String>,
    pub recenter: bool,
    pub refresh_rate: Option<f32>,
    /// `(image-cache key, image)` uploads for the renderer.
    pub images: Vec<(u64, RgbaImage)>,
    /// Last open / playback failure (headless exit status).
    pub last_error: Option<String>,
    /// A session was opened at least once (headless exit condition).
    pub opened_once: bool,
}

impl App {
    pub fn new(config: Config, services: Services, player: Player) -> App {
        let video = player.video_output();
        let now = Instant::now();
        let mut app = App {
            ctl: Controller::new(config),
            player,
            video,
            services,
            start: now,
            last_tick: now,
            seek_serial: 0,
            last_feed: None,
            last_requery: None,
            requery_pending: false,
            last_haptics_error: None,
            last_head: None,
            toasts: Vec::new(),
            search_text: None,
            recenter: false,
            refresh_rate: None,
            images: Vec::new(),
            last_error: None,
            opened_once: false,
        };
        let q = app.ctl.requery();
        app.apply(vec![q]);
        app
    }

    pub fn now_s(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    /// Open a path or URI (CLI `--open`, `perf/OPEN`).
    pub fn open_uri(&mut self, uri: &str, start: Option<MediaTime>) {
        let fx = self.ctl.open(
            OpenTarget::Uri(crate::media_input::normalize_uri(uri)),
            start,
        );
        self.apply(fx);
    }

    /// Per-frame update. Returns the frame's dt.
    pub fn tick(&mut self) -> f32 {
        let now = Instant::now();
        let dt = (now - self.last_tick).as_secs_f32().min(0.25);
        self.last_tick = now;

        // Services.
        while let Ok(ev) = self.services.events.try_recv() {
            self.on_service_event(ev);
        }
        // Player events and status.
        let events: Vec<_> = self.player.events().try_iter().collect();
        for ev in &events {
            tracing::debug!("player event: {ev:?}");
            if let fp_video::PlayerEvent::Error(e) = ev {
                self.last_error = Some(e.clone());
            }
            let fx = self.ctl.on_player_event(ev);
            self.apply(fx);
        }
        let fx = self.ctl.on_player_status(self.player.status());
        self.apply(fx);
        let fx = self.ctl.tick(self.now_s(), dt);
        self.apply(fx);

        if self.requery_pending
            && self
                .last_requery
                .is_none_or(|t| t.elapsed() >= REQUERY_THROTTLE)
        {
            self.requery_pending = false;
            self.last_requery = Some(now);
            let q = self.ctl.requery();
            self.apply(vec![q]);
        }

        self.publish(now);
        dt
    }

    /// Remote status + haptics clock (both non-blocking `watch` sends).
    fn publish(&mut self, now: Instant) {
        let rs = view_models::remote_status(&self.ctl);
        self.services.remote_status.send_if_modified(|cur| {
            if *cur != rs {
                *cur = rs;
                true
            } else {
                false
            }
        });
        let feed = HapticsFeed {
            update: PlayerUpdate {
                position: self.ctl.player.position,
                playing: self.ctl.session.is_some()
                    && self.ctl.player.state == PlaybackState::Playing,
                speed: self.ctl.player.speed,
            },
            seek_serial: self.seek_serial,
        };
        let due = match &self.last_feed {
            None => true,
            Some((t, last)) => {
                last.update.playing != feed.update.playing
                    || last.update.speed != feed.update.speed
                    || last.seek_serial != feed.seek_serial
                    || now.duration_since(*t) >= HAPTICS_FEED_INTERVAL
            }
        };
        if due {
            self.last_feed = Some((now, feed));
            self.services.haptics_feed.send_replace(feed);
        }
    }

    pub fn handle_ui(&mut self, actions: Vec<UiAction>) {
        for a in actions {
            tracing::debug!("ui action: {a:?}");
            let fx = self.ctl.handle_ui(a);
            self.apply(fx);
        }
    }

    /// Head orientation for head-tracked (binaural ambisonic) audio; sent
    /// when it changed by more than half a degree.
    pub fn set_head_orientation(&mut self, q: glam::Quat) {
        if self.ctl.session.is_none() {
            return;
        }
        if self
            .last_head
            .is_some_and(|l| l.angle_between(q) < 0.5f32.to_radians())
        {
            return;
        }
        self.last_head = Some(q);
        let _ = self.player.send(PlayerCommand::SetHeadOrientation(q));
    }

    pub fn handle_input(&mut self, actions: &[InputAction]) {
        for a in actions {
            let fx = self.ctl.handle_input(*a);
            self.apply(fx);
        }
    }

    fn on_service_event(&mut self, ev: ServiceEvent) {
        tracing::trace!("service event: {ev:?}");
        match ev {
            ServiceEvent::Sources { list, counts } => {
                self.ctl.library.counts = counts;
                let fx = self.ctl.on_sources(list);
                self.apply(fx);
            }
            ServiceEvent::SourceOnline { id, online } => {
                self.ctl.library.online.insert(id, online);
            }
            ServiceEvent::LibraryChanged => {
                if self.ctl.screen() == Screen::Library || self.ctl.library.items.is_empty() {
                    self.requery_pending = true;
                }
            }
            ServiceEvent::LibraryItems {
                serial,
                items,
                tags,
            } => self.ctl.on_library_items(serial, items, tags),
            ServiceEvent::MediaOpened { meta, input } => {
                let uri = meta.uri.clone();
                match self.ctl.on_media_opened(*meta) {
                    Some(d) => {
                        tracing::info!("opening {uri} (start {:?})", d.start);
                        self.opened_once = true;
                        let req = OpenRequest {
                            input,
                            name: d.name,
                            start: d.start,
                            start_paused: false,
                        };
                        if self.player.send(PlayerCommand::Open(req)).is_err() {
                            self.last_error = Some("playback engine stopped".into());
                        }
                        self.apply(d.effects);
                    }
                    None => tracing::debug!("dropping stale open of {uri}"),
                }
            }
            ServiceEvent::OpenFailed { token, error } => {
                tracing::warn!("open failed: {error}");
                self.last_error = Some(error.clone());
                let fx = self.ctl.on_open_failed(token, &error);
                self.apply(fx);
            }
            ServiceEvent::ScriptsLoaded { token, name, heat } => {
                tracing::info!("funscript loaded: {name}");
                self.ctl.on_scripts_loaded(token, name, heat);
            }
            ServiceEvent::Thumbnail { item, image } => {
                let ok = image.is_some();
                if let Some(img) = image {
                    self.images.push((THUMB_KEY_BASE | item as u64, img));
                }
                self.ctl.on_thumbnail(item, ok);
            }
            ServiceEvent::Sprite { item, info, image } => {
                let sheet = (image.width, image.height);
                self.images.push((SPRITE_KEY_BASE | item as u64, image));
                self.ctl.on_sprite_loaded(item, info, sheet);
            }
            ServiceEvent::Remote(cmd) => {
                tracing::info!("remote: {cmd:?}");
                let fx = self.ctl.handle_remote(cmd);
                self.apply(fx);
            }
            ServiceEvent::RemoteStarted { pairing_url, token } => {
                if let Some(u) = &pairing_url {
                    tracing::info!("web remote ready (pair at the URL shown in Settings → Remote)");
                    tracing::debug!("pairing URL: {u}");
                }
                let fx = self.ctl.set_pairing(pairing_url, token);
                self.apply(fx);
            }
            ServiceEvent::HapticsState { device, error } => {
                self.ctl.runtime.haptics_device = device;
                if error.is_some() && error != self.last_haptics_error {
                    if let Some(e) = &error {
                        self.toasts
                            .push((format!("Haptics: {e}"), ToastKind::Warning));
                    }
                }
                self.last_haptics_error = error;
            }
            ServiceEvent::HapticsDevices(list) => {
                self.ctl.runtime.haptics_devices = list;
                self.ctl.runtime.haptics_scanning = false;
            }
            ServiceEvent::Update(u) => {
                let rt = &mut self.ctl.runtime;
                if let Some(a) = u.available {
                    if !a.is_empty() {
                        rt.update_available = Some(a);
                    }
                }
                rt.update_progress = u.progress;
                if u.status.is_some() {
                    rt.update_status = u.status;
                }
            }
            ServiceEvent::Info(s) => self.toasts.push((s, ToastKind::Info)),
            ServiceEvent::Error(s) => {
                tracing::warn!("{s}");
                self.toasts.push((s, ToastKind::Error));
            }
        }
    }

    /// Carry out controller effects.
    pub fn apply(&mut self, effects: Vec<Effect>) {
        for e in effects {
            match e {
                Effect::Player(op) => self.player_op(op),
                Effect::Open {
                    token,
                    target,
                    start,
                } => self.services.send(ServiceRequest::Open {
                    token,
                    target,
                    start,
                }),
                Effect::Library(op) => self.services.send(ServiceRequest::Library(op)),
                Effect::Haptics(op) => self.services.send(ServiceRequest::Haptics(op)),
                Effect::ConfigChanged { old, new } => {
                    self.services.send(ServiceRequest::ApplyConfig { old, new })
                }
                Effect::Recenter => self.recenter = true,
                Effect::RequestRefreshRate(hz) => self.refresh_rate = Some(hz),
                Effect::Toast(t, k) => {
                    tracing::debug!("toast: {t}");
                    self.toasts.push((t, k));
                }
                Effect::SetSearchText(t) => self.search_text = Some(t),
                Effect::CheckUpdates => self.services.send(ServiceRequest::CheckUpdates),
                Effect::InstallUpdate => self.services.send(ServiceRequest::InstallUpdate),
            }
        }
    }

    fn player_op(&mut self, op: PlayerOp) {
        let cmd = match op {
            PlayerOp::Play => PlayerCommand::Play,
            PlayerOp::Pause => PlayerCommand::Pause,
            PlayerOp::TogglePause => PlayerCommand::TogglePause,
            PlayerOp::Seek(t, mode) => {
                self.seek_serial += 1;
                PlayerCommand::Seek { target: t, mode }
            }
            PlayerOp::SetSpeed(s) => PlayerCommand::SetSpeed(s),
            PlayerOp::Step(n) => PlayerCommand::StepFrames(n),
            PlayerOp::SetLoop(l) => PlayerCommand::SetLoop(l),
            PlayerOp::SelectAudio(t) => PlayerCommand::SelectAudioTrack(t),
            PlayerOp::SelectSubtitle(t) => PlayerCommand::SelectSubtitleTrack(t),
            PlayerOp::SetAvOffset(o) => PlayerCommand::SetAvOffset(o),
            PlayerOp::NextChapter => PlayerCommand::NextChapter,
            PlayerOp::PrevChapter => PlayerCommand::PrevChapter,
            PlayerOp::Stop => PlayerCommand::Stop,
        };
        if self.player.send(cmd).is_err() {
            tracing::error!("playback engine is gone");
        }
    }

    /// Stop playback cleanly and shut the services down.
    pub fn shutdown(mut self) {
        let fx = self.ctl.handle_remote(fp_remote::RemoteCommand::Stop);
        self.apply(fx);
        // Requests are processed in order, so the resume / history writes
        // above land before the shutdown.
        self.services.shutdown(Duration::from_secs(3));
    }
}
