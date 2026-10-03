//! The application: owns the services, the open video and the panels, and
//! turns controller input into UI events and playback commands each frame.

use crate::jobs::Jobs;
use crate::playback::{self, OpenRequest, Opened, Playback};
use crate::services::Services;
use crate::settings::Settings;
use crate::ui::{self, Action, Browse, UiState, UpdateStatus, View};
use crate::world::{Panel, Pointer, anchor_from_head, place};
use fp_core::PlaybackStatus;
use fp_core::format::Projection;
use fp_library::Library;
use fp_media::{PlayerState, VideoFrame};
use fp_render::{QuadDraw, Renderer, VideoParams};
use fp_xr::Hand;
use glam::{Mat4, Quat, Vec2, Vec3};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

/// Per-frame input from the XR session (or the preview simulator).
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameInput {
    /// Seconds since start.
    pub time: f64,
    pub dt: f32,
    pub head: Option<(Vec3, Quat)>,
    pub hands: [Hand; 2],
    pub passthrough_available: bool,
}

/// What to draw and do this frame.
pub struct FrameOutput {
    pub frame: Option<Arc<VideoFrame>>,
    pub video: VideoParams,
    pub quads: Vec<QuadDraw>,
    pub passthrough: bool,
    /// (hand, amplitude, milliseconds)
    pub buzz: Vec<(usize, f32, i64)>,
    pub quit: bool,
}

enum Job {
    Opened(u64, Box<Result<Opened, String>>),
    Listed {
        source: String,
        location: Option<String>,
        result: Result<Vec<fp_core::source::Entry>, String>,
    },
    Status(String),
    ScanDone(String),
    Imported(Result<usize, String>),
    Dlna(Result<Vec<fp_sources::DlnaDevice>, String>),
    Device(
        crate::settings::HapticDeviceConfig,
        Result<Box<dyn fp_haptics::Device>, String>,
    ),
    Update(UpdateStatus, Option<Box<fp_updater::Update>>),
}

const MAIN: usize = 0;
const BAR: usize = 1;
const ADJUST: usize = 2;
const KEYBOARD: usize = 3;

pub struct App {
    pub settings: Settings,
    saved: Settings,
    services: Services,
    pub ui: UiState,
    thumbs: ui::thumbs::Thumbs,
    panels: [Panel; 4],
    subs: Panel,
    sub_key: String,
    sub_textures: Vec<egui::TextureHandle>,
    pointer: Pointer,
    pub playback: Option<Playback>,
    open_seq: u64,
    jobs: Jobs<Job>,
    anchor: Option<Mat4>,
    reanchor: bool,
    ui_visible: bool,
    show_browser: bool,
    last_activity: Instant,
    controls: crate::controls::Controls,
    trigger_prev: [f32; 2],
    /// Yaw/pitch when a dome drag began.
    drag_start: Option<(f32, f32)>,
    /// The list the open video came from, for next/previous.
    queue: Vec<OpenRequest>,
    queue_pos: usize,
    last_status: Option<PlaybackStatus>,
    update: Option<fp_updater::Update>,
    /// Removable drives seen at the last check, and when that was.
    mounts: Vec<std::path::PathBuf>,
    mounts_checked: Instant,
    /// Browser command to run after FramePlayer exits (WebXR hand-off).
    pub handoff: Option<Vec<String>>,
    /// The Web XR tab's embedded browser, once started.
    pub web: Option<crate::webview::WebView>,
    /// The browser being started (it takes a few seconds): its result.
    web_starting: Option<std::sync::mpsc::Receiver<Result<crate::webview::WebView, String>>>,
    web_error: Option<String>,
    /// A page in the web view asked for the headset: quit so it can have it.
    pub yield_to_web: bool,
    quit: bool,
}

fn panel(px: [u32; 2], width_m: f32, ppp: f32) -> Panel {
    let size = Vec2::new(width_m, width_m * px[1] as f32 / px[0] as f32);
    Panel::new(px, size, ppp)
}

impl App {
    pub fn new(settings: Settings, library: Library) -> App {
        let services = Services::new(&settings, library);
        let thumbs = ui::thumbs::Thumbs::new(services.opener.clone());
        let mut panels = [
            panel([1600, 1000], 1.8, 1.25),
            panel([1400, 268], 1.2, 1.25),
            panel([900, 1020], 0.75, 1.25),
            panel([1200, 456], 1.0, 1.25),
        ];
        panels[BAR].refresh = Some(Duration::from_millis(250));
        panels[ADJUST].refresh = Some(Duration::from_millis(1000));
        panels[MAIN].refresh = Some(Duration::from_millis(1000));
        let subs = panel([1600, 320], 1.6, 1.0);
        subs.ctx
            .style_mut(|s| s.visuals.panel_fill = egui::Color32::TRANSPARENT);
        let mut app = App {
            saved: settings.clone(),
            settings,
            services,
            ui: UiState::default(),
            thumbs,
            panels,
            subs,
            sub_key: String::new(),
            sub_textures: Vec::new(),
            pointer: Pointer::default(),
            playback: None,
            open_seq: 0,
            jobs: Jobs::new(),
            anchor: None,
            reanchor: true,
            ui_visible: true,
            show_browser: true,
            last_activity: Instant::now(),
            controls: Default::default(),
            trigger_prev: [0.0; 2],
            drag_start: None,
            queue: Vec::new(),
            queue_pos: 0,
            last_status: None,
            update: None,
            mounts: crate::services::removable_mounts(),
            mounts_checked: Instant::now(),
            handoff: None,
            web: None,
            web_starting: None,
            web_error: None,
            yield_to_web: false,
            quit: false,
        };
        // Back from a page's VR session: reopen the web view.
        if std::env::var("FRAMEPLAYER_RESUMED").is_ok_and(|v| v == "web") {
            app.ui.screen = ui::Screen::Web;
        }
        app.rescan(false);
        for cfg in app.settings.haptic_devices.clone() {
            app.connect_device(cfg);
        }
        if app.settings.check_updates && fp_updater::RELEASE_PUBLIC_KEY.is_some() {
            app.check_updates();
        }
        app
    }

