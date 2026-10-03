//! Background services on the tokio runtime, driven by [`ServiceRequest`]s
//! and reporting [`ServiceEvent`]s.
//!
//! One actor task owns the long-lived state (configured sources and their
//! connections, credentials, the remote servers, the updater) and processes
//! requests in order; anything slow (scans, opening media, thumbnails,
//! downloads) runs in its own task so the actor stays responsive.
//! Synchronous library calls go through `spawn_blocking`.
//!
//! The render thread never waits on any of this: requests go through an
//! unbounded channel, events come back through a crossbeam channel polled
//! once per frame, and the per-frame player clock is published through
//! `watch` channels (remote status, haptics feed).

use crate::config::{Config, Paths};
use crate::controller::{HapticsOp, LibraryOp, OpenTarget, OpenedMeta, ThumbSource};
use crate::haptics_bridge::{self, HapticsCtl, HapticsFeed};
use crate::media_input;
use crate::remote_bridge::{LibraryBridge, RemoteService};
use crate::thumbnailer::{self, VideoThumbnailer};
use anyhow::{anyhow, Context, Result};
use fp_core::{MediaTime, ViewSettings};
use fp_library::{Indexer, Item, Library, SpriteInfo};
use fp_remote::RemoteCommand;
use fp_sources::{CredentialStore, Source, SourceConfig};
use fp_ui::screens::HapticsDevice;
use fp_video::decode::convert::RgbaImage;
use fp_video::MediaInput;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, watch};

/// Work for the services.
pub enum ServiceRequest {
    Library(LibraryOp),
    Open {
        token: u64,
        target: OpenTarget,
        start: Option<MediaTime>,
    },
    Haptics(HapticsOp),
    ApplyConfig {
        old: Box<Config>,
        new: Box<Config>,
    },
    CheckUpdates,
    InstallUpdate,
    Shutdown(std::sync::mpsc::Sender<()>),
}

/// Update availability / progress for the Settings screen.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct UpdateEvent {
    pub available: Option<String>,
    pub progress: Option<f32>,
    pub status: Option<String>,
}

/// Results and notifications for the app.
pub enum ServiceEvent {
    Sources {
        list: Vec<SourceConfig>,
        counts: HashMap<String, usize>,
    },
    SourceOnline {
        id: String,
        online: bool,
    },
    /// Items changed (scan finished): re-run the current query.
    LibraryChanged,
    LibraryItems {
        serial: u64,
        items: Vec<Item>,
        tags: Vec<String>,
    },
    MediaOpened {
        meta: Box<OpenedMeta>,
        input: Box<dyn MediaInput>,
    },
    OpenFailed {
        token: u64,
        error: String,
    },
    ScriptsLoaded {
        token: u64,
        name: String,
        heat: Vec<f32>,
    },
    Thumbnail {
        item: i64,
        image: Option<RgbaImage>,
    },
    Sprite {
        item: i64,
        info: SpriteInfo,
        image: RgbaImage,
    },
    Remote(RemoteCommand),
    RemoteStarted {
        pairing_url: Option<String>,
        token: Option<String>,
    },
    HapticsState {
        device: Option<String>,
        error: Option<String>,
    },
    HapticsDevices(Vec<HapticsDevice>),
    Update(UpdateEvent),
    Info(String),
    Error(String),
}

impl std::fmt::Debug for ServiceEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServiceEvent::Sources { list, .. } => write!(f, "Sources({})", list.len()),
            ServiceEvent::SourceOnline { id, online } => write!(f, "SourceOnline({id}, {online})"),
            ServiceEvent::LibraryChanged => write!(f, "LibraryChanged"),
            ServiceEvent::LibraryItems { serial, items, .. } => {
                write!(f, "LibraryItems(#{serial}, {})", items.len())
            }
            ServiceEvent::MediaOpened { meta, .. } => write!(f, "MediaOpened({})", meta.uri),
            ServiceEvent::OpenFailed { token, error } => write!(f, "OpenFailed({token}, {error})"),
            ServiceEvent::ScriptsLoaded { name, .. } => write!(f, "ScriptsLoaded({name})"),
            ServiceEvent::Thumbnail { item, image } => {
                write!(f, "Thumbnail({item}, {})", image.is_some())
            }
            ServiceEvent::Sprite { item, .. } => write!(f, "Sprite({item})"),
            ServiceEvent::Remote(c) => write!(f, "Remote({c:?})"),
            ServiceEvent::RemoteStarted { pairing_url, .. } => {
                write!(f, "RemoteStarted({pairing_url:?})")
            }
            ServiceEvent::HapticsState { device, error } => {
                write!(f, "HapticsState({device:?}, {error:?})")
            }
            ServiceEvent::HapticsDevices(d) => write!(f, "HapticsDevices({})", d.len()),
            ServiceEvent::Update(u) => write!(f, "Update({u:?})"),
            ServiceEvent::Info(s) => write!(f, "Info({s})"),
            ServiceEvent::Error(s) => write!(f, "Error({s})"),
        }
    }
}

