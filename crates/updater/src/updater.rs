//! The orchestrator the app uses: check → download (delta or full) → install.
//!
//! ```ignore
//! let layout = InstallLayout::from_env().unwrap();
//! match layout.boot_check(&HealthPolicy::default())? { /* see BootOutcome */ }
//! let updater = Updater::new(UpdaterConfig::new(layout, env!("CARGO_PKG_VERSION").parse()?))?;
//! if let UpdateCheck::Available(plan) = updater.check().await? {
//!     let staged = updater.download(&plan, &mut |p| ui.progress(p)).await?;
//!     tokio::task::spawn_blocking(move || updater.install(&staged)).await??;
//!     // takes effect on next launch; the new version starts on trial
//! }
//! ```

use crate::archive::{self, ExtractOptions};
use crate::delta;
use crate::download::{self, Expect};
use crate::layout::{HealthPolicy, InstallLayout};
use crate::manifest::{Artifact, Channel, DeltaPatch, ReleaseManifest};
use crate::signing::ManifestVerifier;
use crate::{check_sha256, Result, UpdateError};
use semver::Version;
use std::path::PathBuf;
use url::Url;

/// Where release manifests are published (GitHub Pages of the repo).
pub const DEFAULT_UPDATE_BASE: &str = "https://yellkell.github.io/frameplayer/updates/";

/// Updater settings. Construct with [`UpdaterConfig::new`] and adjust.
#[derive(Debug, Clone)]
pub struct UpdaterConfig {
    pub layout: InstallLayout,
    /// Directory URL containing `<channel>.json` and `<channel>.json.sig`.
    pub base_url: Url,
    pub channel: Channel,
    pub current_version: Version,
    pub arch: String,
    pub steamos_version: Option<String>,
    pub policy: HealthPolicy,
    pub user_agent: String,
}

impl UpdaterConfig {
    pub fn new(layout: InstallLayout, current_version: Version) -> Self {
        Self {
            layout,
            base_url: Url::parse(DEFAULT_UPDATE_BASE).expect("valid default URL"),
            channel: Channel::Stable,
            user_agent: format!(
                "FramePlayer/{current_version} (SteamOS; {})",
                crate::platform::arch()
            ),
            current_version,
            arch: crate::platform::arch().to_string(),
            steamos_version: crate::platform::steamos_version(),
            policy: HealthPolicy::default(),
        }
    }
}

/// Outcome of [`Updater::check`].
#[derive(Debug, Clone)]
pub enum UpdateCheck {
    UpToDate {
        latest: Version,
    },
    Available(Box<UpdatePlan>),
    /// A newer version exists but cannot be installed here.
    Unavailable {
        version: Version,
        reason: String,
    },
}

/// Everything needed to download one update.
#[derive(Debug, Clone)]
pub struct UpdatePlan {
    pub manifest: ReleaseManifest,
    pub artifact: Artifact,
    /// Set when a patch from the running version exists *and* we still have
    /// that version's uncompressed tar cached.
    pub delta: Option<DeltaPatch>,
}

impl UpdatePlan {
    pub fn version(&self) -> &Version {
        &self.manifest.version
    }

