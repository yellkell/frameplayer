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
            let path = std::path::Path::new(location);
            if !path.exists() && location.starts_with(&format!("{REMOVABLE_ROOT}/")) {
                return Err(
                    "This video is on a microSD card or USB drive that is not inserted.".into(),
                );
            }
            return FileSource::open(path)
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
    // Always offered, so a card inserted later shows up without a restart.
    out.push(SourceConfig::Local(fp_sources::LocalConfig {
        id: REMOVABLE_SOURCE.into(),
        name: "microSD and USB drives".into(),
        root: PathBuf::from(REMOVABLE_ROOT),
    }));
    out
}

pub const REMOVABLE_SOURCE: &str = "device-removable";
/// Where SteamOS (udisks) mounts microSD cards and USB drives:
/// `/run/media/deck/<label>` (older images: `/run/media/<device>`).
pub const REMOVABLE_ROOT: &str = "/run/media";

/// Mount points of removable drives, from a `/proc/mounts` listing.
pub fn parse_removable_mounts(mounts: &str) -> Vec<PathBuf> {
    let prefix = format!("{REMOVABLE_ROOT}/");
    let mut out: Vec<PathBuf> = mounts
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1))
        .map(unescape_mount)
        .filter(|p| p.starts_with(&prefix))
        .map(PathBuf::from)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// `/proc/mounts` writes spaces and a few other bytes as `\ooo` octal.
fn unescape_mount(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let octal = b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c));
        if octal {
            out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Currently mounted microSD cards and USB drives.
pub fn removable_mounts() -> Vec<PathBuf> {
    std::fs::read_to_string("/proc/mounts")
        .map(|m| parse_removable_mounts(&m))
        .unwrap_or_default()
}

/// A drive partition SteamOS left unmounted that FramePlayer mounts itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnmountedDrive {
    /// Kernel name, e.g. `mmcblk0p1`.
    pub device: String,
    pub fs_type: String,
}

/// Whether FramePlayer should mount this partition. The Frame's SteamOS
/// automounts only ext4 microSD cards (Steam libraries need symlinks) and
/// no USB drives at all (`sd*` also names its internal OS slots), so a card
/// formatted on Windows is never mounted. `sys_path` is the resolved
/// `/sys/class/block/<device>` link; USB drives are told apart from the
/// internal UFS disks by it.
pub fn should_mount(device: &str, sys_path: &str, fs_type: &str) -> bool {
    const FOREIGN: [&str; 4] = ["exfat", "vfat", "ntfs", "ntfs3"];
    if device.starts_with("mmcblk") {
        // ext4 cards are SteamOS's: it checks them before mounting.
        FOREIGN.contains(&fs_type)
    } else {
        sys_path.contains("/usb") && (FOREIGN.contains(&fs_type) || fs_type == "ext4")
    }
}

/// `ID_FS_TYPE` from a udev database record (`/run/udev/data/b<maj>:<min>`).
pub fn udev_fs_type(record: &str) -> Option<&str> {
    record
        .lines()
        .find_map(|l| l.strip_prefix("E:ID_FS_TYPE="))
        .filter(|t| !t.is_empty())
}

/// microSD cards and USB drives with a readable filesystem that nothing
/// has mounted.
pub fn unmounted_drives() -> Vec<UnmountedDrive> {
    let mounted = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    let mounted: Vec<&str> = mounted
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .collect();
    let Ok(dir) = std::fs::read_dir("/sys/class/block") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in dir.flatten() {
        let device = e.file_name().to_string_lossy().into_owned();
        if !device.starts_with("mmcblk") && !device.starts_with("sd") {
            continue;
        }
        let sys_path = std::fs::canonicalize(e.path())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !device.starts_with("mmcblk") && !sys_path.contains("/usb") {
            continue;
        }
        let Ok(devnum) = std::fs::read_to_string(e.path().join("dev")) else {
            continue;
        };
        let Ok(record) = std::fs::read_to_string(format!("/run/udev/data/b{}", devnum.trim()))
        else {
            continue;
        };
        let Some(fs_type) = udev_fs_type(&record) else {
            continue;
        };
        if should_mount(&device, &sys_path, fs_type)
            && !mounted.contains(&format!("/dev/{device}").as_str())
        {
            out.push(UnmountedDrive {
                fs_type: fs_type.to_string(),
                device,
            });
        }
    }
    out.sort_by(|a, b| a.device.cmp(&b.device));
    out
}