/// Startup switches (tests and headless runs turn the slow parts off).
#[derive(Debug, Clone)]
pub struct ServiceOptions {
    pub config_path: PathBuf,
    /// Scan all sources at start.
    pub scan_on_start: bool,
    /// Watch for removable media.
    pub watch_mounts: bool,
    /// Add `~/Videos` etc. when the library has no sources yet.
    pub default_sources: bool,
    /// Generate thumbnails after scans.
    pub thumbnails: bool,
}

/// The app's handle on the services.
pub struct Services {
    req: mpsc::UnboundedSender<ServiceRequest>,
    pub events: crossbeam_channel::Receiver<ServiceEvent>,
    /// Player status for the remote APIs (written by the render thread).
    pub remote_status: watch::Sender<fp_remote::PlayerStatus>,
    /// Player clock for the haptics engine (written by the render thread).
    pub haptics_feed: watch::Sender<HapticsFeed>,
}

impl Services {
    pub fn start(
        handle: &Handle,
        config: Config,
        paths: Paths,
        library: Arc<Library>,
        opts: ServiceOptions,
    ) -> Services {
        let (req_tx, req_rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = crossbeam_channel::unbounded();
        let (app_link, remote_link) = fp_remote::link(64);
        let (feed_tx, feed_rx) = watch::channel(HapticsFeed::default());

        // Remote commands → app events.
        let mut commands = app_link.commands;
        let ev = ev_tx.clone();
        handle.spawn(async move {
            while let Some(c) = commands.recv().await {
                if ev.send(ServiceEvent::Remote(c)).is_err() {
                    break;
                }
            }
        });

        let haptics = haptics_bridge::spawn_haptics_task(handle, feed_rx, ev_tx.clone());
        let creds = match CredentialStore::open(&paths.data_dir) {
            Ok(c) => Some(Arc::new(Mutex::new(c))),
            Err(e) => {
                tracing::warn!("credential store unavailable: {e}");
                None
            }
        };
        let actor = Actor {
            handle: handle.clone(),
            remote: RemoteService::new(
                remote_link,
                Arc::new(LibraryBridge::new(library.clone())),
                ev_tx.clone(),
            ),
            config,
            paths,
            lib: library.clone(),
            events: ev_tx,
            sources: Arc::new(Mutex::new(HashMap::new())),
            creds,
            haptics,
            pending_plan: Arc::new(Mutex::new(None)),
            opts,
            open_task: None,
            scan_lock: Arc::new(tokio::sync::Mutex::new(())),
        };
        handle.spawn(actor.run(req_rx));
        Services {
            req: req_tx,
            events: ev_rx,
            remote_status: app_link.status,
            haptics_feed: feed_tx,
        }
    }

    /// Queue a request (never blocks).
    pub fn send(&self, r: ServiceRequest) {
        if self.req.send(r).is_err() {
            tracing::warn!("services are gone; request dropped");
        }
    }

    /// Stop the servers / devices and wait up to `timeout`.
    pub fn shutdown(&self, timeout: Duration) {
        let (tx, rx) = std::sync::mpsc::channel();
        self.send(ServiceRequest::Shutdown(tx));
        let _ = rx.recv_timeout(timeout);
    }
}

type SourceCache = Arc<Mutex<HashMap<String, Arc<dyn Source>>>>;

struct Actor {
    handle: Handle,
    config: Config,
    paths: Paths,
    lib: Arc<Library>,
    events: crossbeam_channel::Sender<ServiceEvent>,
    sources: SourceCache,
    creds: Option<Arc<Mutex<CredentialStore>>>,
    remote: RemoteService,
    haptics: mpsc::UnboundedSender<HapticsCtl>,
    pending_plan: Arc<Mutex<Option<Box<fp_updater::UpdatePlan>>>>,
    opts: ServiceOptions,
    open_task: Option<tokio::task::JoinHandle<()>>,
    scan_lock: Arc<tokio::sync::Mutex<()>>,
}

/// Run a synchronous library call off the async workers.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> fp_library::Result<T> + Send + 'static,
) -> Result<T> {
    Ok(tokio::task::spawn_blocking(f).await??)
}