    pub fn set_about(&mut self, about: Vec<(String, String)>) {
        self.ui.about = about;
    }

    /// Opens a location (path or URL) as if picked in the UI.
    pub fn open(&mut self, req: OpenRequest) {
        self.apply(Action::Open(req));
    }

    /// One line about the open video and the UI, for the preview harness.
    pub fn status_line(&self) -> String {
        let ui = format!(
            "ui_visible={} browser={} adjust={}",
            self.ui_visible, self.show_browser, self.ui.adjust_open
        );
        match &self.playback {
            Some(p) => format!(
                "{} pos={:.1} vol={:.2} zoom={:.2} yaw={:.1} pitch={:.1} queue={}/{} paused={} {ui}",
                p.title,
                p.player.position(),
                p.player.volume(),
                p.settings.zoom,
                p.settings.yaw,
                p.settings.pitch,
                self.queue_pos + 1,
                self.queue.len(),
                p.player.is_paused()
            ),
            None => format!("no video, screen={:?} {ui}", self.ui.screen),
        }
    }

    /// World position of a fractional point on a visible panel (tests).
    pub fn panel_point(&self, name: &str, x: f32, y: f32) -> Option<Vec3> {
        let i = match name {
            "main" => MAIN,
            "bar" => BAR,
            "adjust" => ADJUST,
            "keyboard" => KEYBOARD,
            _ => return None,
        };
        let p = &self.panels[i];
        if !p.visible {
            return None;
        }
        let pts = p.points();
        Some(p.world_point(egui::pos2(x * pts.x, y * pts.y)))
    }

    // ---- background jobs -------------------------------------------------

    fn rescan(&mut self, force: bool) {
        let mut folders = self.settings.library_folders.clone();
        if self.settings.index_removable {
            // Each drive on its own, so an absent card never marks videos
            // on another drive missing.
            let extra: Vec<_> = self
                .mounts
                .iter()
                .filter(|m| !folders.iter().any(|f| m.starts_with(f)))
                .cloned()
                .collect();
            folders.extend(extra);
        }
        let lib = self.services.library.clone();
        let tx = self.jobs.sender();
        self.ui.scan_status = Some("Scanning…".into());
        self.jobs.spawn("scan", move || {
            let opts = fp_library::ScanOptions {
                force,
                ..Default::default()
            };
            let (mut added, mut missing, mut errors) = (0, 0, Vec::new());
            for f in &folders {
                let mut last = Instant::now();
                let r = lib.scan_folder(f, "local", &opts, |p| {
                    if last.elapsed() > Duration::from_millis(300) {
                        last = Instant::now();
                        let _ = tx.send(Job::Status(format!(
                            "Scanning {}: {} videos",
                            f.display(),
                            p.videos_found
                        )));
                    }
                });
                match r {
                    Ok(rep) => {
                        added += rep.added;
                        missing += rep.missing;
                    }
                    Err(e) => errors.push(format!("{}: {e}", f.display())),
                }
            }
            let mut msg = format!("Scan finished: {added} new, {missing} missing");
            if !errors.is_empty() {
                msg += &format!(" · {}", errors.join("; "));
            }
            Job::ScanDone(msg)
        });
    }

    fn browse(&mut self, source: String, location: Option<String>) {
        let Some(src) = self.services.opener.source(&source) else {
            self.ui.toast("That source is not available");
            return;
        };
        let b = self.ui.browse.get_or_insert_with(Browse::default);
        if b.source != source {
            *b = Browse {
                source: source.clone(),
                ..Default::default()
            };
        }
        b.location = location.clone();
        b.loading = true;
        b.error = None;
        let lib = self.services.library.clone();
        self.jobs.spawn("list", move || {
            let result = src.list(location.as_deref()).map(|mut e| {
                fp_sources::sort_entries(&mut e);
                // Thumbnails the library already made for these videos.
                for entry in e.iter_mut().filter(|e| {
                    e.thumbnail_url.is_none() && e.kind == fp_core::source::EntryKind::Video
                }) {
                    if let Ok(Some(r)) = lib.get_by_location(&entry.location) {
                        entry.thumbnail_url = r.thumbnail.map(|p| p.display().to_string());
                    }
                }
                e
            });
            let result = result.map_err(|e| {
                let removable = source == crate::services::REMOVABLE_SOURCE;
                if removable && crate::services::removable_mounts().is_empty() {
                    "No microSD card or USB drive is inserted.".to_string()
                } else {
                    e.to_string()
                }
            });
            Job::Listed {
                source,
                location,
                result,
            }
        });
    }

    fn import(&mut self, source: String, location: Option<String>) {
        let Some(src) = self.services.opener.source(&source) else {
            return;
        };
        let lib = self.services.library.clone();
        self.ui.toast("Adding videos to the library…");
        self.jobs.spawn("import", move || {
            let mut queue = vec![(location, 0)];
            let mut videos = Vec::new();
            while let Some((loc, depth)) = queue.pop() {
                let entries = match src.list(loc.as_deref()) {
                    Ok(e) => e,
                    Err(e) if depth == 0 => return Job::Imported(Err(e.to_string())),
                    Err(_) => continue,
                };
                for e in entries {
                    match e.kind {
                        fp_core::source::EntryKind::Directory if depth < 4 => {
                            queue.push((Some(e.location.clone()), depth + 1))
                        }
                        fp_core::source::EntryKind::Video => videos.push(e),
                        _ => {}
                    }
                }
                if videos.len() > 5000 {
                    break;
                }
            }
            let n = videos.len();
            Job::Imported(
                lib.upsert_entries(src.id(), &videos)
                    .map(|_| n)
                    .map_err(|e| e.to_string()),
            )
        });
    }