/// Mounts a drive through udisks, as SteamOS does, under
/// `/run/media/<user>/<label>`. The Frame's polkit rules let the logged-in
/// user do this without a password. Blocking: call from a job thread.
pub fn mount_drive(drive: &UnmountedDrive) -> Result<PathBuf, String> {
    let out = std::process::Command::new("udisksctl")
        .args(["mount", "--no-user-interaction", "-o", "noatime", "-b"])
        .arg(format!("/dev/{}", drive.device))
        .output()
        .map_err(|e| format!("udisksctl: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(err.trim().to_string());
    }
    // "Mounted /dev/mmcblk0p1 at /run/media/steamos/SDCARD"
    text.trim()
        .split_once(" at ")
        .map(|(_, p)| PathBuf::from(p.trim_end_matches('.')))
        .ok_or_else(|| format!("unexpected udisksctl output: {}", text.trim()))
}

/// Video folders worth offering for the library that are not in it yet
/// (removable drives are indexed automatically).
pub fn suggested_folders(existing: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for d in ["Videos", "Downloads", "Movies"] {
            out.push(home.join(d));
        }
    }
    out.retain(|p| p.is_dir() && !existing.iter().any(|e| p.starts_with(e)));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removable_mounts_from_proc() {
        let m = "/dev/nvme0n1p8 /home ext4 rw 0 0\n\
                 /dev/mmcblk0p1 /run/media/deck/SD\\040Card ext4 rw,nosuid 0 0\n\
                 /dev/sda1 /run/media/deck/USB exfat rw 0 0\n\
                 /dev/mmcblk0p1 /run/media/mmcblk0p1 ext4 rw 0 0\n\
                 tmpfs /run/user/1000 tmpfs rw 0 0\n";
        assert_eq!(
            parse_removable_mounts(m),
            vec![
                PathBuf::from("/run/media/deck/SD Card"),
                PathBuf::from("/run/media/deck/USB"),
                PathBuf::from("/run/media/mmcblk0p1")
            ]
        );
    }

    #[test]
    fn mounts_what_steamos_leaves_alone() {
        let card = "/sys/devices/platform/soc@0/8804000.mmc/mmc_host/mmc0/mmc0:d555/block/mmcblk0/mmcblk0p1";
        let usb = "/sys/devices/platform/soc@0/a600000.usb/xhci-hcd.1.auto/usb1/1-1/1-1:1.0/host0/target0:0:0/0:0:0:0/block/sde/sde1";
        let ufs =
            "/sys/devices/platform/soc@0/1d84000.ufshc/host0/target0:0:0/0:0:0:0/block/sda/sda8";
        assert!(should_mount("mmcblk0p1", card, "exfat"));
        assert!(should_mount("mmcblk0p1", card, "vfat"));
        assert!(!should_mount("mmcblk0p1", card, "ext4"));
        assert!(should_mount("sde1", usb, "exfat"));
        assert!(should_mount("sde1", usb, "ext4"));
        assert!(!should_mount("sda8", ufs, "ext4"));
        assert!(!should_mount("sda1", ufs, "vfat"));
        assert!(!should_mount("sde1", usb, "swap"));
    }

    #[test]
    fn fs_type_from_udev_record() {
        let r = "S:disk/by-label/SDCARD\nE:ID_FS_LABEL=SDCARD\nE:ID_FS_TYPE=exfat\nE:ID_FS_USAGE=filesystem\n";
        assert_eq!(udev_fs_type(r), Some("exfat"));
        assert_eq!(udev_fs_type("E:ID_FS_TYPE=\n"), None);
        assert_eq!(udev_fs_type("E:ID_PART_TABLE_TYPE=dos\n"), None);
    }
}