/// The source config for a configured library root (path or URI).
pub fn root_source(root: &str) -> Option<SourceConfig> {
    let root = root.trim();
    if root.is_empty() {
        return None;
    }
    let uri = media_input::normalize_uri(root);
    let kind = fp_sources::kind_for_uri(&uri)?;
    let name = uri
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(crate::controller::percent_decode)
        .unwrap_or_else(|| uri.clone());
    Some(SourceConfig {
        id: format!("root:{uri}"),
        name,
        kind,
        uri,
        pinned_host_key: None,
    })
}

/// Connect (or reuse) the source with `id`.
async fn connect_source(
    lib: &Arc<Library>,
    cache: &SourceCache,
    creds: &Option<Arc<Mutex<CredentialStore>>>,
    id: &str,
) -> Result<Arc<dyn Source>> {
    if let Some(s) = cache.lock().get(id) {
        return Ok(s.clone());
    }
    let lib2 = lib.clone();
    let id2 = id.to_string();
    let cfg = blocking(move || lib2.source(&id2))
        .await?
        .ok_or_else(|| anyhow!("unknown source {id}"))?;
    let c = creds.as_ref().and_then(|c| c.lock().get(id).ok().flatten());
    let src = fp_sources::connect(&cfg, c)
        .await
        .map_err(|e| anyhow!("{}: {e}", cfg.name))?;
    cache.lock().insert(id.to_string(), src.clone());
    Ok(src)
}

impl Actor {
    fn emit(&self, e: ServiceEvent) {
        let _ = self.events.send(e);
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<ServiceRequest>) {
        if let Err(e) = self.startup().await {
            tracing::warn!("service startup: {e:#}");
            self.emit(ServiceEvent::Error(format!("{e:#}")));
        }
        while let Some(req) = rx.recv().await {
            match req {
                ServiceRequest::Shutdown(done) => {
                    self.remote.shutdown().await;
                    let _ = self.haptics.send(HapticsCtl::Configure(Default::default()));
                    if let Some(t) = self.open_task.take() {
                        t.abort();
                    }
                    let _ = done.send(());
                    break;
                }
                other => {
                    if let Err(e) = self.handle_request(other).await {
                        tracing::warn!("service request failed: {e:#}");
                        self.emit(ServiceEvent::Error(format!("{e:#}")));
                    }
                }
            }
        }
    }

    async fn startup(&mut self) -> Result<()> {
        self.sync_roots().await?;
        if self.opts.default_sources {
            let lib = self.lib.clone();
            if blocking(move || lib.sources()).await?.is_empty() {
                self.add_default_sources().await?;
            }
        }
        self.publish_sources().await?;
        self.remote.apply(&self.config.remote_config()).await;
        let _ = self
            .haptics
            .send(HapticsCtl::Configure(self.config.haptics.clone()));
        if self.opts.scan_on_start {
            self.scan(None);
        }
        if self.opts.watch_mounts {
            self.watch_mounts();
        }
        if self.config.updates.check_on_start {
            self.spawn_update_check();
        }
        Ok(())
    }

    /// Mirror `general.library_roots` into the library's source table.
    async fn sync_roots(&self) -> Result<Vec<String>> {
        let mut added = Vec::new();
        for root in &self.config.general.library_roots {
            match root_source(root) {
                Some(cfg) => {
                    let lib = self.lib.clone();
                    let id = cfg.id.clone();
                    let existed = {
                        let lib = lib.clone();
                        let id = id.clone();
                        blocking(move || lib.source(&id)).await?.is_some()
                    };
                    if !existed {
                        blocking(move || lib.upsert_source(&cfg)).await?;
                        added.push(id);
                    }
                }
                None => tracing::warn!("ignoring library root {root:?} (unsupported URI)"),
            }
        }
        Ok(added)
    }

    async fn add_default_sources(&self) -> Result<Vec<String>> {
        let mut added = Vec::new();
        for p in fp_sources::local::LocalSource::default_roots() {
            let uri = fp_sources::local::path_to_uri(&p);
            let Some(cfg) = root_source(&uri) else {
                continue;
            };
            let lib = self.lib.clone();
            added.push(cfg.id.clone());
            blocking(move || lib.upsert_source(&cfg)).await?;
        }
        Ok(added)
    }