    fn connect_device(&mut self, cfg: crate::settings::HapticDeviceConfig) {
        self.jobs.spawn("haptics", move || {
            let r = Services::connect_device(&cfg);
            Job::Device(cfg, r)
        });
    }

    fn check_updates(&mut self) {
        self.ui.update = UpdateStatus::Checking;
        let channel = if self.settings.update_channel == "beta" {
            fp_updater::Channel::Beta
        } else {
            fp_updater::Channel::Stable
        };
        self.jobs.spawn("update-check", move || {
            match fp_updater::check(
                fp_updater::DEFAULT_MANIFEST_URL,
                env!("CARGO_PKG_VERSION"),
                channel,
            ) {
                Ok(Some(u)) => Job::Update(
                    UpdateStatus::Available {
                        version: u.version.to_string(),
                        notes: u.notes.clone(),
                    },
                    Some(Box::new(u)),
                ),
                Ok(None) => Job::Update(UpdateStatus::UpToDate, None),
                Err(e) => Job::Update(UpdateStatus::Failed(e.to_string()), None),
            }
        });
    }

    fn install_update(&mut self) {
        let Some(u) = self.update.clone() else { return };
        let tx = self.jobs.sender();
        self.ui.update = UpdateStatus::Downloading {
            done: 0,
            total: u.artifact.size,
        };
        self.jobs.spawn("update-install", move || {
            let dir = fp_core::dirs::data_dir().join("updates");
            let cancel = AtomicBool::new(false);
            let mut last = Instant::now();
            let r = std::fs::create_dir_all(&dir)
                .map_err(|e| e.to_string())
                .and_then(|_| {
                    fp_updater::download(
                        &u,
                        &dir,
                        |done, total| {
                            if last.elapsed() > Duration::from_millis(250) {
                                last = Instant::now();
                                let _ = tx.send(Job::Update(
                                    UpdateStatus::Downloading { done, total },
                                    None,
                                ));
                            }
                        },
                        &cancel,
                    )
                    .map_err(|e| e.to_string())
                })
                .and_then(|zip| {
                    let install = fp_updater::current_install_dir().map_err(|e| e.to_string())?;
                    fp_updater::install::install_expecting(&zip, &install, &u.version)
                        .map_err(|e| e.to_string())
                });
            match r {
                Ok(v) => Job::Update(
                    UpdateStatus::Installed {
                        version: v.to_string(),
                    },
                    None,
                ),
                Err(e) => Job::Update(UpdateStatus::Failed(e), None),
            }
        });
    }

    fn poll_jobs(&mut self) {
        for job in self.jobs.poll() {
            match job {
                Job::Opened(seq, r) => {
                    if seq != self.open_seq {
                        if let Ok(o) = *r {
                            o.playback.close(&self.services.library);
                        }
                        continue;
                    }
                    self.ui.opening = None;
                    match *r {
                        Ok(o) => {
                            if let Some(old) = self.playback.take() {
                                old.close(&self.services.library);
                            }
                            self.services.haptics.load(o.scripts);
                            self.playback = Some(o.playback);
                            self.ui.open_error = None;
                            self.ui.adjust_open = false;
                            self.show_browser = false;
                            self.ui_visible = true;
                            self.last_activity = Instant::now();
                            self.sub_key.clear();
                            self.ui.invalidate();
                        }
                        Err(e) => {
                            log::warn!("open failed: {e}");
                            self.ui.open_error = Some(e);
                        }
                    }
                }
                Job::Listed {
                    source,
                    location,
                    result,
                } => {
                    if let Some(b) = self
                        .ui
                        .browse
                        .as_mut()
                        .filter(|b| b.source == source && b.location == location)
                    {
                        b.loading = false;
                        match result {
                            Ok(e) => {
                                b.entries = e;
                                b.error = None;
                            }
                            Err(e) => b.error = Some(e),
                        }
                    }
                }
                Job::Status(s) => self.ui.scan_status = Some(s),
                Job::ScanDone(s) => {
                    log::info!("{s}");
                    self.ui.scan_status = Some(s);
                    self.ui.invalidate();
                    self.services.wake_worker();
                }
                Job::Imported(r) => {
                    match r {
                        Ok(n) => self.ui.toast(format!("Added {n} videos to the library")),
                        Err(e) => self.ui.toast(format!("Could not add folder: {e}")),
                    }
                    self.ui.invalidate();
                    self.services.wake_worker();
                }
                Job::Dlna(r) => {
                    self.ui.dlna_searching = false;
                    match r {
                        Ok(d) => self.ui.dlna_found = d,
                        Err(e) => self.ui.toast(format!("Network search failed: {e}")),
                    }
                }
                Job::Device(cfg, r) => match r {
                    Ok(d) => {
                        let id = self.services.haptics.add_device(d);
                        self.services.haptic_devices.retain(|(c, _, _)| *c != cfg);
                        self.services.haptic_devices.push((cfg, Some(id), None));
                    }
                    Err(e) => {
                        log::warn!("haptics {}: {e}", cfg.label());
                        self.ui.toast(format!("{}: {e}", cfg.label()));
                        self.services.haptic_devices.retain(|(c, _, _)| *c != cfg);
                        self.services.haptic_devices.push((cfg, None, Some(e)));
                    }
                },
                Job::Update(status, update) => {
                    if let Some(u) = update {
                        self.update = Some(*u);
                    }
                    self.ui.update = status;
                }
            }
        }
        if let Some(rx) = &self.services.worker_events {
            for ev in rx.try_iter() {
                match ev {
                    fp_library::WorkerEvent::Processed { done, total, .. }
                    | fp_library::WorkerEvent::Failed { done, total, .. } => {
                        self.ui.worker_status =
                            Some(format!("Preparing thumbnails {done}/{total}"));
                        if done % 12 == 0 || done == total {
                            self.ui.invalidate();
                            self.thumbs.retry_failed();
                        }
                    }
                    fp_library::WorkerEvent::Idle { .. } => {
                        if self.ui.worker_status.take().is_some() {
                            self.ui.invalidate();
                            self.thumbs.retry_failed();
                        }
                    }
                    fp_library::WorkerEvent::Error(e) => log::warn!("metadata worker: {e}"),
                    _ => {}
                }
            }
        }
    }