    /// Bytes we expect to transfer (delta if usable, else full tarball).
    pub fn download_size(&self) -> u64 {
        self.delta.as_ref().map_or(self.artifact.size, |d| d.size)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadPhase {
    Delta,
    Full,
    Unpack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadProgress {
    pub phase: DownloadPhase,
    pub done: u64,
    pub total: Option<u64>,
}

/// A verified, uncompressed release tar waiting in `staging/`.
#[derive(Debug, Clone)]
pub struct StagedUpdate {
    pub version: Version,
    pub tar_path: PathBuf,
    pub used_delta: bool,
}

/// Talks to the update server and manages the install layout.
#[derive(Debug, Clone)]
pub struct Updater {
    cfg: UpdaterConfig,
    http: reqwest::Client,
    verifier: ManifestVerifier,
}

fn cached_tar(layout: &InstallLayout, v: &Version) -> PathBuf {
    layout.cache_dir().join(format!("{v}.tar"))
}

impl Updater {
    pub fn new(cfg: UpdaterConfig) -> Result<Self> {
        let http = download::http_client(&cfg.user_agent)?;
        Ok(Self {
            cfg,
            http,
            verifier: ManifestVerifier::default(),
        })
    }

    /// Replace the trusted keys (tests, staging channel).
    pub fn with_verifier(mut self, verifier: ManifestVerifier) -> Self {
        self.verifier = verifier;
        self
    }

    pub fn config(&self) -> &UpdaterConfig {
        &self.cfg
    }

    pub fn layout(&self) -> &InstallLayout {
        &self.cfg.layout
    }

    pub fn manifest_url(&self) -> Result<Url> {
        self.cfg
            .base_url
            .join(&self.cfg.channel.manifest_file())
            .map_err(|e| UpdateError::InvalidManifest(e.to_string()))
    }

    /// Download and verify the channel manifest.
    pub async fn fetch_manifest(&self) -> Result<ReleaseManifest> {
        let url = self.manifest_url()?;
        let sig_url = Url::parse(&format!("{url}.sig"))
            .map_err(|e| UpdateError::InvalidManifest(e.to_string()))?;
        let max = download::MAX_METADATA_BYTES;
        let (body, sig) = futures::try_join!(
            download::fetch_bytes(&self.http, &url, max),
            download::fetch_bytes(&self.http, &sig_url, 4096)
        )?;
        let m = self.verifier.verify_and_parse(&body, &sig)?;
        if m.channel != self.cfg.channel {
            return Err(UpdateError::InvalidManifest(format!(
                "{url} is a {} manifest, expected {}",
                m.channel, self.cfg.channel
            )));
        }
        Ok(m)
    }

    /// Decide whether an update is available and how to fetch it.
    pub fn plan(&self, m: ReleaseManifest) -> UpdateCheck {
        if !m.is_newer_than(&self.cfg.current_version) {
            return UpdateCheck::UpToDate { latest: m.version };
        }
        let unavailable = |reason: String| UpdateCheck::Unavailable {
            version: m.version.clone(),
            reason,
        };
        if self.cfg.layout.is_blocked(&m.version.to_string()) {
            return unavailable(
                "this version failed its health check here and was rolled back".into(),
            );
        }
        if !m.supports_steamos(self.cfg.steamos_version.as_deref()) {
            return unavailable(format!(
                "requires SteamOS {} or newer",
                m.min_steamos.as_deref().unwrap_or("?")
            ));
        }
        let Some(artifact) = m.artifact_for(&self.cfg.arch).cloned() else {
            return unavailable(format!("no build for {}", self.cfg.arch));
        };
        let delta = artifact
            .delta_from(&self.cfg.current_version)
            .filter(|_| cached_tar(&self.cfg.layout, &self.cfg.current_version).is_file())
            .cloned();
        UpdateCheck::Available(Box::new(UpdatePlan {
            manifest: m,
            artifact,
            delta,
        }))
    }

    pub async fn check(&self) -> Result<UpdateCheck> {
        Ok(self.plan(self.fetch_manifest().await?))
    }

    /// Fetch the update into `staging/` as a verified uncompressed tar.
    /// Tries the delta first and falls back to the full tarball.
    pub async fn download(
        &self,
        plan: &UpdatePlan,
        progress: &mut (dyn FnMut(DownloadProgress) + Send),
    ) -> Result<StagedUpdate> {
        let layout = &self.cfg.layout;
        let staging = layout.staging_dir();
        tokio::fs::create_dir_all(&staging).await?;
        let version = plan.version().clone();
        let tar_path = staging.join(format!("{version}.tar"));

        if let Some(d) = &plan.delta {
            match self.try_delta(plan, d, &tar_path, progress).await {
                Ok(()) => {
                    return Ok(StagedUpdate {
                        version,
                        tar_path,
                        used_delta: true,
                    })
                }
                Err(e) => tracing::warn!("delta update failed, falling back to full download: {e}"),
            }
        }

        let a = &plan.artifact;
        let packed = staging.join(format!(
            "frameplayer-{version}-{}.{}",
            a.arch,
            a.format.extension()
        ));
        download::download_resumable(
            &self.http,
            &a.url,
            &packed,
            Expect {
                size: Some(a.size),
                sha256: Some(&a.sha256),
            },
            &mut |done, total| {
                progress(DownloadProgress {
                    phase: DownloadPhase::Full,
                    done,
                    total,
                })
            },
        )
        .await?;
        progress(DownloadProgress {
            phase: DownloadPhase::Unpack,
            done: 0,
            total: a.unpacked_size,
        });
        let (fmt, src, dst) = (a.format, packed.clone(), tar_path.clone());
        let (_, sha) =
            tokio::task::spawn_blocking(move || archive::decompress_to_tar(&src, fmt, &dst))
                .await
                .map_err(join_err)??;
        if let Some(want) = &a.unpacked_sha256 {
            check_sha256("unpacked tarball", want, &sha)?;
        }
        let _ = tokio::fs::remove_file(&packed).await;
        Ok(StagedUpdate {
            version,
            tar_path,
            used_delta: false,
        })
    }

    async fn try_delta(
        &self,
        plan: &UpdatePlan,
        d: &DeltaPatch,
        tar_path: &std::path::Path,
        progress: &mut (dyn FnMut(DownloadProgress) + Send),
    ) -> Result<()> {
        let layout = &self.cfg.layout;
        let patch = layout
            .staging_dir()
            .join(format!("{}-from-{}.fpd", plan.version(), d.from));
        download::download_resumable(
            &self.http,
            &d.url,
            &patch,
            Expect {
                size: Some(d.size),
                sha256: Some(&d.sha256),
            },
            &mut |done, total| {
                progress(DownloadProgress {
                    phase: DownloadPhase::Delta,
                    done,
                    total,
                })
            },
        )
        .await?;
        progress(DownloadProgress {
            phase: DownloadPhase::Unpack,
            done: 0,
            total: plan.artifact.unpacked_size,
        });
        let base = cached_tar(layout, &self.cfg.current_version);
        let (p, out) = (patch.clone(), tar_path.to_path_buf());
        let header = tokio::task::spawn_blocking(move || delta::apply_files(&base, &p, &out))
            .await
            .map_err(join_err)?;
        let _ = tokio::fs::remove_file(&patch).await;
        let header = header?;
        let want = plan.artifact.unpacked_sha256.as_deref().unwrap_or_default();
        check_sha256("patched tarball", want, &hex::encode(header.target_sha256))
    }

    /// Extract the staged tar into the layout and activate it (blocking; call
    /// from `spawn_blocking`). Takes effect on the next launch.
    pub fn install(&self, staged: &StagedUpdate) -> Result<()> {
        let layout = &self.cfg.layout;
        let ver = staged.version.to_string();
        let tree = layout.staging_dir().join(format!("tree-{ver}"));
        let _ = std::fs::remove_dir_all(&tree);
        let file = std::io::BufReader::new(std::fs::File::open(&staged.tar_path)?);
        archive::extract_tar(file, &tree, &ExtractOptions::default())?;
        layout.install_tree(&tree, &ver, &self.cfg.policy)?;
        let _ = std::fs::remove_dir_all(&tree);

        // Keep the new tar as the base for the next delta; keep the running
        // version's tar in case we roll back to it.
        std::fs::create_dir_all(layout.cache_dir())?;
        std::fs::rename(&staged.tar_path, cached_tar(layout, &staged.version))?;
        let keep = [
            staged.version.to_string(),
            self.cfg.current_version.to_string(),
        ];
        for e in std::fs::read_dir(layout.cache_dir())?.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !keep.iter().any(|k| name == format!("{k}.tar")) {
                let _ = std::fs::remove_file(e.path());
            }
        }
        layout.gc()?;
        Ok(())
    }

    /// Check, download and install in one go. Returns the installed version.
    pub async fn update(
        &self,
        progress: &mut (dyn FnMut(DownloadProgress) + Send),
    ) -> Result<Option<Version>> {
        let UpdateCheck::Available(plan) = self.check().await? else {
            return Ok(None);
        };
        let staged = self.download(&plan, progress).await?;
        let me = self.clone();
        let s = staged.clone();
        tokio::task::spawn_blocking(move || me.install(&s))
            .await
            .map_err(join_err)??;
        Ok(Some(staged.version))
    }

    /// Seed the delta cache with the running version's tar (e.g. after the
    /// first install, if the installer left the tarball around).
    pub fn seed_cache(&self, packed_tarball: &std::path::Path) -> Result<()> {
        std::fs::create_dir_all(self.cfg.layout.cache_dir())?;
        let fmt = archive::detect_format(packed_tarball)?;
        archive::decompress_to_tar(
            packed_tarball,
            fmt,
            &cached_tar(&self.cfg.layout, &self.cfg.current_version),
        )?;
        Ok(())
    }
}

fn join_err(e: tokio::task::JoinError) -> UpdateError {
    UpdateError::Io(std::io::Error::other(e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::testserver::Server;
    use crate::manifest::ArchiveFormat;
    use crate::signing::{generate_keypair, parse_public_key, parse_secret_key, sign};
    use crate::{sha256_hex, BootOutcome};
    use std::io::Write;

    fn gz(data: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    struct Fixture {
        srv: Server,
        base: String,
        sk: ed25519_dalek::SigningKey,
        verifier: ManifestVerifier,
    }

    impl Fixture {
        async fn new() -> Self {
            let srv = Server::default();
            let base = srv.start().await;
            let (sk, pk) = generate_keypair();
            Self {
                srv,
                base,
                sk: parse_secret_key(&sk).unwrap(),
                verifier: ManifestVerifier::with_keys(vec![parse_public_key(&pk).unwrap()]),
            }
        }

        /// Publish `version` as the stable release, optionally with a delta.
        fn publish(&self, version: &str, delta_from: Option<(&str, &[u8])>) -> Vec<u8> {
            let tar = crate::archive::tests::sample_tar(version);
            let packed = gz(&tar);
            let tar_name = format!("/frameplayer-{version}-aarch64.tar.gz");
            self.srv.put(&tar_name, packed.clone());
            let mut deltas = vec![];
            if let Some((from, old_tar)) = delta_from {
                let patch = delta::create(old_tar, &tar, 64);
                let name = format!("/{version}-from-{from}.fpd");
                self.srv.put(&name, patch.clone());
                deltas.push(DeltaPatch {
                    from: from.parse().unwrap(),
                    url: Url::parse(&format!("{}{name}", self.base)).unwrap(),
                    size: patch.len() as u64,
                    sha256: sha256_hex(&patch),
                });
            }
            let m = ReleaseManifest {
                schema: 1,
                name: "frameplayer".into(),
                version: version.parse().unwrap(),
                channel: Channel::Stable,
                published: chrono::Utc::now(),
                min_steamos: None,
                notes: "test".into(),
                notes_url: None,
                artifacts: vec![Artifact {
                    arch: "aarch64".into(),
                    format: ArchiveFormat::TarGz,
                    url: Url::parse(&format!("{}{tar_name}", self.base)).unwrap(),
                    size: packed.len() as u64,
                    sha256: sha256_hex(&packed),
                    unpacked_size: Some(tar.len() as u64),
                    unpacked_sha256: Some(sha256_hex(&tar)),
                    deltas,
                }],
            };
            let json = m.to_json_pretty().unwrap();
            self.srv
                .put("/updates/stable.json", json.clone().into_bytes());
            self.srv.put(
                "/updates/stable.json.sig",
                sign(&self.sk, json.as_bytes()).into_bytes(),
            );
            tar
        }

        fn updater(&self, layout: &InstallLayout, current: &str) -> Updater {
            let mut cfg = UpdaterConfig::new(layout.clone(), current.parse().unwrap());
            cfg.base_url = Url::parse(&format!("{}/updates/", self.base)).unwrap();
            cfg.arch = "aarch64".into();
            cfg.steamos_version = None;
            Updater::new(cfg)
                .unwrap()
                .with_verifier(self.verifier.clone())
        }
    }

    fn seed_installed(root: &std::path::Path, v: &str) -> InstallLayout {
        let l = InstallLayout::new(root);
        crate::archive::extract_tar(
            crate::archive::tests::sample_tar(v).as_slice(),
            root,
            &Default::default(),
        )
        .unwrap();
        l.adopt_release(&HealthPolicy::default()).unwrap();
        l
    }

    #[tokio::test]
    async fn full_then_delta_update() {
        let fx = Fixture::new().await;
        let dir = tempfile::tempdir().unwrap();
        let layout = seed_installed(dir.path(), "0.1.0");

        // 0.2.0 full download (no cached tar for 0.1.0).
        let tar2 = fx.publish(
            "0.2.0",
            Some(("0.1.0", &crate::archive::tests::sample_tar("0.1.0"))),
        );
        let up = fx.updater(&layout, "0.1.0");
        let UpdateCheck::Available(plan) = up.check().await.unwrap() else {
            panic!("expected update")
        };
        assert!(plan.delta.is_none(), "no cached base tar yet");
        let mut phases = vec![];
        let staged = up
            .download(&plan, &mut |p| phases.push(p.phase))
            .await
            .unwrap();
        assert!(!staged.used_delta);
        assert!(phases.contains(&DownloadPhase::Full));
        up.install(&staged).unwrap();
        assert_eq!(layout.current_version().as_deref(), Some("0.2.0"));
        assert_eq!(layout.previous_version().as_deref(), Some("0.1.0"));
        assert!(matches!(
            layout
                .boot_check_with(&HealthPolicy::default(), false)
                .unwrap(),
            BootOutcome::Trial { .. }
        ));
        layout.mark_healthy().unwrap();

        // 0.3.0 via delta from the cached 0.2.0 tar.
        fx.publish("0.3.0", Some(("0.2.0", &tar2)));
        let up = fx.updater(&layout, "0.2.0");
        let UpdateCheck::Available(plan) = up.check().await.unwrap() else {
            panic!()
        };
        assert!(plan.delta.is_some());
        assert!(plan.download_size() < plan.artifact.size * 2);
        let staged = up.download(&plan, &mut |_| {}).await.unwrap();
        assert!(staged.used_delta);
        up.install(&staged).unwrap();
        assert_eq!(layout.current_version().as_deref(), Some("0.3.0"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("current/bin/frameplayer")).unwrap(),
            "binary 0.3.0"
        );
        // GC removed 0.1.0; cache holds only 0.3.0 and 0.2.0.
        assert_eq!(layout.installed_versions(), ["0.2.0", "0.3.0"]);
        let up = fx.updater(&layout, "0.3.0");
        assert!(matches!(
            up.check().await.unwrap(),
            UpdateCheck::UpToDate { .. }
        ));
    }

    #[tokio::test]
    async fn corrupt_delta_falls_back_to_full() {
        let fx = Fixture::new().await;
        let dir = tempfile::tempdir().unwrap();
        let layout = seed_installed(dir.path(), "1.0.0");
        let old_tar = crate::archive::tests::sample_tar("1.0.0");
        std::fs::create_dir_all(layout.cache_dir()).unwrap();
        // Cached base differs from what the patch expects.
        std::fs::write(layout.cache_dir().join("1.0.0.tar"), b"not the real tar").unwrap();
        fx.publish("1.1.0", Some(("1.0.0", &old_tar)));
        let up = fx.updater(&layout, "1.0.0");
        let UpdateCheck::Available(plan) = up.check().await.unwrap() else {
            panic!()
        };
        assert!(plan.delta.is_some());
        let staged = up.download(&plan, &mut |_| {}).await.unwrap();
        assert!(!staged.used_delta);
        up.install(&staged).unwrap();
        assert_eq!(layout.current_version().as_deref(), Some("1.1.0"));
    }

    #[tokio::test]
    async fn rejects_bad_signature_and_blocked_versions() {
        let fx = Fixture::new().await;
        let dir = tempfile::tempdir().unwrap();
        let layout = seed_installed(dir.path(), "0.1.0");
        fx.publish("0.2.0", None);
        let up = fx.updater(&layout, "0.1.0");

        // Blocked after a failed trial.
        std::fs::write(dir.path().join("blocked"), "0.2.0\n").unwrap();
        assert!(matches!(
            up.check().await.unwrap(),
            UpdateCheck::Unavailable { .. }
        ));

        // Signature from an untrusted key.
        let (other, _) = generate_keypair();
        let json = fx.srv.files.lock().unwrap()["/updates/stable.json"].clone();
        fx.srv.put(
            "/updates/stable.json.sig",
            sign(&parse_secret_key(&other).unwrap(), &json).into_bytes(),
        );
        assert!(matches!(
            up.check().await,
            Err(UpdateError::BadSignature(_))
        ));
    }

    #[tokio::test]
    async fn update_convenience_and_steamos_gate() {
        let fx = Fixture::new().await;
        let dir = tempfile::tempdir().unwrap();
        let layout = seed_installed(dir.path(), "0.1.0");
        fx.publish("0.2.0", None);
        let mut up = fx.updater(&layout, "0.1.0");
        let m = up.fetch_manifest().await.unwrap();
        let mut gated = m.clone();
        gated.min_steamos = Some("99".into());
        up.cfg.steamos_version = Some("3.8".into());
        assert!(matches!(up.plan(gated), UpdateCheck::Unavailable { .. }));
        assert_eq!(
            up.update(&mut |_| {}).await.unwrap(),
            Some("0.2.0".parse().unwrap())
        );
        assert_eq!(layout.current_version().as_deref(), Some("0.2.0"));
    }
}