    async fn publish_sources(&self) -> Result<()> {
        let lib = self.lib.clone();
        let (list, counts) = blocking(move || source_counts(&lib)).await?;
        self.emit(ServiceEvent::Sources { list, counts });
        Ok(())
    }

    fn scan_ctx(&self) -> ScanCtx {
        ScanCtx {
            lib: self.lib.clone(),
            cache: self.sources.clone(),
            creds: self.creds.clone(),
            events: self.events.clone(),
            thumbs_dir: self.paths.thumbs_dir(),
            lock: self.scan_lock.clone(),
            handle: self.handle.clone(),
            thumbnails: self.opts.thumbnails,
        }
    }

    /// Scan sources in the background (one scan at a time).
    fn scan(&self, only: Option<String>) {
        spawn_scan(self.scan_ctx(), only);
    }

    fn watch_mounts(&self) {
        let ctx = self.scan_ctx();
        self.handle.spawn(async move {
            let (_watcher, mut rx) = fp_sources::local::MountWatcher::spawn(Duration::from_secs(3));
            while let Some(ev) = rx.recv().await {
                match ev {
                    fp_sources::local::MountEvent::Added(m) => {
                        let uri = fp_sources::local::path_to_uri(&m.mount_point);
                        let cfg = SourceConfig {
                            id: format!("mount:{}", m.mount_point.display()),
                            name: format!("{} ({})", m.label(), m.kind_label()),
                            kind: fp_sources::SourceKind::Local,
                            uri,
                            pinned_host_key: None,
                        };
                        let id = cfg.id.clone();
                        let lib = ctx.lib.clone();
                        if blocking(move || lib.upsert_source(&cfg)).await.is_ok() {
                            let _ = ctx
                                .events
                                .send(ServiceEvent::Info(format!("{} connected", m.kind_label())));
                            spawn_scan(ctx.clone(), Some(id));
                        }
                    }
                    fp_sources::local::MountEvent::Removed(m) => {
                        let id = format!("mount:{}", m.mount_point.display());
                        ctx.cache.lock().remove(&id);
                        let _ = ctx
                            .events
                            .send(ServiceEvent::SourceOnline { id, online: false });
                    }
                }
            }
        });
    }

    async fn handle_request(&mut self, req: ServiceRequest) -> Result<()> {
        match req {
            ServiceRequest::Library(op) => self.library_op(op).await?,
            ServiceRequest::Open {
                token,
                target,
                start,
            } => self.open(token, target, start),
            ServiceRequest::Haptics(op) => match op {
                HapticsOp::ClearScripts => {
                    let _ = self.haptics.send(HapticsCtl::Clear);
                }
                HapticsOp::SetScriptEnabled(on) => {
                    let _ = self.haptics.send(HapticsCtl::SetEnabled(on));
                }
                HapticsOp::Scan => {
                    let cfg = self.config.haptics.clone();
                    let ev = self.events.clone();
                    self.handle.spawn(async move {
                        let found = haptics_bridge::scan_devices(&cfg).await;
                        let _ = ev.send(ServiceEvent::HapticsDevices(found));
                    });
                }
            },
            ServiceRequest::ApplyConfig { old, new } => self.apply_config(*old, *new).await?,
            ServiceRequest::CheckUpdates => self.spawn_update_check(),
            ServiceRequest::InstallUpdate => self.spawn_install(),
            ServiceRequest::Shutdown(_) => unreachable!("handled in run"),
        }
        Ok(())
    }

    async fn apply_config(&mut self, old: Config, new: Config) -> Result<()> {
        self.config = new.clone();
        let path = self.opts.config_path.clone();
        let to_save = new.clone();
        tokio::task::spawn_blocking(move || to_save.save(&path))
            .await?
            .context("saving config")?;
        if remote_needs_restart(&old, &new) {
            self.remote.apply(&new.remote_config()).await;
        }
        if old.haptics != new.haptics {
            let _ = self
                .haptics
                .send(HapticsCtl::Configure(new.haptics.clone()));
        }
        if old.general.library_roots != new.general.library_roots {
            for id in self.sync_roots().await? {
                self.scan(Some(id));
            }
            self.publish_sources().await?;
        }
        Ok(())
    }

