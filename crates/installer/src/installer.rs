//! High-level install flows shared by the CLI and a future GUI.
//!
//! Every user-visible message goes through a [`Reporter`], so a GUI can show
//! steps and progress without parsing stdout.

use crate::config::{Device, Paths, DEVKIT_SERVICE_PORT};
use crate::devkit::{DevkitClient, RegisterOutcome};
use crate::discovery::{self, Discovered};
use crate::remote::{self, ProbeState, RemoteStatus};
use crate::ssh::SshTarget;
use crate::steam::{ArtKind, SteamCdp, CEF_DEBUG_PORT};
use crate::transport::{LineSink, Remote, TransportKind};
use crate::{sshkey, DISPLAY_NAME, GAME_DIR, PROBE_DISPLAY_NAME};
use anyhow::{anyhow, bail, Context, Result};
use fp_updater::download::{self, Expect};
use fp_updater::manifest::Channel;
use fp_updater::signing::ManifestVerifier;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use url::Url;

/// Something the user should see.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A new top-level step ("Uploading…").
    Step(String),
    Info(String),
    Warn(String),
    /// Byte progress of the current step.
    Progress {
        done: u64,
        total: Option<u64>,
    },
    /// The user must approve the pairing on the headset.
    ApproveOnHeadset {
        fingerprint: String,
    },
    Done(String),
}

pub trait Reporter: Send + Sync {
    fn event(&self, e: Event);
}

/// Reporter that discards everything (tests, scripting).
pub struct Silent;
impl Reporter for Silent {
    fn event(&self, _: Event) {}
}

/// Options for [`Installer::pair`].
#[derive(Debug, Clone)]
pub struct PairOptions {
    pub host: Option<String>,
    pub name: Option<String>,
    pub user: Option<String>,
    pub service_port: u16,
    pub ssh_port: u16,
    pub approve_timeout: Duration,
    pub discover_timeout: Duration,
}

impl Default for PairOptions {
    fn default() -> Self {
        Self {
            host: None,
            name: None,
            user: None,
            service_port: DEVKIT_SERVICE_PORT,
            ssh_port: 22,
            approve_timeout: Duration::from_secs(180),
            discover_timeout: Duration::from_secs(4),
        }
    }
}

/// Where the build comes from.
#[derive(Debug, Clone)]
pub enum InstallSource {
    Tarball(PathBuf),
    /// Newest release from the signed update manifest.
    Latest {
        channel: Channel,
        base_url: Url,
    },
}

#[derive(Debug, Clone, Default)]
pub struct InstallOptions {
    pub pin: bool,
    pub skip_steam: bool,
}

/// What a release tarball contains, read locally before upload.
#[derive(Debug, Clone, Default)]
pub struct TarballInfo {
    pub version: String,
    pub has_launcher: bool,
    pub artwork: BTreeMap<&'static str, Vec<u8>>,
}

/// Scan a release tarball: version from `RELEASE`, presence of the launcher
/// and `versions/<ver>/`, and Steam artwork from `versions/<ver>/share/steam/`.
pub fn inspect_tarball(path: &Path) -> Result<TarballInfo> {
    let fmt = fp_updater::archive::detect_format(path)?;
    let dec =
        fp_updater::archive::decoder(std::io::BufReader::new(std::fs::File::open(path)?), fmt)?;
    let mut ar = tar::Archive::new(dec);
    let mut info = TarballInfo::default();
    let mut art: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut version_dirs = Vec::new();
    for e in ar.entries()? {
        let mut e = e?;
        let p = e
            .path()?
            .to_string_lossy()
            .trim_start_matches("./")
            .to_string();
        if p == "RELEASE" {
            let mut s = String::new();
            e.read_to_string(&mut s)?;
            info.version = s.trim().to_string();
        } else if p == "frameplayer.sh" {
            info.has_launcher = true;
        } else if let Some(rest) = p.strip_prefix("versions/") {
            let mut parts = rest.splitn(2, '/');
            let ver = parts.next().unwrap_or_default().to_string();
            if !version_dirs.contains(&ver) {
                version_dirs.push(ver.clone());
            }
            if let Some(file) = parts.next().and_then(|r| r.strip_prefix("share/steam/")) {
                if let Some(stem) = file.strip_suffix(".png") {
                    if e.header().size()? <= 8 << 20 {
                        let mut buf = Vec::new();
                        e.read_to_end(&mut buf)?;
                        art.insert(format!("{ver}/{stem}"), buf);
                    }
                }
            }
        }
    }
    if info.version.is_empty() || !info.has_launcher {
        bail!(
            "{} is not a FramePlayer release tarball (missing RELEASE or frameplayer.sh)",
            path.display()
        );
    }
    fp_updater::layout::validate_version(&info.version)?;
    if !version_dirs.contains(&info.version) {
        bail!("tarball has no versions/{}/ directory", info.version);
    }
    for kind in ArtKind::ALL {
        if let Some(png) = art.remove(&format!("{}/{}", info.version, kind.file_stem())) {
            info.artwork.insert(kind.file_stem(), png);
        }
    }
    Ok(info)
}

