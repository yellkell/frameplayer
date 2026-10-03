//! Long-lived subsystems: media sources, the library and its metadata
//! worker, haptic devices and the remote-control servers.

use crate::settings::{HapticDeviceConfig, Settings};
use fp_core::ByteSource;
use fp_core::source::FileSource;
use fp_haptics::HapticsEngine;
use fp_library::Library;
use fp_remote::{RemoteEvent, RemoteHub, RemoteItem, RemoteLibrary};
use fp_sources::{Source, SourceConfig};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

/// Resolves locations to readable byte sources: local paths directly,
/// everything else through the configured source that owns it.
pub struct Opener {
    sources: RwLock<HashMap<String, Arc<dyn Source>>>,
    http: fp_sources::http::HttpClient,
}

impl Opener {
    pub fn new() -> Opener {
        Opener {
            sources: RwLock::new(HashMap::new()),
            http: fp_sources::http::HttpClient::new(Default::default(), None),
        }
    }

    pub fn set_sources(&self, configs: &[SourceConfig]) -> Vec<String> {
        let mut map = HashMap::new();
        let mut errors = Vec::new();
        for c in builtin_sources().iter().chain(configs) {
            match fp_sources::build(c) {
                Ok(s) => {
                    map.insert(c.id().to_string(), Arc::<dyn Source>::from(s));
                }
                Err(e) => errors.push(format!("{}: {e}", c.name())),
            }
        }
        if let Ok(mut s) = self.sources.write() {
            *s = map;
        }
        errors
    }

    pub fn source(&self, id: &str) -> Option<Arc<dyn Source>> {
        self.sources.read().ok()?.get(id).cloned()
    }

    /// Opens `location`, preferring the source `source_id` when given.
    pub fn open_in(
        &self,
        source_id: Option<&str>,
        location: &str,
    ) -> Result<Arc<dyn ByteSource>, String> {
        if location.starts_with('/') {
            return FileSource::open(std::path::Path::new(location))
                .map(|f| Arc::new(f) as Arc<dyn ByteSource>)
                .map_err(|e| format!("{location}: {e}"));
        }
        if let Some(path) = location.strip_prefix("file://") {
            return self.open_in(None, path);
        }
        if let Some(src) = source_id.and_then(|id| self.source(id)) {
            return src.open(location).map_err(|e| e.to_string());
        }
        // Try sources whose scheme fits, then plain HTTP.
        let sources: Vec<Arc<dyn Source>> = self
            .sources
            .read()
            .map(|s| s.values().cloned().collect())
            .unwrap_or_default();
        let mut last = String::from("no source can open this location");
        for s in sources {
            match s.open(location) {
                Ok(b) => return Ok(b),
                Err(e) => last = e.to_string(),
            }
        }
        if location.starts_with("http://") || location.starts_with("https://") {
            return self
                .http
                .open(location)
                .map(|f| Arc::new(f) as Arc<dyn ByteSource>)
                .map_err(|e| e.to_string());
        }
        Err(last)
    }

    pub fn open(&self, location: &str) -> Result<Arc<dyn ByteSource>, String> {
        self.open_in(None, location)
    }

    /// Small downloads (thumbnails, scripts) from any http(s) URL.
    pub fn fetch(&self, url: &str, limit: u64) -> Result<Vec<u8>, String> {
        if url.starts_with('/') {
            return std::fs::read(url).map_err(|e| e.to_string());
        }
        self.http.get_bytes(url, limit).map_err(|e| e.to_string())
    }
}

impl Default for Opener {
    fn default() -> Self {
        Opener::new()
    }
}

/// Sources that always exist: the home folder and removable drives.
pub fn builtin_sources() -> Vec<SourceConfig> {
    let mut out = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        out.push(SourceConfig::Local(fp_sources::LocalConfig {
            id: "device-home".into(),
            name: "This device".into(),
            root: home,
        }));
    }
    let media = PathBuf::from("/run/media");
    if media.is_dir() {
        out.push(SourceConfig::Local(fp_sources::LocalConfig {
            id: "device-removable".into(),
            name: "microSD and USB drives".into(),
            root: media,
        }));
    }
    out
}

/// Video folders worth offering for the library that are not in it yet:
/// ~/Videos, ~/Downloads and the top of each mounted removable drive.
pub fn suggested_folders(existing: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for d in ["Videos", "Downloads"] {
            out.push(home.join(d));
        }
    }
    // /run/media/<user>/<label> (SteamOS) and /run/media/<label>.
    if let Ok(rd) = std::fs::read_dir("/run/media") {
        for e in rd.flatten() {
            let p = e.path();
            let children: Vec<PathBuf> = std::fs::read_dir(&p)
                .map(|r| {
                    r.flatten()
                        .map(|c| c.path())
                        .filter(|c| c.is_dir())
                        .collect()
                })
                .unwrap_or_default();
            if p.join("steamapps").exists() || children.is_empty() {
                out.push(p);
            } else {
                out.extend(children);
            }
        }
    }
    out.retain(|p| p.is_dir() && !existing.iter().any(|e| p.starts_with(e)));
    out.truncate(6);
    out
}

pub struct Services {
    pub library: Library,
    pub opener: Arc<Opener>,
    pub source_configs: Vec<SourceConfig>,
    pub worker: Option<fp_library::MetadataWorker>,
    pub worker_events: Option<std::sync::mpsc::Receiver<fp_library::WorkerEvent>>,
    pub haptics: HapticsEngine,
    pub haptic_devices: Vec<(
        HapticDeviceConfig,
        Option<fp_haptics::DeviceId>,
        Option<String>,
    )>,
    pub remote: Option<RemoteHub>,
    pub remote_error: Option<String>,
}