    async fn library_op(&mut self, op: LibraryOp) -> Result<()> {
        let lib = self.lib.clone();
        match op {
            LibraryOp::Query { serial, query } => {
                let ev = self.events.clone();
                self.handle.spawn(async move {
                    let r = blocking(move || {
                        let items = lib.query_items(&query)?;
                        let tags = lib.all_tags()?.into_iter().map(|(t, _)| t).collect();
                        Ok((items, tags))
                    })
                    .await;
                    match r {
                        Ok((items, tags)) => {
                            let _ = ev.send(ServiceEvent::LibraryItems {
                                serial,
                                items,
                                tags,
                            });
                        }
                        Err(e) => {
                            let _ = ev.send(ServiceEvent::Error(format!("Library query: {e:#}")));
                        }
                    }
                });
            }
            LibraryOp::ContinueWatching { serial } => {
                let ev = self.events.clone();
                self.handle.spawn(async move {
                    let r = blocking(move || {
                        let items = lib.continue_watching(500)?;
                        let tags = lib.all_tags()?.into_iter().map(|(t, _)| t).collect();
                        Ok((items, tags))
                    })
                    .await;
                    if let Ok((items, tags)) = r {
                        let _ = ev.send(ServiceEvent::LibraryItems {
                            serial,
                            items,
                            tags,
                        });
                    }
                });
            }
            LibraryOp::Refresh(id) => self.scan(id),
            LibraryOp::AddDefaultSources => {
                for id in self.add_default_sources().await? {
                    self.scan(Some(id));
                }
                self.publish_sources().await?;
            }
            LibraryOp::SetFavourite(id, on) => blocking(move || lib.set_favourite(id, on)).await?,
            LibraryOp::SaveResume(id, pos) => blocking(move || lib.set_resume(id, pos)).await?,
            LibraryOp::ClearResume(id) => blocking(move || lib.clear_resume(id)).await?,
            LibraryOp::RecordWatch {
                item,
                position,
                completed,
            } => blocking(move || lib.record_watch(item, position, completed)).await?,
            LibraryOp::SaveView { target, settings } => {
                blocking(move || save_view(&lib, target.item_id, &target.uri, &settings)).await?
            }
            LibraryOp::LoadThumbnails(jobs) => {
                let ev = self.events.clone();
                self.handle.spawn(async move {
                    for (item, src) in jobs {
                        let bytes = match &src {
                            ThumbSource::File(p) => tokio::fs::read(p).await.ok(),
                            ThumbSource::Url(u) => fetch(u).await,
                        };
                        let image = match bytes {
                            Some(b) => tokio::task::spawn_blocking(move || {
                                thumbnailer::display_thumbnail(&b).ok()
                            })
                            .await
                            .ok()
                            .flatten(),
                            None => None,
                        };
                        if ev.send(ServiceEvent::Thumbnail { item, image }).is_err() {
                            break;
                        }
                    }
                });
            }
            LibraryOp::LoadSprite { item, path } => {
                let ev = self.events.clone();
                self.handle.spawn(async move {
                    let Ok(Some(info)) = blocking(move || lib.sprite_info(item)).await else {
                        return;
                    };
                    let Ok(bytes) = tokio::fs::read(&path).await else {
                        return;
                    };
                    let img =
                        tokio::task::spawn_blocking(move || thumbnailer::decode_image(&bytes))
                            .await;
                    if let Ok(Ok(image)) = img {
                        let _ = ev.send(ServiceEvent::Sprite { item, info, image });
                    }
                });
            }
        }
        Ok(())
    }

    /// Resolve media and its metadata, then hand it to the app.
    fn open(&mut self, token: u64, target: OpenTarget, start: Option<MediaTime>) {
        if let Some(t) = self.open_task.take() {
            t.abort();
        }
        let lib = self.lib.clone();
        let cache = self.sources.clone();
        let creds = self.creds.clone();
        let ev = self.events.clone();
        let haptics = self.haptics.clone();
        let handle = self.handle.clone();
        self.open_task = Some(self.handle.spawn(async move {
            match resolve_open(&lib, &cache, &creds, &handle, token, target, start).await {
                Ok((meta, input, source, refs)) => {
                    let uri = meta.uri.clone();
                    let duration = meta.item.as_ref().and_then(|i| i.duration);
                    if ev
                        .send(ServiceEvent::MediaOpened {
                            meta: Box::new(meta),
                            input,
                        })
                        .is_err()
                    {
                        return;
                    }
                    if let Some((set, name)) =
                        haptics_bridge::load_scripts(&uri, &refs, source).await
                    {
                        let heat = haptics_bridge::heat_strip(&set, duration);
                        let _ = haptics.send(HapticsCtl::Load(set));
                        let _ = ev.send(ServiceEvent::ScriptsLoaded { token, name, heat });
                    }
                }
                Err(e) => {
                    let _ = ev.send(ServiceEvent::OpenFailed {
                        token,
                        error: format!("{e:#}"),
                    });
                }
            }
        }));
    }