    /// Notices microSD cards and USB drives being inserted or removed.
    fn check_mounts(&mut self) {
        if self.mounts_checked.elapsed() < Duration::from_secs(3) {
            return;
        }
        self.mounts_checked = Instant::now();
        let now = crate::services::removable_mounts();
        if now == self.mounts {
            return;
        }
        let added: Vec<_> = now
            .iter()
            .filter(|m| !self.mounts.contains(m))
            .cloned()
            .collect();
        self.mounts = now;
        if let Some(m) = added.first() {
            let name = m
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            log::info!("drive mounted: {}", m.display());
            self.ui.toast(format!("Found drive \"{name}\""));
            if self.settings.index_removable {
                self.rescan(false);
            }
        } else {
            log::info!("a drive was removed");
        }
        self.ui.invalidate();
        if let Some(b) = self
            .ui
            .browse
            .as_ref()
            .filter(|b| b.source == crate::services::REMOVABLE_SOURCE)
        {
            let loc = b.location.clone();
            self.browse(crate::services::REMOVABLE_SOURCE.into(), loc);
        }
    }

    // ---- actions -----------------------------------------------------------

    fn apply(&mut self, a: Action) {
        let lib = self.services.library.clone();
        match a {
            Action::Open(req) => {
                self.queue.clear();
                self.open_request(req);
            }
            Action::OpenList(list, pos) => {
                if let Some(req) = list.get(pos).cloned() {
                    self.queue = list;
                    self.queue_pos = pos;
                    self.open_request(req);
                }
            }
            Action::TogglePause => {
                if let Some(p) = &self.playback {
                    if p.player.state() == PlayerState::Ended {
                        p.player.seek(0.0, false);
                        p.player.play();
                    } else {
                        p.player.toggle_pause();
                    }
                }
            }
            Action::Seek(t) => {
                if let Some(p) = &self.playback {
                    p.player.seek(t, true);
                }
            }
            Action::SeekRelative(d) => {
                if let Some(p) = &self.playback {
                    p.player.seek_relative(d);
                }
            }
            Action::SetSpeed(s) => {
                if let Some(p) = &self.playback {
                    p.player.set_speed(s.clamp(0.25, 3.0));
                }
            }
            Action::SetVolume(v) => {
                if let Some(p) = &self.playback {
                    p.player.set_volume(v);
                }
                self.settings.volume = v;
            }
            Action::ClosePlayback => {
                if let Some(p) = self.playback.take() {
                    p.close(&lib);
                }
                self.services.haptics.clear();
                self.show_browser = true;
                self.ui_visible = true;
                self.ui.invalidate();
            }
            Action::SetFormat(f) => {
                if let Some(p) = &mut self.playback {
                    p.set_format(&lib, f);
                }
                self.ui.invalidate();
            }
            Action::SaveView => {
                if let Some(p) = &mut self.playback {
                    p.save_settings(&lib);
                    self.ui.toast("Adjustments saved");
                }
            }
            Action::ResetView => {
                if let Some(p) = &mut self.playback {
                    p.settings = self.settings.default_view;
                    p.keyframes = Default::default();
                    p.settings_dirty = true;
                }
            }
            Action::AddKeyframe => {
                if let Some(p) = &mut self.playback {
                    let t = p.player.position();
                    p.keyframes.insert(t, p.settings);
                    p.settings_dirty = true;
                }
            }
            Action::ClearKeyframes => {
                if let Some(p) = &mut self.playback {
                    p.keyframes = Default::default();
                    p.settings_dirty = true;
                }
            }
            Action::AddBookmark => {
                if let Some(p) = &mut self.playback {
                    p.add_bookmark(&lib);
                    self.ui.toast("Bookmark added");
                }
            }
            Action::SelectAudio(i) => {
                if let Some(p) = &self.playback {
                    p.player.select_audio(i);
                }
            }
            Action::SelectSubtitleStream(i) => {
                if let Some(p) = &self.playback {
                    p.player.select_subtitle(i);
                }
            }
            Action::SelectSubtitleFile(i) => {
                if let Some(p) = &mut self.playback {
                    p.player.clear_external_subtitles();
                    p.active_subtitle_file = None;
                    if let Some((name, loc)) = i.and_then(|i| p.subtitle_files.get(i).cloned()) {
                        match self.services.opener.open(&loc).and_then(|s| {
                            p.player.load_subtitles(s, &name).map_err(|e| e.to_string())
                        }) {
                            Ok(_) => p.active_subtitle_file = i,
                            Err(e) => self.ui.toast(format!("Subtitles: {e}")),
                        }
                    }
                }
            }
            Action::SetFavorite(id, f) => {
                let _ = lib.set_favorite(id, f);
                self.ui.invalidate();
            }
            Action::SetRating(id, r) => {
                let _ = lib.set_rating(id, r);
                self.ui.invalidate();
            }
            Action::SetUserFormat(id, f) => {
                let _ = lib.set_user_format(id, f);
                self.ui.invalidate();
            }
            Action::RemoveMedia(id) => {
                let _ = lib.remove_media(id);
                self.ui.invalidate();
            }
            Action::Rescan { force } => self.rescan(force),
            Action::RetryFailed => {
                let n = lib.retry_failed_probes().unwrap_or(0);
                self.services.wake_worker();
                self.ui.toast(format!("Retrying {n} videos"));
            }
            Action::ClearHistory => {
                let _ = lib.clear_history();
                self.ui.invalidate();
            }
            Action::AddFolder(p) => {
                if !self.settings.library_folders.contains(&p) {
                    self.settings.library_folders.push(p);
                    self.rescan(false);
                }
            }
            Action::RemoveFolder(i) => {
                if i < self.settings.library_folders.len() {
                    self.settings.library_folders.remove(i);
                }
            }
            Action::AddSource(cfg) => {
                let mut list = self.services.source_configs.clone();
                list.push(cfg.clone());
                let errors = self.services.set_sources(list);
                match errors.into_iter().find(|e| e.starts_with(cfg.name())) {
                    Some(e) => self.ui.toast(e),
                    None => {
                        self.ui.toast(format!("Added {}", cfg.name()));
                        self.browse(cfg.id().to_string(), None);
                    }
                }
            }
            Action::RemoveSource(id) => {
                let list = self
                    .services
                    .source_configs
                    .iter()
                    .filter(|c| c.id() != id)
                    .cloned()
                    .collect();
                self.services.set_sources(list);
            }
            Action::Browse { source, location } => self.browse(source, location),
            Action::ImportFolder { source, location } => self.import(source, location),
            Action::DiscoverDlna => {
                self.ui.dlna_searching = true;
                self.jobs.spawn("dlna", || {
                    Job::Dlna(
                        fp_sources::discover(Duration::from_secs(3)).map_err(|e| e.to_string()),
                    )
                });
            }
            Action::AddDevice(cfg) => {
                if !self.settings.haptic_devices.contains(&cfg) {
                    self.settings.haptic_devices.push(cfg.clone());
                }
                self.connect_device(cfg);
            }
            Action::RemoveDevice(i) => {
                if i < self.settings.haptic_devices.len() {
                    let cfg = self.settings.haptic_devices.remove(i);
                    if let Some(pos) = self
                        .services
                        .haptic_devices
                        .iter()
                        .position(|(c, _, _)| *c == cfg)
                    {
                        let (_, id, _) = self.services.haptic_devices.remove(pos);
                        if let Some(id) = id {
                            self.services.haptics.remove_device(id);
                        }
                    }
                }
            }
            Action::ReconnectDevices => {
                for (_, id, _) in self.services.haptic_devices.drain(..) {
                    if let Some(id) = id {
                        self.services.haptics.remove_device(id);
                    }
                }
                for cfg in self.settings.haptic_devices.clone() {
                    self.connect_device(cfg);
                }
            }
            Action::RegenerateToken => {
                if let Some(h) = &self.services.remote
                    && let Err(e) = h.regenerate_token()
                {
                    self.ui.toast(e.to_string());
                }
            }
            Action::CheckUpdates => self.check_updates(),
            Action::InstallUpdate => self.install_update(),
            Action::LaunchWeb(url) => match (
                crate::webxr::normalize_url(&url),
                crate::webxr::find_browser(),
            ) {
                (Err(e), _) => self.ui.toast(e),
                (Ok(_), None) => {
                    self.ui.screen = ui::Screen::Web;
                    self.ui
                        .toast("Install Chromium XR first (see the Web XR tab)");
                }
                (Ok(url), Some(b)) => {
                    let home = std::env::var_os("HOME")
                        .map(std::path::PathBuf::from)
                        .unwrap_or_default();
                    self.settings.web_home = url.clone();
                    if let Err(e) = crate::webxr::write_home_url(&home, &url) {
                        log::warn!("can't record the browser's page: {e}");
                    }
                    // Started as its own Steam entry, the browser gets a
                    // panel in the headset; started by us it stays hidden.
                    match crate::webxr::steam_appid(&home) {
                        Some(appid) => match crate::webxr::launch_via_steam(appid) {
                            Ok(()) => {
                                log::info!("opening {url} in Chromium XR (Steam app {appid})");
                                self.quit = true;
                            }
                            Err(e) => self.ui.toast(format!("Can't start Chromium XR: {e}")),
                        },
                        None => {
                            log::info!("handing the headset to {} for {url}", b.path().display());
                            self.handoff = Some(crate::webxr::command(&b, &url, &home));
                            self.quit = true;
                        }
                    }
                }
            },
            Action::Recenter => self.reanchor = true,
            Action::TogglePassthrough => self.settings.passthrough = !self.settings.passthrough,
            Action::ShowBrowser(show) => {
                self.show_browser = show;
                if show {
                    self.ui.invalidate();
                }
            }
            Action::Quit => self.quit = true,
            Action::WebRetry => self.web_error = None,
        }
    }

