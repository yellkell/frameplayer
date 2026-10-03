//! Glue between the player and fp-haptics.
//!
//! * Device selection from `Config.haptics` ([`choose_device`]), one of the
//!   Handy cloud API, buttplug.io / Intiface or TCode (OSR2 / SR6).
//! * A tokio task ([`spawn_haptics_task`]) that owns the [`HapticsEngine`],
//!   restarts it on config changes, and forwards the player clock from a
//!   `watch` channel the render thread writes without ever blocking.
//! * Script discovery for the open video (library script refs, local
//!   `Interactive/` conventions, feed URLs) and the timeline heat strip.

use crate::config;
use crate::runtime::services::ServiceEvent;
use fp_core::MediaTime;
use fp_haptics::buttplug::{ButtplugConfig, ButtplugDevice};
use fp_haptics::handy::{HandyCloud, HandyConfig};
use fp_haptics::tcode::{TCodeConfig, TCodeDevice, TCodeTransport};
use fp_haptics::{
    EngineConfig, HapticDevice, HapticsEngine, HeatmapScale, PlayerUpdate, ScriptSet,
};
use fp_sources::Source;
use fp_ui::screens::HapticsDevice;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};

/// Buckets in the timeline heat strip.
pub const HEAT_BUCKETS: usize = 200;

/// What the render thread publishes every frame.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct HapticsFeed {
    pub update: PlayerUpdate,
    /// Bumped on every explicit seek (forces a resync).
    pub seek_serial: u64,
}

/// Which backend `Config.haptics` asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum DeviceChoice {
    Handy {
        key: String,
    },
    Buttplug {
        url: String,
    },
    TCode {
        transport: TCodeTransport,
        sr6: bool,
    },
}

/// Parse a TCode address: `tcp://h:p`, `udp://h:p`, `h:p` (TCP) or a
/// serial device path (`/dev/ttyACM0[@baud]`).
pub fn parse_tcode_address(s: &str) -> Result<TCodeTransport, String> {
    let s = s.trim();
    let resolve = |hp: &str| -> Result<SocketAddr, String> {
        hp.to_socket_addrs()
            .map_err(|e| format!("{hp}: {e}"))?
            .next()
            .ok_or_else(|| format!("{hp}: no address"))
    };
    if let Some(hp) = s.strip_prefix("tcp://") {
        Ok(TCodeTransport::Tcp(resolve(hp)?))
    } else if let Some(hp) = s.strip_prefix("udp://") {
        Ok(TCodeTransport::Udp(resolve(hp)?))
    } else if s.starts_with('/') || s.to_ascii_uppercase().starts_with("COM") {
        let (path, baud) = match s.split_once('@') {
            Some((p, b)) => (p, b.parse().map_err(|_| format!("bad baud rate {b}"))?),
            None => (s, 115_200),
        };
        Ok(TCodeTransport::Serial {
            path: path.to_string(),
            baud,
        })
    } else if s.contains(':') {
        Ok(TCodeTransport::Tcp(resolve(s)?))
    } else {
        Err(format!("unrecognised TCode address {s:?}"))
    }
}

/// Resolve the configured backend. `Ok(None)` = haptics off.
pub fn choose_device(cfg: &config::Haptics) -> Result<Option<DeviceChoice>, String> {
    if !cfg.enabled {
        return Ok(None);
    }
    match cfg.backend.as_str() {
        "" => Ok(None),
        "handy" => match cfg.handy_connection_key.as_deref().map(str::trim) {
            Some(k) if !k.is_empty() => Ok(Some(DeviceChoice::Handy { key: k.to_string() })),
            _ => Err("The Handy needs a connection key (haptics.handy_connection_key)".into()),
        },
        "buttplug" | "intiface" => Ok(Some(DeviceChoice::Buttplug {
            url: if cfg.buttplug_url.is_empty() {
                fp_haptics::buttplug::DEFAULT_URL.to_string()
            } else {
                cfg.buttplug_url.clone()
            },
        })),
        "tcode" | "osr" => {
            let addr = cfg
                .tcode_address
                .as_deref()
                .ok_or("TCode needs an address (haptics.tcode_address)")?;
            Ok(Some(DeviceChoice::TCode {
                transport: parse_tcode_address(addr)?,
                sr6: cfg.tcode_model.eq_ignore_ascii_case("sr6"),
            }))
        }
        other => Err(format!("unknown haptics backend {other:?}")),
    }
}

pub fn engine_config(cfg: &config::Haptics) -> EngineConfig {
    EngineConfig {
        offset_ms: cfg.offset_ms as i64,
        ..Default::default()
    }
}