    fn spawn_update_check(&mut self) {
        let Some(updater) = self.updater() else {
            self.emit(ServiceEvent::Update(UpdateEvent {
                status: Some(
                    "Updates are managed by the installer (not a launcher install).".into(),
                ),
                ..Default::default()
            }));
            return;
        };
        let ev = self.events.clone();
        let slot = self.pending_plan.clone();
        self.handle.spawn(async move {
            let e = match updater.check().await {
                Ok(fp_updater::UpdateCheck::Available(plan)) => {
                    let v = plan.version().to_string();
                    let mb = plan.download_size() as f64 / 1e6;
                    *slot.lock() = Some(plan);
                    UpdateEvent {
                        available: Some(v),
                        status: Some(format!("{mb:.1} MB download")),
                        progress: None,
                    }
                }
                Ok(fp_updater::UpdateCheck::UpToDate { latest }) => UpdateEvent {
                    status: Some(format!("Up to date (latest {latest})")),
                    ..Default::default()
                },
                Ok(fp_updater::UpdateCheck::Unavailable { version, reason }) => UpdateEvent {
                    status: Some(format!("{version} can't be installed: {reason}")),
                    ..Default::default()
                },
                Err(e) => UpdateEvent {
                    status: Some(format!("Update check failed: {e}")),
                    ..Default::default()
                },
            };
            let _ = ev.send(ServiceEvent::Update(e));
        });
    }

    fn spawn_install(&mut self) {
        let plan = self.pending_plan.lock().take();
        let (Some(updater), Some(plan)) = (self.updater(), plan) else {
            self.emit(ServiceEvent::Update(UpdateEvent {
                status: Some("No update to install; check first.".into()),
                ..Default::default()
            }));
            return;
        };
        let ev = self.events.clone();
        self.handle.spawn(async move {
            let ev2 = ev.clone();
            let mut last = std::time::Instant::now() - Duration::from_secs(1);
            let mut progress = move |p: fp_updater::DownloadProgress| {
                if last.elapsed() < Duration::from_millis(200) {
                    return;
                }
                last = std::time::Instant::now();
                let frac = p.total.map(|t| p.done as f32 / t.max(1) as f32);
                let _ = ev2.send(ServiceEvent::Update(UpdateEvent {
                    available: Some(String::new()),
                    progress: Some(frac.unwrap_or(0.0)),
                    status: None,
                }));
            };
            let result = async {
                let staged = updater.download(&plan, &mut progress).await?;
                let u = updater.clone();
                let s = staged.clone();
                tokio::task::spawn_blocking(move || u.install(&s))
                    .await
                    .map_err(|e| anyhow!("install task: {e}"))??;
                Ok::<_, anyhow::Error>(staged.version.to_string())
            }
            .await;
            let e = match result {
                Ok(v) => UpdateEvent {
                    status: Some(format!("Installed {v}. Restart FramePlayer to use it.")),
                    ..Default::default()
                },
                Err(e) => UpdateEvent {
                    status: Some(format!("Update failed: {e:#}")),
                    ..Default::default()
                },
            };
            let _ = ev.send(ServiceEvent::Update(e));
        });
    }

    fn updater(&self) -> Option<fp_updater::Updater> {
        let layout = fp_updater::InstallLayout::from_env()?;
        let version = semver::Version::parse(env!("CARGO_PKG_VERSION")).ok()?;
        let mut cfg = fp_updater::UpdaterConfig::new(layout, version);
        cfg.channel = self.config.update_channel();
        if let Ok(u) = url::Url::parse(&self.config.updates.base_url) {
            cfg.base_url = u;
        }
        fp_updater::Updater::new(cfg)
            .map_err(|e| tracing::warn!("updater: {e}"))
            .ok()
    }
}

/// Everything a background scan needs.
#[derive(Clone)]
struct ScanCtx {
    lib: Arc<Library>,
    cache: SourceCache,
    creds: Option<Arc<Mutex<CredentialStore>>>,
    events: crossbeam_channel::Sender<ServiceEvent>,
    thumbs_dir: PathBuf,
    lock: Arc<tokio::sync::Mutex<()>>,
    handle: Handle,
    thumbnails: bool,
}