    /// Saves and applies settings edited through the UI.
    fn sync_settings(&mut self) {
        if self.settings == self.saved {
            return;
        }
        if self.settings.remote != self.saved.remote {
            self.services.apply_remote(&self.settings.remote);
        }
        if self.settings.haptics != self.saved.haptics {
            self.services
                .haptics
                .set_settings(self.settings.haptics.clone());
        }
        if self.settings.ui_scale != self.saved.ui_scale {
            for p in self.panels.iter_mut() {
                p.ctx.set_zoom_factor(self.settings.ui_scale);
            }
        }
        if let Err(e) = self.settings.save(&Settings::path()) {
            log::warn!("saving settings: {e}");
        }
        self.saved = self.settings.clone();
    }

    fn remote_events(&mut self) {
        for ev in self.services.remote_events() {
            match ev {
                fp_remote::RemoteEvent::Command(c) => {
                    use fp_core::PlayerCommand as C;
                    let a = match c {
                        C::Open { location } => Action::Open(OpenRequest {
                            location,
                            ..Default::default()
                        }),
                        C::Play => {
                            if let Some(p) = &self.playback {
                                p.player.play();
                            }
                            continue;
                        }
                        C::Pause => {
                            if let Some(p) = &self.playback {
                                p.player.pause();
                            }
                            continue;
                        }
                        C::TogglePause => Action::TogglePause,
                        C::Seek { position } => Action::Seek(position),
                        C::SeekRelative { delta } => Action::SeekRelative(delta),
                        C::SetSpeed { speed } => Action::SetSpeed(speed),
                        C::Stop => Action::ClosePlayback,
                        C::Recenter => Action::Recenter,
                    };
                    self.apply(a);
                }
                fp_remote::RemoteEvent::Text(t) => {
                    // Typed on the phone: into whichever panel has a focused field.
                    if let Some(p) = self
                        .panels
                        .iter_mut()
                        .find(|p| p.visible && p.wants_keyboard())
                    {
                        p.push(egui::Event::Text(t));
                    } else {
                        self.ui.search = t;
                        self.ui.screen = ui::Screen::Library;
                        self.ui.invalidate();
                        self.show_browser = true;
                        self.ui_visible = true;
                    }
                }
            }
        }
    }