/// Build the device for a choice.
pub fn build_device(choice: &DeviceChoice) -> Result<Box<dyn HapticDevice>, String> {
    Ok(match choice {
        DeviceChoice::Handy { key } => {
            Box::new(HandyCloud::new(HandyConfig::new(key.clone())).map_err(|e| e.to_string())?)
        }
        DeviceChoice::Buttplug { url } => Box::new(ButtplugDevice::new(ButtplugConfig {
            url: url.clone(),
            ..Default::default()
        })),
        DeviceChoice::TCode { transport, sr6 } => Box::new(TCodeDevice::new(if *sr6 {
            TCodeConfig::sr6(transport.clone())
        } else {
            TCodeConfig::osr2(transport.clone())
        })),
    })
}

/// Devices the Settings screen can offer, from config plus a quick
/// reachability probe of Intiface.
pub async fn scan_devices(cfg: &config::Haptics) -> Vec<HapticsDevice> {
    let mut out = Vec::new();
    if cfg
        .handy_connection_key
        .as_deref()
        .is_some_and(|k| !k.trim().is_empty())
    {
        out.push(HapticsDevice {
            id: "handy".into(),
            name: "The Handy (cloud)".into(),
            backend: "handy".into(),
            connected: false,
        });
    }
    let url = if cfg.buttplug_url.is_empty() {
        fp_haptics::buttplug::DEFAULT_URL
    } else {
        cfg.buttplug_url.as_str()
    };
    if let Ok(u) = url::Url::parse(url) {
        if let (Some(host), Some(port)) = (u.host_str(), u.port_or_known_default()) {
            let reachable = tokio::time::timeout(
                Duration::from_millis(800),
                tokio::net::TcpStream::connect((host.to_string(), port)),
            )
            .await
            .is_ok_and(|r| r.is_ok());
            if reachable {
                out.push(HapticsDevice {
                    id: "buttplug".into(),
                    name: format!("Intiface Central ({host}:{port})"),
                    backend: "buttplug".into(),
                    connected: false,
                });
            }
        }
    }
    if let Some(addr) = cfg.tcode_address.as_deref().filter(|a| !a.is_empty()) {
        out.push(HapticsDevice {
            id: "tcode".into(),
            name: format!("{} ({addr})", cfg.tcode_model.to_ascii_uppercase()),
            backend: "tcode".into(),
            connected: false,
        });
    }
    out
}

/// Heat strip for the timeline.
pub fn heat_strip(set: &ScriptSet, duration: Option<MediaTime>) -> Vec<f32> {
    let Some(primary) = set.primary() else {
        return Vec::new();
    };
    let dur_ms = duration
        .map(|d| d.as_millis())
        .filter(|d| *d > 0)
        .unwrap_or_else(|| set.end_ms());
    primary.heatmap(HEAT_BUCKETS, dur_ms, HeatmapScale::default())
}

fn file_name(uri: &str) -> String {
    let u = uri.split(['?', '#']).next().unwrap_or(uri);
    let f = u.rsplit(['/', '\\']).next().unwrap_or(u);
    crate::controller::percent_decode(f)
}

/// Find and load the funscripts for a video. Returns the set and a display
/// name (the main script's file name).
pub async fn load_scripts(
    video_uri: &str,
    refs: &[fp_library::ScriptRef],
    source: Option<Arc<dyn Source>>,
) -> Option<(ScriptSet, String)> {
    let mut set = ScriptSet::new();
    let mut name = None;
    for r in refs {
        let fname = file_name(&r.uri);
        let bytes = if let Some(src) = &source {
            match src.open(&r.uri).await {
                Ok(ra) => fp_sources::read_all(ra.as_ref(), 32 << 20).await.ok(),
                Err(_) => None,
            }
        } else if r.uri.starts_with("http") {
            match reqwest::get(&r.uri).await {
                Ok(resp) => resp.bytes().await.ok().map(|b| b.to_vec()),
                Err(_) => None,
            }
        } else if crate::media_input::is_local(&r.uri) {
            let p = crate::media_input::local_path(&r.uri).ok()?;
            tokio::fs::read(p).await.ok()
        } else {
            None
        };
        let Some(bytes) = bytes else {
            tracing::warn!("could not read script {}", r.uri);
            continue;
        };
        match set.add_file(&fname, &String::from_utf8_lossy(&bytes)) {
            Ok(()) => {
                if name.is_none() || r.axis == "main" {
                    name = Some(fname);
                }
            }
            Err(e) => tracing::warn!("script {}: {e}", r.uri),
        }
    }
    if set.is_empty() && crate::media_input::is_local(video_uri) {
        if let Ok(path) = crate::media_input::local_path(video_uri) {
            let found = tokio::task::spawn_blocking(move || {
                let names = fp_haptics::funscript::discover_scripts(&path);
                ScriptSet::load_for_video(&path).ok().map(|s| (s, names))
            })
            .await
            .ok()
            .flatten();
            if let Some((s, names)) = found {
                name = names
                    .first()
                    .and_then(|(_, p)| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned());
                set = s;
            }
        }
    }
    (!set.is_empty()).then(|| (set, name.unwrap_or_else(|| "funscript".into())))
}