fn source_counts(lib: &Library) -> fp_library::Result<(Vec<SourceConfig>, HashMap<String, usize>)> {
    let list = lib.sources()?;
    let mut counts = HashMap::new();
    for s in &list {
        counts.insert(s.id.clone(), lib.items(Some(&s.id))?.len());
    }
    Ok((list, counts))
}

/// Index `only` (or every source), then generate thumbnails; one scan at a time.
fn spawn_scan(ctx: ScanCtx, only: Option<String>) {
    let h = ctx.handle.clone();
    h.spawn(async move {
        let ScanCtx {
            lib,
            cache,
            creds,
            events,
            thumbs_dir,
            lock,
            handle,
            thumbnails,
        } = ctx;
        let _guard = lock.lock().await;
        let ids: Vec<String> = match only {
            Some(id) => vec![id],
            None => {
                let lib = lib.clone();
                match blocking(move || lib.sources()).await {
                    Ok(s) => s.into_iter().map(|s| s.id).collect(),
                    Err(e) => {
                        tracing::warn!("listing sources: {e:#}");
                        return;
                    }
                }
            }
        };
        for id in ids {
            let src = match connect_source(&lib, &cache, &creds, &id).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("source {id} offline: {e:#}");
                    let _ = events.send(ServiceEvent::SourceOnline { id, online: false });
                    continue;
                }
            };
            let _ = events.send(ServiceEvent::SourceOnline {
                id: id.clone(),
                online: true,
            });
            let mut indexer = Indexer::new(lib.clone(), thumbs_dir.clone());
            if thumbnails {
                indexer = indexer.with_thumbnailer(Arc::new(VideoThumbnailer::new(handle.clone())));
            }
            if src.kind() == fp_sources::SourceKind::DeoVr {
                let lib2 = lib.clone();
                let id2 = id.clone();
                if let Ok(Some(cfg)) = blocking(move || lib2.source(&id2)).await {
                    let auth = creds
                        .as_ref()
                        .and_then(|c| c.lock().get(&id).ok().flatten())
                        .map(|c| fp_sources::http::HttpAuth::new(&c.username, &c.password));
                    if let Ok(http) = fp_sources::http::HttpClient::new(auth) {
                        if let Ok(client) = fp_sources::deovr::DeoVrClient::new(http, &cfg.uri) {
                            indexer = indexer.with_deovr_client(client);
                        }
                    }
                }
            }
            match indexer.index_source(&id, src.clone()).await {
                Ok(r) => {
                    tracing::info!(
                        "indexed {id}: +{} ~{} moved {} -{} ={} ({} errors)",
                        r.added,
                        r.updated,
                        r.moved,
                        r.removed,
                        r.unchanged,
                        r.errors.len()
                    );
                    let lib2 = lib.clone();
                    let id2 = id.clone();
                    let _ = blocking(move || lib2.set_source_scanned(&id2)).await;
                    let _ = events.send(ServiceEvent::LibraryChanged);
                }
                Err(e) => {
                    tracing::warn!("indexing {id}: {e}");
                    let _ = events.send(ServiceEvent::Error(format!("Scanning failed: {e}")));
                    continue;
                }
            }
            if thumbnails {
                match indexer.generate_thumbnails(&id, src, false).await {
                    Ok(n) if n > 0 => {
                        let _ = events.send(ServiceEvent::LibraryChanged);
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!("thumbnails for {id}: {e}"),
                }
            }
        }
        let lib2 = lib.clone();
        if let Ok((list, counts)) = blocking(move || source_counts(&lib2)).await {
            let _ = events.send(ServiceEvent::Sources { list, counts });
        }
    });
}

/// Whether a config change requires restarting the remote servers. Saving
/// the token the running server generated is not a change.
pub fn remote_needs_restart(old: &Config, new: &Config) -> bool {
    let mut o = old.remote_config();
    if old.remote.api_token.is_none() {
        o.http.token = new.remote_config().http.token;
    }
    o != new.remote_config()
}

/// Persist a view override for an item or a bare URI.
pub fn save_view(
    lib: &Library,
    item_id: Option<i64>,
    uri: &str,
    vs: &ViewSettings,
) -> fp_library::Result<()> {
    match item_id {
        Some(id) => lib.set_item_view_settings(id, vs),
        None => lib.set_override(None, Some(uri), vs),
    }
}

async fn fetch(url: &str) -> Option<Vec<u8>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .ok()?;
    let resp = client.get(url).send().await.ok()?.error_for_status().ok()?;
    resp.bytes().await.ok().map(|b| b.to_vec())
}