    /// Publishes playback status to remotes and the haptics engine.
    fn publish_status(&mut self) {
        let Some(p) = &self.playback else {
            self.last_status = None;
            return;
        };
        let s = p.status();
        let changed = match &self.last_status {
            Some(prev) => {
                fp_remote::significant_change(prev, &s, s.sampled_at_ms)
                    || s.sampled_at_ms.saturating_sub(prev.sampled_at_ms) > 1000
            }
            None => true,
        };
        if changed {
            self.services.haptics.set_status(s.clone());
            if let Some(r) = &self.services.remote {
                r.publish(&s);
            }
            self.last_status = Some(s);
        }
    }

    // ---- input ---------------------------------------------------------------

    /// DeoVR-style controller bindings (see controls.rs).
    fn controller(
        &mut self,
        input: &FrameInput,
        over_ui: bool,
        click_outside: bool,
        anchor: Mat4,
        buzz: &mut Vec<(usize, f32, i64)>,
    ) {
        use crate::controls::{Cmd, Context};
        let h = input.hands;
        let playing = self.playback.is_some();
        let anchor_rot = anchor.to_scale_rotation_translation().1;
        let cmds = self.controls.update(
            &h,
            anchor_rot,
            Context {
                over_ui,
                click_outside,
                playing,
                dt: input.dt,
            },
        );
        if over_ui && (0..2).any(|i| h[i].trigger > 0.7 && self.trigger_prev[i] <= 0.7) {
            buzz.push((self.pointer.active, 0.2, 8));
        }
        self.trigger_prev = [h[0].trigger, h[1].trigger];
        if over_ui || !cmds.is_empty() {
            self.last_activity = Instant::now();
        }
        for c in cmds {
            match c {
                Cmd::TogglePause => self.apply(Action::TogglePause),
                Cmd::Back => self.back(),
                Cmd::Menu => {
                    if playing {
                        self.show_browser = !self.show_browser;
                        self.ui_visible = true;
                    } else {
                        self.ui_visible = true;
                    }
                }
                Cmd::Seek(dir) => {
                    self.apply(Action::SeekRelative(self.settings.seek_step * dir as f64));
                    buzz.push((self.pointer.active, 0.15, 10));
                }
                Cmd::Volume(d) => {
                    if let Some(p) = &self.playback {
                        let v = (p.player.volume() + d).clamp(0.0, 1.5);
                        self.apply(Action::SetVolume(v));
                    }
                }
                Cmd::Zoom(d) => {
                    if let Some(p) = &mut self.playback {
                        p.settings.zoom = (p.settings.zoom + d).clamp(0.5, 2.5);
                        p.settings_dirty = true;
                    }
                }
                Cmd::Next => self.step_queue(1),
                Cmd::Previous => self.step_queue(-1),
                Cmd::ResetImage => {
                    if let Some(p) = &mut self.playback {
                        let d = self.settings.default_view;
                        (
                            p.settings.yaw,
                            p.settings.pitch,
                            p.settings.roll,
                            p.settings.zoom,
                        ) = (d.yaw, d.pitch, d.roll, d.zoom);
                        p.settings_dirty = true;
                        self.ui.toast("View reset");
                    }
                }
                Cmd::DragBegin => {
                    self.drag_start = self
                        .playback
                        .as_ref()
                        .map(|p| (p.settings.yaw, p.settings.pitch));
                    buzz.push((self.pointer.active, 0.25, 12));
                }
                Cmd::DragTo { yaw, pitch } => {
                    if let (Some(p), Some((y0, p0))) = (&mut self.playback, self.drag_start) {
                        // The picture follows the hand.
                        p.settings.yaw = y0 + yaw;
                        p.settings.pitch = (p0 + pitch).clamp(-90.0, 90.0);
                        p.settings_dirty = true;
                    }
                }
                Cmd::DragEnd => self.drag_start = None,
                Cmd::Recenter => {
                    self.reanchor = true;
                    buzz.push((0, 0.3, 20));
                    buzz.push((1, 0.3, 20));
                }
                Cmd::ToggleUi => {
                    self.ui_visible = !self.ui_visible;
                    if !self.ui_visible {
                        self.show_browser = false;
                    }
                }
                Cmd::Page(dir) => {
                    if let Some(i) = self.pointer.hovered() {
                        let page = self.panels[i].points().y * 0.8;
                        self.panels[i].push(egui::Event::MouseWheel {
                            unit: egui::MouseWheelUnit::Point,
                            delta: egui::vec2(0.0, -page * dir as f32),
                            modifiers: egui::Modifiers::NONE,
                        });
                    }
                }
            }
        }
    }