/// Library access for the web remote.
struct WebLibrary(Library);

impl RemoteLibrary for WebLibrary {
    fn search(&self, query: &str, limit: usize, offset: usize) -> Vec<RemoteItem> {
        let q = fp_library::Query {
            limit: Some(limit),
            offset,
            ..fp_library::Query::text(query)
        };
        self.0
            .search(&q)
            .unwrap_or_default()
            .into_iter()
            .map(|r| RemoteItem {
                id: r.id.0,
                title: r.title.clone(),
                location: r.location.clone(),
                duration: r.duration,
                format_label: r.effective_format().format.label(),
                has_thumbnail: r.thumbnail.is_some(),
            })
            .collect()
    }

    fn thumbnail_path(&self, id: i64) -> Option<PathBuf> {
        self.0
            .get(fp_library::MediaId(id))
            .ok()
            .flatten()
            .and_then(|r| r.thumbnail)
    }
}

impl Services {
    pub fn new(settings: &Settings, library: Library) -> Services {
        let opener = Arc::new(Opener::new());
        let source_configs = match fp_sources::load_configs(&Settings::sources_path()) {
            Ok(c) => c,
            Err(e) => {
                if Settings::sources_path().exists() {
                    log::warn!("sources: {e}");
                }
                Vec::new()
            }
        };
        for e in opener.set_sources(&source_configs) {
            log::warn!("source unavailable: {e}");
        }
        let haptics = HapticsEngine::new();
        haptics.set_settings(settings.haptics.clone());
        let mut s = Services {
            library,
            opener,
            source_configs,
            worker: None,
            worker_events: None,
            haptics,
            haptic_devices: Vec::new(),
            remote: None,
            remote_error: None,
        };
        s.start_worker();
        s.apply_remote(&settings.remote);
        s
    }

    pub fn start_worker(&mut self) {
        let prober = Arc::new(crate::prober::FfmpegProber {
            opener: self.opener.clone(),
        });
        match fp_library::MetadataWorker::spawn(
            self.library.clone(),
            prober,
            fp_library::WorkerOptions::default(),
        ) {
            Ok((w, rx)) => {
                self.worker = Some(w);
                self.worker_events = Some(rx);
            }
            Err(e) => log::error!("metadata worker: {e}"),
        }
    }

    pub fn wake_worker(&self) {
        if let Some(w) = &self.worker {
            w.wake();
        }
    }

    /// Persists the source list (mode 0600) and rebuilds the sources.
    pub fn set_sources(&mut self, configs: Vec<SourceConfig>) -> Vec<String> {
        if let Err(e) = fp_sources::save_configs(&Settings::sources_path(), &configs) {
            log::error!("saving sources: {e}");
        }
        let errors = self.opener.set_sources(&configs);
        self.source_configs = configs;
        errors
    }

    /// Connects one haptic device (blocking: call from a job thread).
    pub fn connect_device(cfg: &HapticDeviceConfig) -> Result<Box<dyn fp_haptics::Device>, String> {
        match cfg {
            HapticDeviceConfig::Tcode { endpoint } => {
                let endpoint =
                    fp_haptics::tcode::TcodeEndpoint::parse(endpoint).map_err(|e| e.to_string())?;
                fp_haptics::tcode::TcodeDevice::connect(fp_haptics::tcode::TcodeConfig {
                    endpoint,
                    ..Default::default()
                })
                .map(|d| Box::new(d) as Box<dyn fp_haptics::Device>)
                .map_err(|e| e.to_string())
            }
            HapticDeviceConfig::Buttplug { url } => fp_haptics::buttplug::ButtplugDevice::connect(
                fp_haptics::buttplug::ButtplugConfig {
                    url: url.clone(),
                    client_name: "FramePlayer".into(),
                    ..Default::default()
                },
            )
            .map(|d| Box::new(d) as Box<dyn fp_haptics::Device>)
            .map_err(|e| e.to_string()),
            HapticDeviceConfig::Handy { key } => {
                fp_haptics::handy::HandyDevice::connect(fp_haptics::handy::HandyConfig {
                    connection_key: key.clone(),
                    ..Default::default()
                })
                .map(|d| Box::new(d) as Box<dyn fp_haptics::Device>)
                .map_err(|e| e.to_string())
            }
        }
    }

    /// Starts or stops the remote-control servers to match `config`.
    pub fn apply_remote(&mut self, config: &fp_remote::config::RemoteConfig) {
        if let Some(r) = self.remote.take() {
            r.stop();
        }
        self.remote_error = None;
        if !config.deovr_enabled && !config.web_enabled {
            return;
        }
        let mut cfg = config.clone();
        if cfg.token_path.is_none() {
            cfg.token_path = Some(fp_remote::config::RemoteConfig::default_token_path());
        }
        match RemoteHub::new(cfg.clone()) {
            Ok(hub) => {
                if cfg.deovr_enabled
                    && let Err(e) = hub.start_deovr_server()
                {
                    self.remote_error = Some(format!("DeoVR remote API: {e}"));
                }
                if cfg.web_enabled
                    && let Err(e) = hub.start_web(Arc::new(WebLibrary(self.library.clone())))
                {
                    self.remote_error = Some(format!("web remote: {e}"));
                }
                self.remote = Some(hub);
            }
            Err(e) => self.remote_error = Some(e.to_string()),
        }
    }

    pub fn remote_events(&self) -> Vec<RemoteEvent> {
        self.remote
            .as_ref()
            .map(|r| r.events().try_iter().collect())
            .unwrap_or_default()
    }
}