type Resolved = (
    OpenedMeta,
    Box<dyn MediaInput>,
    Option<Arc<dyn Source>>,
    Vec<fp_library::ScriptRef>,
);

/// Look the media up in the library, connect its source and open it.
async fn resolve_open(
    lib: &Arc<Library>,
    cache: &SourceCache,
    creds: &Option<Arc<Mutex<CredentialStore>>>,
    handle: &Handle,
    token: u64,
    target: OpenTarget,
    start: Option<MediaTime>,
) -> Result<Resolved> {
    let (uri, item) = match target {
        OpenTarget::Item(id) => {
            let l = lib.clone();
            let item = blocking(move || l.item(id))
                .await?
                .ok_or_else(|| anyhow!("library item {id} not found"))?;
            (item.uri.clone(), Some(item))
        }
        OpenTarget::Uri(u) => {
            let uri = media_input::normalize_uri(&u);
            let l = lib.clone();
            let u2 = uri.clone();
            let item = blocking(move || l.item_by_uri(&u2)).await?;
            (uri, item)
        }
    };
    let (user_override, refs) = {
        let l = lib.clone();
        let item2 = item.clone();
        let uri2 = uri.clone();
        blocking(move || {
            let hash = item2.as_ref().and_then(|i| i.content_hash.clone());
            let over = l.get_override(hash.as_deref(), Some(&uri2))?;
            let refs = match &item2 {
                Some(i) => l.scripts(i.id)?,
                None => Vec::new(),
            };
            Ok((over, refs))
        })
        .await?
    };
    // The configured source this URI lives in (for credentials).
    let source_id = match &item {
        Some(i) => i.source_id.clone(),
        None => {
            let l = lib.clone();
            let u = uri.clone();
            blocking(move || {
                Ok(l.sources()?
                    .into_iter()
                    .filter(|s| u.starts_with(s.uri.trim_end_matches('/')))
                    .max_by_key(|s| s.uri.len())
                    .map(|s| s.id))
            })
            .await?
        }
    };
    let source = match (&source_id, media_input::is_local(&uri)) {
        (Some(id), false) => Some(connect_source(lib, cache, creds, id).await?),
        _ => None,
    };
    let input = media_input::open_input(&uri, source.clone(), handle.clone())
        .await
        .with_context(|| format!("opening {uri}"))?;
    Ok((
        OpenedMeta {
            token,
            uri,
            item,
            user_override,
            explicit_start: start,
        },
        input,
        source,
        refs,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_sources() {
        let s = root_source("/home/deck/My Videos").unwrap();
        assert_eq!(s.kind, fp_sources::SourceKind::Local);
        assert!(s.uri.starts_with("file:///"));
        assert_eq!(s.name, "My Videos");
        assert!(s.id.starts_with("root:file:///"));
        let s = root_source("smb://nas/share/vr").unwrap();
        assert_eq!(s.kind, fp_sources::SourceKind::Smb);
        assert_eq!(s.name, "vr");
        assert!(root_source("").is_none());
        assert!(root_source("gopher://x").is_none());
    }

    #[test]
    fn remote_restart_rules() {
        let old = Config::default();
        let mut new = old.clone();
        assert!(!remote_needs_restart(&old, &new));
        new.remote.api_enabled = true;
        assert!(remote_needs_restart(&old, &new));
        // Persisting the generated token does not restart.
        let mut running = new.clone();
        running.remote.api_token = Some("generated".into());
        assert!(!remote_needs_restart(&new, &running));
        // Regenerating (token cleared) does.
        let mut regen = running.clone();
        regen.remote.api_token = None;
        assert!(remote_needs_restart(&running, &regen));
    }

    #[test]
    fn save_view_for_item_and_uri() {
        let lib = Library::open_in_memory().unwrap();
        let id = lib
            .upsert_item(&fp_library::NewItem {
                uri: "file:///v/a.mp4".into(),
                path: "a.mp4".into(),
                title: "a".into(),
                ..Default::default()
            })
            .unwrap();
        let vs = ViewSettings {
            stereo: fp_core::StereoMode::Ou,
            ..Default::default()
        };
        save_view(&lib, Some(id), "file:///v/a.mp4", &vs).unwrap();
        assert_eq!(
            lib.view_settings(id).unwrap().stereo,
            fp_core::StereoMode::Ou
        );
        save_view(&lib, None, "https://h/x.mp4", &vs).unwrap();
        assert_eq!(
            lib.get_override(None, Some("https://h/x.mp4")).unwrap(),
            Some(vs)
        );
    }
}