    /// B / Y, as DeoVR's Back: close the innermost thing that is open.
    fn back(&mut self) {
        if self.panels[KEYBOARD].visible {
            let target = [MAIN, ADJUST]
                .into_iter()
                .find(|&i| self.panels[i].wants_keyboard());
            if let Some(i) = target {
                self.panels[i].push(egui::Event::Key {
                    key: egui::Key::Escape,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                });
            }
            return;
        }
        let playing = self.playback.is_some();
        if playing && self.ui.adjust_open && self.ui_visible && !self.show_browser {
            self.ui.adjust_open = false;
        } else if (!playing || self.show_browser) && self.ui.details.is_some() {
            self.ui.details = None;
        } else if (!playing || self.show_browser) && self.ui.source_form.is_some() {
            self.ui.source_form = None;
        } else if (!playing || self.show_browser) && self.ui.browse.is_some() {
            let source = self
                .ui
                .browse
                .as_ref()
                .map(|b| b.source.clone())
                .unwrap_or_default();
            match self.ui.browse.as_mut().and_then(|b| b.stack.pop()) {
                Some(parent) => self.browse(source, parent),
                None => self.ui.browse = None,
            }
        } else if playing && self.show_browser {
            // Library open over the video: back to the video.
            self.show_browser = false;
            self.ui_visible = true;
        } else if playing && self.ui_visible {
            // Player controls: back to the library, video keeps playing.
            self.show_browser = true;
            self.ui.invalidate();
        } else if playing {
            self.ui_visible = true;
        } else if self.ui.screen != ui::Screen::Home {
            self.ui.screen = ui::Screen::Home;
            self.ui.invalidate();
        }
        self.last_activity = Instant::now();
    }

    fn open_request(&mut self, req: OpenRequest) {
        self.open_seq += 1;
        let lib = self.services.library.clone();
        let seq = self.open_seq;
        let opener = self.services.opener.clone();
        let settings = self.settings.clone();
        self.ui.opening = Some(
            req.entry
                .as_ref()
                .map(|e| e.name.clone())
                .unwrap_or_else(|| req.location.rsplit('/').next().unwrap_or("").to_string()),
        );
        self.ui.open_error = None;
        self.jobs.spawn("open", move || {
            Job::Opened(seq, Box::new(playback::open(req, &opener, &lib, &settings)))
        });
    }

    /// Next/previous video in the list the current one was opened from.
    fn step_queue(&mut self, dir: i32) {
        if self.queue.is_empty() {
            self.ui.toast("No other videos in this list");
            return;
        }
        let n = self.queue.len() as i32;
        let next = self.queue_pos as i32 + dir;
        if !(0..n).contains(&next) {
            self.ui.toast(if dir > 0 {
                "Last video in this list"
            } else {
                "First video in this list"
            });
            return;
        }
        self.queue_pos = next as usize;
        let req = self.queue[self.queue_pos].clone();
        self.ui.toast(format!("{} of {}", self.queue_pos + 1, n));
        self.open_request(req);
    }

    // ---- frame -----------------------------------------------------------------