/// Parse `key=value` lines.
fn kv(out: &str) -> BTreeMap<String, String> {
    out.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

/// Steam shortcut lookup hint for the app (its exe lives in the game dir).
fn app_hint() -> String {
    format!("devkit-game/{GAME_DIR}")
}

/// Report files copied back from the headset.
#[derive(Debug, Clone, Default)]
pub struct FetchedReports {
    pub json: Option<String>,
    pub txt: Option<String>,
}

/// Drives pairing and installation.
pub struct Installer {
    pub paths: Paths,
    reporter: Arc<dyn Reporter>,
    transport: TransportKind,
}

impl Installer {
    /// Uses [`TransportKind::from_env_or_default`].
    pub fn new(paths: Paths, reporter: Arc<dyn Reporter>) -> Self {
        Self {
            paths,
            reporter,
            transport: TransportKind::from_env_or_default(),
        }
    }

    pub fn with_transport(mut self, kind: TransportKind) -> Self {
        self.transport = kind;
        self
    }

    pub fn transport(&self) -> TransportKind {
        self.transport
    }

    /// Connection to a paired device over the configured transport.
    pub fn remote(&self, d: &Device) -> Result<Remote> {
        Ok(Remote::new(self.transport, self.target(d)?))
    }

    fn say(&self, e: Event) {
        self.reporter.event(e);
    }

    /// SSH target for a paired device (generates the key if missing).
    pub fn target(&self, d: &Device) -> Result<SshTarget> {
        sshkey::load_or_create(
            &self.paths.ssh_key(),
            &self.paths.ssh_pubkey(),
            &sshkey::default_comment(),
        )?;
        Ok(SshTarget::new(
            &d.host,
            &d.user,
            d.ssh_port,
            &self.paths.ssh_key(),
            &self.paths.known_hosts(),
        ))
    }

    /// The device to act on: by name/host, else the default.
    pub fn device(&self, which: Option<&str>) -> Result<Device> {
        let state = self.paths.load_state()?;
        state.find(which).cloned().ok_or_else(|| match which {
            Some(w) => {
                anyhow!("no paired headset named {w:?}; run `frameplayer-install pair` first")
            }
            None if state.devices.is_empty() => {
                anyhow!("no headset paired yet; run `frameplayer-install pair`")
            }
            None => anyhow!("several headsets are paired; choose one with --device"),
        })
    }

    pub async fn discover(&self, timeout: Duration) -> Result<Vec<Discovered>> {
        tokio::task::spawn_blocking(move || discovery::browse(timeout)).await?
    }

    /// Pair with a headset via the devkit service. Idempotent.
    pub async fn pair(&self, opts: &PairOptions) -> Result<Device> {
        if self.transport == TransportKind::System {
            SshTarget::ensure_client_available()?;
        }
        let key = sshkey::load_or_create(
            &self.paths.ssh_key(),
            &self.paths.ssh_pubkey(),
            &sshkey::default_comment(),
        )?;
        if key.created {
            self.say(Event::Info(format!(
                "Created SSH key {}",
                self.paths.ssh_key().display()
            )));
        }

        let (host, name, mut service_port) = match &opts.host {
            Some(h) => (
                h.clone(),
                opts.name.clone().unwrap_or_else(|| h.clone()),
                opts.service_port,
            ),
            None => {
                self.say(Event::Step(
                    "Looking for a Steam Frame in Developer Mode on your network…".into(),
                ));
                let found = self.discover(opts.discover_timeout).await?;
                match found.as_slice() {
                    [] => bail!(
                        "no headset found. Check that Developer Mode is on (Settings > System > Developer Mode) \
                         and that this computer is on the same network, or pass --host <ip>."
                    ),
                    [one] => {
                        let addr = one.best_address().ok_or_else(|| anyhow!("{} has no address", one.name))?;
                        self.say(Event::Info(format!("Found {} at {addr}", one.name)));
                        (addr.to_string(), opts.name.clone().unwrap_or_else(|| one.name.clone()), one.port)
                    }
                    many => bail!(
                        "found several headsets: {}. Pick one with --host <ip>.",
                        many.iter()
                            .map(|d| format!("{} ({})", d.name, d.best_address().map(|a| a.to_string()).unwrap_or_default()))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                }
            }
        };
        if service_port == 0 {
            service_port = DEVKIT_SERVICE_PORT;
        }

        self.say(Event::Step(format!(
            "Contacting the devkit service on {host}…"
        )));
        let client = DevkitClient::new(&host, service_port)?;
        let user = match &opts.user {
            Some(u) => u.clone(),
            None => {
                let from_props = client.properties().await.ok().and_then(|p| p.login());
                match from_props {
                    Some(u) => u,
                    None => client
                        .login_name()
                        .await
                        .context("could not learn the login name from the headset; pass --user")?,
                }
            }
        };
        let device = Device {
            name,
            host: host.clone(),
            service_port,
            ssh_port: opts.ssh_port,
            user,
        };
        let remote = self.remote(&device)?;

        if !remote.check().await {
            self.say(Event::ApproveOnHeadset {
                fingerprint: key.fingerprint.clone(),
            });
            match client
                .register(&key.public_openssh, opts.approve_timeout)
                .await?
            {
                RegisterOutcome::Denied(msg) => bail!("pairing was declined on the headset {msg}"),
                RegisterOutcome::Accepted(_) => {}
            }
            // The user just approved on the headset: a host key we recorded
            // earlier (before a reset/reflash) is stale, not an attack.
            match crate::transport::forget_host(&self.paths.known_hosts(), &host) {
                Ok(n) if n > 0 => tracing::info!("forgot {n} old host key(s) for {host}"),
                Ok(_) => {}
                Err(e) => tracing::warn!("could not update known_hosts: {e:#}"),
            }
            self.say(Event::Step("Waiting for SSH access…".into()));
            let deadline = Instant::now() + opts.approve_timeout;
            loop {
                if remote.check().await {
                    break;
                }
                if Instant::now() > deadline {
                    bail!("timed out waiting for approval on the headset");
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }

        let mut state = self.paths.load_state()?;
        state.upsert(device.clone());
        self.paths.save_state(&state)?;
        self.say(Event::Done(format!(
            "Paired with {} as {}@{}",
            device.name, device.user, device.host
        )));
        Ok(device)
    }

    /// Download the newest release to the local cache (verified).
    pub async fn fetch_latest(&self, channel: Channel, base_url: &Url) -> Result<PathBuf> {
        self.say(Event::Step(format!(
            "Checking the {channel} channel for the latest release…"
        )));
        let http =
            download::http_client(concat!("frameplayer-install/", env!("CARGO_PKG_VERSION")))?;
        let url = base_url.join(&channel.manifest_file())?;
        let sig_url = Url::parse(&format!("{url}.sig"))?;
        let body = download::fetch_bytes(&http, &url, download::MAX_METADATA_BYTES).await?;
        let sig = download::fetch_bytes(&http, &sig_url, 4096).await?;
        let m = ManifestVerifier::default().verify_and_parse(&body, &sig)?;
        let a = m
            .artifact_for("aarch64")
            .ok_or_else(|| anyhow!("release {} has no aarch64 build", m.version))?;
        let dest = self.paths.downloads().join(format!(
            "frameplayer-{}-aarch64.{}",
            m.version,
            a.format.extension()
        ));
        if dest.is_file() && fp_updater::sha256_file(&dest)?.eq_ignore_ascii_case(&a.sha256) {
            self.say(Event::Info(format!("Using cached {}", dest.display())));
            return Ok(dest);
        }
        self.say(Event::Step(format!(
            "Downloading FramePlayer {}…",
            m.version
        )));
        let rep = self.reporter.clone();
        download::download_resumable(
            &http,
            &a.url,
            &dest,
            Expect {
                size: Some(a.size),
                sha256: Some(&a.sha256),
            },
            &mut |done, total| rep.event(Event::Progress { done, total }),
        )
        .await?;
        Ok(dest)
    }

    /// Upload, extract, register with Steam, set artwork, optionally pin.
    /// Returns the installed version.
    pub async fn install(
        &self,
        device: &Device,
        source: &InstallSource,
        opts: &InstallOptions,
    ) -> Result<String> {
        let remote = self.remote(device)?;
        if let Err(e) = remote.verify().await {
            return Err(e.context(format!(
                "cannot log in to {} over SSH; run `frameplayer-install pair` again",
                device.host
            )));
        }
        let tarball = match source {
            InstallSource::Tarball(p) => p.clone(),
            InstallSource::Latest { channel, base_url } => {
                self.fetch_latest(*channel, base_url).await?
            }
        };
        let info = inspect_tarball(&tarball)?;
        let ext = fp_updater::archive::detect_format(&tarball)?.extension();
        let upload_rel = format!("{}/.frameplayer-upload.{ext}", remote::UPLOAD_DIR);

        self.say(Event::Step(format!(
            "Uploading FramePlayer {} to {}…",
            info.version, device.name
        )));
        remote.run_script(&remote::prepare_upload_script()).await?;
        let rep = self.reporter.clone();
        remote
            .upload(&tarball, &upload_rel, &mut |done, total| {
                rep.event(Event::Progress {
                    done,
                    total: Some(total),
                })
            })
            .await?;

        self.say(Event::Step("Installing on the headset…".into()));
        let out = remote
            .run_script(&remote::install_script(&upload_rel, GAME_DIR))
            .await?;
        let out = kv(&out.stdout_str());
        let home = out
            .get("home")
            .cloned()
            .unwrap_or_else(|| format!("/home/{}", device.user));
        if out.get("release").map(String::as_str) != Some(info.version.as_str()) {
            self.say(Event::Warn(format!(
                "headset reports release {:?}",
                out.get("release")
            )));
        }

        if !opts.skip_steam {
            if let Err(e) = self
                .register_with_steam(&remote, &home, &info, opts.pin)
                .await
            {
                self.say(Event::Warn(format!(
                    "installed, but adding it to the Steam library failed: {e:#}. \
                     It may still appear under Library > Non-Steam; otherwise add \
                     {home}/devkit-game/{GAME_DIR}/frameplayer.sh as a non-Steam game."
                )));
            }
        }
        self.say(Event::Done(format!(
            "FramePlayer {} installed. Find it in Library > Non-Steam on the headset.",
            info.version
        )));
        Ok(info.version)
    }

    async fn register_with_steam(
        &self,
        remote: &Remote,
        home: &str,
        info: &TarballInfo,
        pin: bool,
    ) -> Result<()> {
        self.say(Event::Step(
            "Adding FramePlayer to the Steam library…".into(),
        ));
        let devkit = remote
            .run_script(&remote::devkit_shortcut_script(GAME_DIR, DISPLAY_NAME))
            .await
            .map(|o| o.stdout_str().contains("devkit-utils: ok"))
            .unwrap_or(false);

        let tunnel = remote.open_tunnel("127.0.0.1", CEF_DEBUG_PORT).await?;
        let mut cdp = SteamCdp::connect(tunnel.local_port).await?;
        let root = format!("{home}/devkit-game/{GAME_DIR}");
        let exe = format!("{root}/frameplayer.sh");
        let appid = match cdp
            .find_shortcut(DISPLAY_NAME, &app_hint(), remote::PROBE_LAUNCHER_NAME)
            .await?
        {
            Some(id) => id,
            None => {
                if devkit {
                    self.say(Event::Info(
                        "Devkit shortcut not visible yet; adding a regular shortcut".into(),
                    ));
                }
                cdp.add_shortcut(DISPLAY_NAME, &exe, &root).await?
            }
        };
        for kind in ArtKind::ALL {
            if let Some(png) = info.artwork.get(kind.file_stem()) {
                if let Err(e) = cdp.set_artwork(appid, kind, png).await {
                    self.say(Event::Warn(format!(
                        "could not set {} artwork: {e:#}",
                        kind.file_stem()
                    )));
                }
            }
        }
        if pin {
            match cdp.pin(appid).await {
                Ok(()) => self.say(Event::Info("Pinned to Favorites".into())),
                Err(e) => self.say(Event::Warn(format!("could not pin: {e:#}"))),
            }
        }
        // Second entry for the self-test, so Steam can start it with the
        // headset's XR environment.
        if let Err(e) = self
            .ensure_probe_shortcut(remote, &mut cdp, home, info)
            .await
        {
            self.say(Event::Warn(format!(
                "could not add the {PROBE_DISPLAY_NAME} entry: {e:#}"
            )));
        }
        drop(tunnel);
        Ok(())
    }

    /// Find or create the "FramePlayer Self-Test" shortcut; returns its appid.
    async fn ensure_probe_shortcut(
        &self,
        remote: &Remote,
        cdp: &mut SteamCdp,
        home: &str,
        info: &TarballInfo,
    ) -> Result<u32> {
        remote
            .run_script(&remote::ensure_probe_launcher_script(GAME_DIR))
            .await?;
        if let Some(id) = cdp
            .find_shortcut(PROBE_DISPLAY_NAME, remote::PROBE_LAUNCHER_NAME, "")
            .await?
        {
            return Ok(id);
        }
        let root = format!("{home}/devkit-game/{GAME_DIR}");
        let exe = format!("{root}/{}", remote::PROBE_LAUNCHER_NAME);
        let id = cdp.add_shortcut(PROBE_DISPLAY_NAME, &exe, &root).await?;
        if let Some(png) = info.artwork.get(ArtKind::Icon.file_stem()) {
            let _ = cdp.set_artwork(id, ArtKind::Icon, png).await;
        }
        Ok(id)
    }

    /// Start FramePlayer through Steam (so it gets the right runtime env).
    pub async fn launch(&self, device: &Device) -> Result<()> {
        let remote = self.remote(device)?;
        let tunnel = remote.open_tunnel("127.0.0.1", CEF_DEBUG_PORT).await?;
        let mut cdp = SteamCdp::connect(tunnel.local_port).await?;
        let appid = cdp
            .find_shortcut(DISPLAY_NAME, &app_hint(), remote::PROBE_LAUNCHER_NAME)
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "FramePlayer is not in the Steam library; run `frameplayer-install install`"
                )
            })?;
        cdp.run(appid).await?;
        self.say(Event::Done("Launched FramePlayer on the headset".into()));
        Ok(())
    }

    pub async fn uninstall(&self, device: &Device, purge: bool) -> Result<()> {
        let remote = self.remote(device)?;
        match remote.open_tunnel("127.0.0.1", CEF_DEBUG_PORT).await {
            Ok(tunnel) => {
                let r: Result<()> = async {
                    let mut cdp = SteamCdp::connect(tunnel.local_port).await?;
                    if let Some(id) = cdp
                        .find_shortcut(DISPLAY_NAME, &app_hint(), remote::PROBE_LAUNCHER_NAME)
                        .await?
                    {
                        cdp.remove(id).await?;
                    }
                    if let Some(id) = cdp
                        .find_shortcut(PROBE_DISPLAY_NAME, remote::PROBE_LAUNCHER_NAME, "")
                        .await?
                    {
                        cdp.remove(id).await?;
                    }
                    Ok(())
                }
                .await;
                if let Err(e) = r {
                    self.say(Event::Warn(format!(
                        "could not remove the Steam shortcut: {e:#}"
                    )));
                }
            }
            Err(e) => self.say(Event::Warn(format!(
                "Steam not reachable ({e:#}); removing files only"
            ))),
        }
        remote
            .run_script(&remote::uninstall_script(GAME_DIR, purge))
            .await?;
        self.say(Event::Done(if purge {
            "FramePlayer and its settings were removed".into()
        } else {
            "FramePlayer was removed (settings and library database kept; use --purge to delete them)".into()
        }));
        Ok(())
    }

    pub async fn status(&self, device: &Device) -> Result<RemoteStatus> {
        let out = self
            .remote(device)?
            .run_script(&remote::status_script(GAME_DIR))
            .await?;
        Ok(remote::parse_status(&out.stdout_str()))
    }

    /// Stream logs to stdout.
    pub async fn logs(&self, device: &Device, lines: u32, follow: bool) -> Result<()> {
        let code = self
            .remote(device)?
            .run_script_streaming(
                &remote::logs_script(lines, follow),
                Box::new(|l| match l.strip_prefix("! ") {
                    Some(e) => eprintln!("{e}"),
                    None => println!("{l}"),
                }),
            )
            .await?;
        if code != 0 && !follow {
            bail!("could not read logs (exit {code})");
        }
        Ok(())
    }

    /// Interactive shell / one-off command (blocking, inherits the
    /// terminal). Always uses the system OpenSSH client (needs a TTY).
    pub fn shell(&self, device: &Device, command: Option<&str>) -> Result<i32> {
        SshTarget::ensure_client_available()?;
        let extra: &[&str] = if command.is_none() { &["-t"] } else { &[] };
        let mut c = self.target(device)?.ssh_command_with(extra, command);
        Ok(c.status()?.code().unwrap_or(1))
    }

    /// Run the self-test headless over SSH, streaming its output lines to
    /// `sink`. Returns the probe's exit code
    /// ([`remote::PROBE_MISSING_EXIT`] when the build has no probe,
    /// [`remote::NOT_INSTALLED_EXIT`] when FramePlayer isn't installed).
    pub async fn run_probe_headless(&self, device: &Device, sink: LineSink) -> Result<i32> {
        self.remote(device)?
            .run_script_streaming(&remote::probe_headless_script(GAME_DIR), sink)
            .await
    }

    /// Report file times and whether a probe is running.
    pub async fn probe_state(&self, device: &Device) -> Result<ProbeState> {
        let out = self
            .remote(device)?
            .run_script(&remote::probe_state_script())
            .await?;
        Ok(remote::parse_probe_state(&out.stdout_str()))
    }

    /// Copy the probe's report files from the headset (missing ones are
    /// `None`).
    pub async fn fetch_reports(&self, device: &Device) -> Result<FetchedReports> {
        let remote = self.remote(device)?;
        let dir = self.paths.dir.join("reports");
        std::fs::create_dir_all(&dir)?;
        let mut out = FetchedReports::default();
        for (name, slot) in [
            (crate::report::REMOTE_REPORT_JSON, &mut out.json),
            (crate::report::REMOTE_REPORT_TXT, &mut out.txt),
        ] {
            let local = dir.join(name);
            match remote.download(name, &local).await {
                Ok(()) => *slot = Some(std::fs::read_to_string(&local)?),
                Err(e) => tracing::info!("no {name}: {e:#}"),
            }
        }
        Ok(out)
    }

    /// Start the self-test through Steam (it needs the XR environment Steam
    /// provides), creating its library entry if needed.
    // [verify] A non-Steam shortcut started via SteamClient.Apps.RunGame gets
    // the OpenXR runtime environment on the Frame, and the probe's window /
    // XR session appears in the headset.
    pub async fn launch_probe(&self, device: &Device) -> Result<()> {
        let remote = self.remote(device)?;
        let home = remote
            .run_script("echo \"$HOME\"\n")
            .await?
            .stdout_str()
            .trim()
            .to_string();
        let tunnel = remote.open_tunnel("127.0.0.1", CEF_DEBUG_PORT).await?;
        let mut cdp = SteamCdp::connect(tunnel.local_port).await?;
        let appid = self
            .ensure_probe_shortcut(&remote, &mut cdp, &home, &TarballInfo::default())
            .await?;
        cdp.run(appid).await?;
        self.say(Event::Info(format!(
            "Started {PROBE_DISPLAY_NAME} on the headset"
        )));
        Ok(())
    }

    /// Find and download the newest GitHub release (pre-releases included).
    /// Returns the tarball path.
    pub async fn fetch_github_latest(&self) -> Result<PathBuf> {
        self.say(Event::Step(
            "Checking GitHub for the newest FramePlayer build…".into(),
        ));
        let http =
            download::http_client(concat!("frameplayer-install/", env!("CARGO_PKG_VERSION")))?;
        let sel = crate::github::latest_release(&http, "aarch64").await?;
        self.say(Event::Step(format!(
            "Downloading FramePlayer {}{} ({:.1} MB)…",
            sel.version,
            if sel.prerelease { " (test build)" } else { "" },
            sel.tarball.size as f64 / 1e6
        )));
        let rep = self.reporter.clone();
        let (path, verified) = crate::github::download_release(
            &http,
            &sel,
            &self.paths.downloads(),
            &mut |done, total| rep.event(Event::Progress { done, total }),
        )
        .await?;
        if verified {
            self.say(Event::Info("Download checked (SHA-256 matches).".into()));
        } else {
            self.say(Event::Warn(
                "this release has no .sha256 file; the download could not be verified".into(),
            ));
        }
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tarball(dir: &Path, entries: &[(&str, &[u8])]) -> PathBuf {
        let mut b = tar::Builder::new(Vec::new());
        for (p, d) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_size(d.len() as u64);
            h.set_mode(0o755);
            b.append_data(&mut h, p, *d).unwrap();
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&b.into_inner().unwrap()).unwrap();
        let p = dir.join("t.tar.gz");
        std::fs::write(&p, gz.finish().unwrap()).unwrap();
        p
    }

    #[test]
    fn inspects_release_tarball() {
        let d = tempfile::tempdir().unwrap();
        let p = tarball(
            d.path(),
            &[
                ("./frameplayer.sh", b"#!/bin/sh"),
                ("RELEASE", b"0.4.0\n"),
                ("versions/0.4.0/bin/frameplayer", b"elf"),
                ("versions/0.4.0/share/steam/hero.png", b"PNGHERO"),
                ("versions/0.4.0/share/steam/icon.png", b"PNGICON"),
                ("versions/0.3.0/share/steam/grid.png", b"OLD"),
            ],
        );
        let info = inspect_tarball(&p).unwrap();
        assert_eq!(info.version, "0.4.0");
        assert_eq!(info.artwork.get("hero").unwrap(), b"PNGHERO");
        assert!(
            !info.artwork.contains_key("grid"),
            "art from other versions ignored"
        );
    }

    #[test]
    fn rejects_non_release_tarballs() {
        let d = tempfile::tempdir().unwrap();
        let p = tarball(d.path(), &[("readme.txt", b"hi")]);
        assert!(inspect_tarball(&p).is_err());
        let p = tarball(d.path(), &[("frameplayer.sh", b""), ("RELEASE", b"1.0.0")]);
        assert!(inspect_tarball(&p)
            .unwrap_err()
            .to_string()
            .contains("versions/1.0.0"));
        let p = tarball(
            d.path(),
            &[("frameplayer.sh", b""), ("RELEASE", b"../../x")],
        );
        assert!(inspect_tarball(&p).is_err());
    }

    #[test]
    fn device_selection_messages() {
        let d = tempfile::tempdir().unwrap();
        let inst = Installer::new(Paths::at(d.path()), Arc::new(Silent));
        assert!(inst.device(None).unwrap_err().to_string().contains("pair"));
        let mut s = inst.paths.load_state().unwrap();
        s.upsert(Device {
            name: "f".into(),
            host: "10.0.0.2".into(),
            service_port: 32000,
            ssh_port: 22,
            user: "u".into(),
        });
        inst.paths.save_state(&s).unwrap();
        assert_eq!(inst.device(None).unwrap().host, "10.0.0.2");
        assert!(inst.device(Some("other")).is_err());
        let t = inst.target(&inst.device(Some("f")).unwrap()).unwrap();
        assert_eq!(t.destination(), "u@10.0.0.2");
        assert!(inst.paths.ssh_key().exists(), "key generated on demand");
    }

    #[test]
    fn kv_parsing() {
        let m = kv("release=0.1.0\nhome=/home/steamos\nnoise\n");
        assert_eq!(m["home"], "/home/steamos");
        assert_eq!(m.len(), 2);
    }
}