/// Control messages for the haptics task.
#[derive(Debug)]
pub enum HapticsCtl {
    Configure(config::Haptics),
    Load(ScriptSet),
    Clear,
    SetEnabled(bool),
}

/// Spawn the task owning the engine. Returns its control channel.
pub fn spawn_haptics_task(
    handle: &tokio::runtime::Handle,
    mut feed: watch::Receiver<HapticsFeed>,
    events: crossbeam_channel::Sender<ServiceEvent>,
) -> mpsc::UnboundedSender<HapticsCtl> {
    let (tx, mut rx) = mpsc::unbounded_channel::<HapticsCtl>();
    let h = handle.clone();
    handle.spawn(async move {
        let mut engine: Option<HapticsEngine> = None;
        let mut state_task: Option<tokio::task::JoinHandle<()>> = None;
        let mut scripts: Option<ScriptSet> = None;
        let mut enabled = true;
        let mut last_seek = 0u64;
        loop {
            tokio::select! {
                changed = feed.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let f = *feed.borrow_and_update();
                    if let Some(e) = &engine {
                        if f.seek_serial != last_seek {
                            last_seek = f.seek_serial;
                            e.seek(f.update.position);
                        }
                        e.update(PlayerUpdate {
                            playing: f.update.playing && enabled && scripts.is_some(),
                            ..f.update
                        });
                    }
                }
                ctl = rx.recv() => {
                    let Some(ctl) = ctl else { break };
                    match ctl {
                        HapticsCtl::Configure(cfg) => {
                            if let Some(t) = state_task.take() {
                                t.abort();
                            }
                            if let Some(e) = engine.take() {
                                e.shutdown().await;
                            }
                            match choose_device(&cfg) {
                                Ok(Some(choice)) => match build_device(&choice) {
                                    Ok(dev) => {
                                        let e = HapticsEngine::spawn_on(&h, dev, engine_config(&cfg));
                                        if let Some(s) = &scripts {
                                            e.load_scripts(s.clone());
                                        }
                                        let mut st = e.subscribe_state();
                                        let ev = events.clone();
                                        state_task = Some(h.spawn(async move {
                                            loop {
                                                let s = st.borrow_and_update().clone();
                                                let _ = ev.send(ServiceEvent::HapticsState {
                                                    device: s.connected.then(|| s.device.clone()),
                                                    error: s.last_error.clone(),
                                                });
                                                if st.changed().await.is_err() {
                                                    break;
                                                }
                                            }
                                        }));
                                        engine = Some(e);
                                    }
                                    Err(err) => {
                                        let _ = events.send(ServiceEvent::HapticsState { device: None, error: Some(err) });
                                    }
                                },
                                Ok(None) => {
                                    let _ = events.send(ServiceEvent::HapticsState { device: None, error: None });
                                }
                                Err(err) => {
                                    let _ = events.send(ServiceEvent::HapticsState { device: None, error: Some(err) });
                                }
                            }
                        }
                        HapticsCtl::Load(s) => {
                            if let Some(e) = &engine {
                                e.load_scripts(s.clone());
                            }
                            scripts = Some(s);
                        }
                        HapticsCtl::Clear => {
                            scripts = None;
                            if let Some(e) = &engine {
                                e.clear_scripts();
                            }
                        }
                        HapticsCtl::SetEnabled(on) => enabled = on,
                    }
                }
            }
        }
        if let Some(e) = engine.take() {
            e.shutdown().await;
        }
    });
    tx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(backend: &str) -> config::Haptics {
        config::Haptics {
            enabled: true,
            backend: backend.into(),
            ..Default::default()
        }
    }

    #[test]
    fn device_selection() {
        assert_eq!(choose_device(&config::Haptics::default()), Ok(None));
        assert_eq!(choose_device(&cfg("")), Ok(None));
        assert!(choose_device(&cfg("handy")).is_err(), "needs a key");
        let mut c = cfg("handy");
        c.handy_connection_key = Some(" abc ".into());
        assert_eq!(
            choose_device(&c),
            Ok(Some(DeviceChoice::Handy { key: "abc".into() }))
        );
        assert_eq!(
            choose_device(&cfg("buttplug")),
            Ok(Some(DeviceChoice::Buttplug {
                url: "ws://127.0.0.1:12345".into()
            }))
        );
        let mut c = cfg("tcode");
        assert!(choose_device(&c).is_err());
        c.tcode_address = Some("udp://127.0.0.1:8000".into());
        c.tcode_model = "SR6".into();
        assert_eq!(
            choose_device(&c),
            Ok(Some(DeviceChoice::TCode {
                transport: TCodeTransport::Udp("127.0.0.1:8000".parse().unwrap()),
                sr6: true
            }))
        );
        assert!(choose_device(&cfg("vibrator9000")).is_err());
        let mut off = cfg("buttplug");
        off.enabled = false;
        assert_eq!(choose_device(&off), Ok(None));
    }

    #[test]
    fn tcode_addresses() {
        assert_eq!(
            parse_tcode_address("127.0.0.1:7777").unwrap(),
            TCodeTransport::Tcp("127.0.0.1:7777".parse().unwrap())
        );
        assert_eq!(
            parse_tcode_address("/dev/ttyACM0").unwrap(),
            TCodeTransport::Serial {
                path: "/dev/ttyACM0".into(),
                baud: 115_200
            }
        );
        assert_eq!(
            parse_tcode_address("/dev/ttyUSB1@250000").unwrap(),
            TCodeTransport::Serial {
                path: "/dev/ttyUSB1".into(),
                baud: 250_000
            }
        );
        assert!(parse_tcode_address("nonsense").is_err());
    }

    #[test]
    fn devices_build() {
        assert!(build_device(&DeviceChoice::Buttplug {
            url: "ws://127.0.0.1:1".into()
        })
        .is_ok());
        assert!(build_device(&DeviceChoice::TCode {
            transport: TCodeTransport::Tcp("127.0.0.1:1".parse().unwrap()),
            sr6: false
        })
        .is_ok());
        assert_eq!(
            engine_config(&config::Haptics {
                offset_ms: -40,
                ..Default::default()
            })
            .offset_ms,
            -40
        );
    }

    #[test]
    fn scripts_next_to_local_video_and_heat() {
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("Scene One.mp4");
        std::fs::write(&video, b"x").unwrap();
        std::fs::write(
            dir.path().join("Scene One.funscript"),
            r#"{"actions":[{"at":0,"pos":0},{"at":500,"pos":100},{"at":1000,"pos":0}]}"#,
        )
        .unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let uri = fp_sources::local::path_to_uri(&video);
        let (set, name) = rt.block_on(load_scripts(&uri, &[], None)).unwrap();
        assert_eq!(name, "Scene One.funscript");
        let heat = heat_strip(&set, Some(MediaTime::from_millis(2000)));
        assert_eq!(heat.len(), HEAT_BUCKETS);
        assert!(heat[..HEAT_BUCKETS / 2].iter().any(|h| *h > 0.0));
        // Library refs (via a source) work too.
        let src: Arc<dyn Source> = Arc::new(fp_sources::local::LocalSource::new(dir.path()));
        let refs = vec![fp_library::ScriptRef {
            axis: "main".into(),
            uri: fp_sources::local::path_to_uri(&dir.path().join("Scene One.funscript")),
        }];
        let (set2, name2) = rt
            .block_on(load_scripts("smb://elsewhere/x.mp4", &refs, Some(src)))
            .unwrap();
        assert_eq!(name2, "Scene One.funscript");
        assert!(!set2.is_empty());
        assert!(rt
            .block_on(load_scripts("smb://nas/none.mp4", &[], None))
            .is_none());
    }

    #[test]
    fn haptics_task_reports_state() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (feed_tx, feed_rx) = watch::channel(HapticsFeed::default());
        let (ev_tx, ev_rx) = crossbeam_channel::unbounded();
        let ctl = spawn_haptics_task(rt.handle(), feed_rx, ev_tx);
        ctl.send(HapticsCtl::Configure(cfg("handy"))).unwrap();
        let ev = ev_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            ev,
            ServiceEvent::HapticsState {
                device: None,
                error: Some(_)
            }
        ));
        ctl.send(HapticsCtl::Configure(config::Haptics::default()))
            .unwrap();
        let ev = ev_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            ev,
            ServiceEvent::HapticsState {
                device: None,
                error: None
            }
        ));
        feed_tx.send_modify(|f| f.update.playing = true);
        drop(ctl);
    }
}