    /// Starts the embedded browser when the Web XR tab is open, and gives a
    /// page the headset when it asks for it.
    fn update_web(&mut self) {
        if let Some(rx) = &self.web_starting {
            match rx.try_recv() {
                Ok(Ok(w)) => {
                    self.web = Some(w);
                    self.web_starting = None;
                }
                Ok(Err(e)) => {
                    log::warn!("web view: {e}");
                    self.web_error = Some(e);
                    self.web_starting = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.web_starting = None,
            }
        }
        if self.ui.screen == ui::Screen::Web
            && self.web.is_none()
            && self.web_starting.is_none()
            && self.web_error.is_none()
        {
            let home = std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_default();
            let url = self.settings.web_home.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(crate::webview::WebView::start(&home, &url));
            });
            self.web_starting = Some(rx);
        }
        if self.web.is_some() && !self.yield_to_web && crate::webview::xr_requested() {
            log::info!("a page in the web view asked for the headset");
            self.yield_to_web = true;
            self.quit = true;
        }
    }

    pub fn frame(&mut self, renderer: &mut Renderer, input: FrameInput) -> FrameOutput {
        self.poll_jobs();
        self.check_mounts();
        self.remote_events();
        self.update_web();
        if let Some(p) = &mut self.playback {
            p.save_progress(&self.services.library, false);
        }
        self.publish_status();

        if let Some((pos, rot)) = input.head
            && (self.reanchor || self.anchor.is_none())
        {
            self.anchor = Some(anchor_from_head(pos, rot));
            self.reanchor = false;
        }
        let anchor = self.anchor.unwrap_or(Mat4::IDENTITY);
        let eye = input.head.map(|h| h.0).unwrap_or(anchor.w_axis.truncate());

        // Visibility.
        let playing = self.playback.is_some();
        let paused = self
            .playback
            .as_ref()
            .is_none_or(|p| p.player.is_paused() || p.player.state() != PlayerState::Playing);
        if playing
            && self.ui_visible
            && !paused
            && !self.ui.adjust_open
            && !self.show_browser
            && self.last_activity.elapsed().as_secs_f32() > self.settings.auto_hide_secs
        {
            self.ui_visible = false;
        }
        let browser = !playing || (self.ui_visible && self.show_browser);
        self.panels[MAIN].visible = browser;
        self.panels[BAR].visible = playing && self.ui_visible && !self.show_browser;
        self.panels[ADJUST].visible = self.panels[BAR].visible && self.ui.adjust_open;
        let kb_target = [MAIN, ADJUST]
            .into_iter()
            .find(|&i| self.panels[i].visible && self.panels[i].wants_keyboard());
        self.panels[KEYBOARD].visible = kb_target.is_some();

        // Placement.
        self.panels[MAIN].pose = place(anchor, 0.0, 1.5, -0.08, 0.0);
        self.panels[BAR].pose = place(anchor, 0.0, 1.05, -0.42, 28.0);
        self.panels[ADJUST].pose = place(anchor, -38.0, 1.15, -0.12, 0.0);
        self.panels[KEYBOARD].pose = place(anchor, 0.0, 0.95, -0.55, 35.0);

        // Pointer → panel events.
        let routed = {
            let [a, b, c, d] = &mut self.panels;
            self.pointer
                .route(&mut [a, b, c, d], &input.hands, input.dt)
        };
        let mut buzz = Vec::new();
        self.controller(
            &input,
            routed.over_ui,
            routed.click_outside,
            anchor,
            &mut buzz,
        );

        // Paint panels.
        let mut actions = Vec::new();
        if self.thumbs.poll(&self.panels[MAIN].ctx) {
            self.panels[MAIN].request_repaint();
        }
        // A new picture of the page in the Web XR tab.
        if self.ui.screen == ui::Screen::Web && self.web.as_ref().is_some_and(|w| w.fresh()) {
            self.panels[MAIN].request_repaint();
        }
        let devices = self.services.haptics.devices();
        let passthrough_available = input.passthrough_available;
        let web_status = if self.web_starting.is_some() {
            Some("Starting the browser...".to_string())
        } else {
            self.web_error.clone()
        };
        for idx in [MAIN, BAR, ADJUST] {
            if !self.panels[idx].needs_paint() {
                continue;
            }
            let App {
                panels,
                ui: state,
                settings,
                services,
                thumbs,
                playback,
                web,
                ..
            } = self;
            let mut view = View {
                state,
                settings,
                services,
                thumbs,
                playback: playback.as_mut(),
                actions: &mut actions,
                passthrough_available,
                devices: &devices,
                web: web.as_mut(),
                web_status: web_status.as_deref(),
            };
            let r = panels[idx].paint(renderer, input.time, |ctx| match idx {
                MAIN => ui::library::browser(ctx, &mut view),
                BAR => ui::player::control_bar(ctx, &mut view),
                _ => ui::player::adjust_panel(ctx, &mut view),
            });
            if let Err(e) = r {
                log::error!("painting panel: {e}");
            }
        }
        if let Some(target) = kb_target {
            let mut events = Vec::new();
            let mut close = false;
            let shift = &mut self.ui.keyboard_shift;
            if self.panels[KEYBOARD].needs_paint() {
                let _ = self.panels[KEYBOARD].paint(renderer, input.time, |ctx| {
                    let (e, c) = ui::keyboard::keyboard(ctx, shift);
                    events = e;
                    close = c;
                });
            }
            for e in events {
                self.panels[target].push(e);
            }
            if close {
                self.panels[target].push(egui::Event::Key {
                    key: egui::Key::Escape,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                });
            }
        }
        for a in actions {
            self.apply(a);
        }
        self.sync_settings();

        // Video.
        let mut video = VideoParams::default();
        let mut frame = None;
        let flat_or_none = self
            .playback
            .as_ref()
            .is_none_or(|p| p.format.projection == Projection::Flat);
        let passthrough = self.settings.passthrough && passthrough_available && flat_or_none;
        if passthrough {
            video.background = [0.0; 4];
        }
        let (anchor_yaw, _, _) = anchor
            .to_scale_rotation_translation()
            .1
            .to_euler(glam::EulerRot::YXZ);
        if let Some(p) = &self.playback {
            frame = p.player.current_frame();
            let mut s = p.current_settings();
            video.format = p.format;
            video.screen_pose = place(anchor, 0.0, s.screen_distance, 0.0, 0.0);
            s.yaw += anchor_yaw.to_degrees();
            video.settings = s;
            if let Some((_, rot)) = input.head {
                let rel = anchor.to_scale_rotation_translation().1.inverse() * rot;
                p.player.set_head_orientation(rel.to_array());
            }
            self.subtitles(renderer, input.time, anchor, &s);
        } else {
            self.subs.visible = false;
        }

        // Quads: panels back to front, then pointers.
        let mut quads = Vec::new();
        if self.subs.visible {
            quads.extend(self.subs.quad());
        }
        let mut order: Vec<usize> = (0..4).filter(|&i| self.panels[i].visible).collect();
        order.sort_by(|&a, &b| {
            let da = (self.panels[a].pose.w_axis.truncate() - eye).length();
            let db = (self.panels[b].pose.w_axis.truncate() - eye).length();
            db.total_cmp(&da)
        });
        for i in order {
            quads.extend(self.panels[i].quad());
        }
        let any_ui = self.panels.iter().any(|p| p.visible);
        quads.extend(self.pointer.quads(eye, any_ui));

        FrameOutput {
            frame,
            video,
            quads,
            passthrough,
            buzz,
            quit: self.quit,
        }
    }

    fn subtitles(
        &mut self,
        renderer: &mut Renderer,
        time: f64,
        anchor: Mat4,
        s: &fp_core::view::ViewSettings,
    ) {
        let Some(p) = &self.playback else { return };
        let cues = p.player.subtitles();
        let key: String = cues
            .iter()
            .map(|c| format!("{}|{}|{};", c.start, c.text, c.bitmaps.len()))
            .collect();
        self.subs.visible = !cues.is_empty();
        let d = s.subtitle_distance.clamp(0.5, 20.0);
        let w = 0.9 * d;
        self.subs.size = Vec2::new(w, w * 320.0 / 1600.0);
        self.subs.pose = place(anchor, 0.0, d, -0.2 * d, 0.0);
        if key != self.sub_key && self.subs.visible {
            self.sub_key = key;
            let textures = &mut self.sub_textures;
            if let Err(e) = self.subs.paint(renderer, time, |ctx| {
                ui::player::subtitles(ctx, &cues, textures)
            }) {
                log::error!("subtitles: {e}");
            }
        }
    }

    pub fn shutdown(mut self) {
        // The browser keeps running while a page has the headset.
        if !self.yield_to_web
            && let Some(w) = self.web.take()
        {
            w.close();
        }
        if let Some(p) = self.playback.take() {
            p.close(&self.services.library);
        }
        self.sync_settings();
        if let Some(r) = self.services.remote.take() {
            r.stop();
        }
        if let Some(w) = self.services.worker.take() {
            w.stop();
        }
    }
}
