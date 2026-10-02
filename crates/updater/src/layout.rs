//! On-disk install layout, atomic version switching, rollback and the
//! first-launch health check.
//!
//! ```text
//! <root>/                      e.g. ~/devkit-game/frameplayer
//!   frameplayer.sh             launcher (from the tarball; Steam runs this)
//!   RELEASE                    version the last *tarball install* delivered
//!   .installed-release         version the launcher/updater last adopted
//!   versions/<ver>/bin/frameplayer …
//!   current  -> versions/<ver> switched with rename(2) of a temp symlink
//!   previous -> versions/<ver> kept for rollback
//!   trial                      "<ver> <attempts> <max_attempts>" while unproven
//!   blocked                    versions that failed their trial, one per line
//!   staging/  cache/           updater scratch space (same filesystem as root)
//! ```
//!
//! A newly activated version is *on trial* until it calls
//! [`InstallLayout::mark_healthy`] (normally via [`LaunchGuard`] after it has
//! been up for [`HealthPolicy::healthy_after`]). Every launch while on trial
//! increments the attempt counter; once it exceeds
//! [`HealthPolicy::max_attempts`], `current` is pointed back at `previous` and
//! the bad version is added to `blocked`. The counting happens in
//! `dist/frameplayer.sh` (so a binary that cannot even start is still caught)
//! and, when started without the launcher, in [`InstallLayout::boot_check`].
//! Both use the same plain-text files.

use crate::{Result, UpdateError};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const VERSIONS_DIR: &str = "versions";
pub const CURRENT_LINK: &str = "current";
pub const PREVIOUS_LINK: &str = "previous";
pub const TRIAL_FILE: &str = "trial";
pub const BLOCKED_FILE: &str = "blocked";
pub const RELEASE_FILE: &str = "RELEASE";
pub const INSTALLED_RELEASE_FILE: &str = ".installed-release";
pub const LAUNCHER: &str = "frameplayer.sh";
pub const STAGING_DIR: &str = "staging";
pub const CACHE_DIR: &str = "cache";
/// Set by `frameplayer.sh` when it has already counted this launch.
pub const LAUNCHER_COUNTED_ENV: &str = "FP_LAUNCHER_COUNTED";
/// Exported by `frameplayer.sh` so the app can find its install root.
pub const INSTALL_ROOT_ENV: &str = "FP_INSTALL_ROOT";

/// How a new version proves itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthPolicy {
    /// Launches allowed without reaching healthy before rolling back.
    pub max_attempts: u32,
    /// Uptime after which a running version counts as healthy.
    pub healthy_after: Duration,
}

impl Default for HealthPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            healthy_after: Duration::from_secs(20),
        }
    }
}

/// A version currently on trial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trial {
    pub version: String,
    pub attempts: u32,
    pub max_attempts: u32,
}

impl Trial {
    fn parse(s: &str) -> Option<Self> {
        let mut it = s.split_whitespace();
        Some(Self {
            version: it.next()?.to_string(),
            attempts: it.next()?.parse().ok()?,
            max_attempts: it.next()?.parse().ok()?,
        })
    }

    fn render(&self) -> String {
        format!("{} {} {}\n", self.version, self.attempts, self.max_attempts)
    }
}

/// Result of [`InstallLayout::boot_check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootOutcome {
    /// Not running from a managed install (dev build, tests).
    NotInstalled,
    /// Current version is proven.
    Healthy { version: String },
    /// Current version is on trial; this is launch number `attempt`.
    Trial { version: String, attempt: u32 },
    /// The trial failed and `current` now points at `to`. The running
    /// process is the bad build: it should re-exec the launcher and exit.
    RolledBack { from: String, to: String },
}

/// Version strings become directory names: allow semver only.
pub fn validate_version(v: &str) -> Result<()> {
    let ok = !v.is_empty()
        && v.len() <= 64
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".+-".contains(&b))
        && semver::Version::parse(v).is_ok();
    if ok {
        Ok(())
    } else {
        Err(UpdateError::Layout(format!("invalid version name {v:?}")))
    }
}

/// Handle on an install root.
#[derive(Debug, Clone)]
pub struct InstallLayout {
    root: PathBuf,
}

impl InstallLayout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Locate the install root of the running binary: `$FP_INSTALL_ROOT`, or
    /// two levels above `versions/<ver>/bin/frameplayer`.
    pub fn from_env() -> Option<Self> {
        if let Some(r) = std::env::var_os(INSTALL_ROOT_ENV) {
            return Some(Self::new(r));
        }
        let exe = std::env::current_exe().ok()?;
        // exe = <root>/versions/<ver>/bin/frameplayer (current/ is resolved by the kernel)
        let ver_dir = exe.parent()?.parent()?;
        let versions = ver_dir.parent()?;
        (versions.file_name()? == VERSIONS_DIR)
            .then(|| Self::new(versions.parent().unwrap_or(versions)))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn versions_dir(&self) -> PathBuf {
        self.root.join(VERSIONS_DIR)
    }
    pub fn version_dir(&self, v: &str) -> PathBuf {
        self.versions_dir().join(v)
    }
    pub fn staging_dir(&self) -> PathBuf {
        self.root.join(STAGING_DIR)
    }
    pub fn cache_dir(&self) -> PathBuf {
        self.root.join(CACHE_DIR)
    }
    pub fn launcher(&self) -> PathBuf {
        self.root.join(LAUNCHER)
    }

    fn read_link_version(&self, name: &str) -> Option<String> {
        let target = fs::read_link(self.root.join(name)).ok()?;
        let mut comps = target.components();
        let first = comps.next()?.as_os_str().to_str()?;
        let ver = comps.next()?.as_os_str().to_str()?.to_string();
        (first == VERSIONS_DIR && comps.next().is_none() && validate_version(&ver).is_ok())
            .then_some(ver)
    }

    /// Version `current` points at.
    pub fn current_version(&self) -> Option<String> {
        self.read_link_version(CURRENT_LINK)
    }

    /// Version kept for rollback.
    pub fn previous_version(&self) -> Option<String> {
        self.read_link_version(PREVIOUS_LINK)
    }

    /// All version directories present, sorted by semver ascending.
    pub fn installed_versions(&self) -> Vec<String> {
        let mut v: Vec<semver::Version> = fs::read_dir(self.versions_dir())
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| validate_version(n).is_ok())
            .filter_map(|n| semver::Version::parse(&n).ok())
            .collect();
        v.sort();
        v.into_iter().map(|v| v.to_string()).collect()
    }

    pub fn trial(&self) -> Option<Trial> {
        Trial::parse(&fs::read_to_string(self.root.join(TRIAL_FILE)).ok()?)
    }

    pub fn blocked_versions(&self) -> Vec<String> {
        fs::read_to_string(self.root.join(BLOCKED_FILE))
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect()
    }

    pub fn is_blocked(&self, v: &str) -> bool {
        self.blocked_versions().iter().any(|b| b == v)
    }

    /// Remove a version from the blocklist (user explicitly retries it).
    pub fn unblock(&self, v: &str) -> Result<()> {
        let rest: String = self
            .blocked_versions()
            .into_iter()
            .filter(|b| b != v)
            .map(|b| b + "\n")
            .collect();
        write_atomic(&self.root.join(BLOCKED_FILE), rest.as_bytes())
    }

    /// Atomically point `name` at `versions/<ver>`: create a temp symlink and
    /// rename(2) it over the old one, then fsync the directory.
    fn swap_link(&self, name: &str, ver: &str) -> Result<()> {
        validate_version(ver)?;
        let target = Path::new(VERSIONS_DIR).join(ver);
        let tmp = self
            .root
            .join(format!(".{name}.tmp-{}", std::process::id()));
        let _ = fs::remove_file(&tmp);
        symlink(&target, &tmp)?;
        if let Err(e) = fs::rename(&tmp, self.root.join(name)) {
            let _ = fs::remove_file(&tmp);
            return Err(e.into());
        }
        sync_dir(&self.root);
        Ok(())
    }

    /// Make `ver` current. The old current becomes `previous` and the new
    /// version goes on trial (unless there was nothing to roll back to).
    pub fn activate(&self, ver: &str, policy: &HealthPolicy) -> Result<()> {
        validate_version(ver)?;
        if !self.version_dir(ver).is_dir() {
            return Err(UpdateError::Layout(format!(
                "version {ver} is not installed"
            )));
        }
        let old = self.current_version();
        if old.as_deref() == Some(ver) {
            return Ok(());
        }
        if let Some(old) = &old {
            self.swap_link(PREVIOUS_LINK, old)?;
        }
        // Trial file before the switch: a crash in between leaves a trial for
        // a non-current version, which boot_check discards.
        if old.is_some() {
            let t = Trial {
                version: ver.to_string(),
                attempts: 0,
                max_attempts: policy.max_attempts,
            };
            write_atomic(&self.root.join(TRIAL_FILE), t.render().as_bytes())?;
        } else {
            let _ = fs::remove_file(self.root.join(TRIAL_FILE));
        }
        self.swap_link(CURRENT_LINK, ver)?;
        write_atomic(
            &self.root.join(INSTALLED_RELEASE_FILE),
            format!("{ver}\n").as_bytes(),
        )?;
        tracing::info!(version = ver, previous = ?old, "activated version");
        Ok(())
    }

    /// Point `current` back at `previous` and block the failed version.
    /// Returns the version now current, or `None` if there is no previous.
    pub fn rollback(&self) -> Result<Option<String>> {
        let (Some(bad), Some(prev)) = (self.current_version(), self.previous_version()) else {
            return Ok(None);
        };
        if !self.version_dir(&prev).is_dir() || prev == bad {
            return Ok(None);
        }
        if !self.is_blocked(&bad) {
            let mut f = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.root.join(BLOCKED_FILE))?;
            writeln!(f, "{bad}")?;
        }
        self.swap_link(CURRENT_LINK, &prev)?;
        let _ = fs::remove_file(self.root.join(TRIAL_FILE));
        tracing::warn!(from = %bad, to = %prev, "rolled back failed version");
        Ok(Some(prev))
    }

    /// Clear the trial for the current version.
    pub fn mark_healthy(&self) -> Result<()> {
        if let Some(t) = self.trial() {
            if Some(&t.version) == self.current_version().as_ref() {
                fs::remove_file(self.root.join(TRIAL_FILE))?;
                tracing::info!(version = %t.version, "version marked healthy");
            }
        }
        Ok(())
    }

    /// Adopt a version delivered by an external flat tarball install (Frame
    /// Control, FrameDrop, frameplayer-install): if `RELEASE` names a version
    /// we have not adopted yet, activate it. Mirrors `frameplayer.sh`.
    pub fn adopt_release(&self, policy: &HealthPolicy) -> Result<Option<String>> {
        let read = |n: &str| {
            fs::read_to_string(self.root.join(n))
                .ok()
                .map(|s| s.trim().to_string())
        };
        let Some(release) = read(RELEASE_FILE).filter(|r| validate_version(r).is_ok()) else {
            return Ok(None);
        };
        if read(INSTALLED_RELEASE_FILE).as_deref() == Some(release.as_str())
            && self.current_version().is_some()
        {
            return Ok(None);
        }
        if !self.version_dir(&release).is_dir() {
            return Ok(None);
        }
        self.activate(&release, policy)?;
        write_atomic(
            &self.root.join(INSTALLED_RELEASE_FILE),
            format!("{release}\n").as_bytes(),
        )?;
        Ok(Some(release))
    }

    /// Run at the very start of the app. Adopts external installs, counts the
    /// launch if the launcher did not, and rolls back an exhausted trial.
    pub fn boot_check(&self, policy: &HealthPolicy) -> Result<BootOutcome> {
        let counted = std::env::var_os(LAUNCHER_COUNTED_ENV).is_some();
        self.boot_check_with(policy, counted)
    }

    /// [`Self::boot_check`] with explicit launcher-counted flag (testable).
    pub fn boot_check_with(
        &self,
        policy: &HealthPolicy,
        launcher_counted: bool,
    ) -> Result<BootOutcome> {
        if !launcher_counted {
            self.adopt_release(policy)?;
        }
        let Some(current) = self.current_version() else {
            return Ok(BootOutcome::NotInstalled);
        };
        let Some(mut trial) = self.trial() else {
            return Ok(BootOutcome::Healthy { version: current });
        };
        if trial.version != current {
            let _ = fs::remove_file(self.root.join(TRIAL_FILE));
            return Ok(BootOutcome::Healthy { version: current });
        }
        if !launcher_counted {
            trial.attempts += 1;
        }
        if trial.attempts > trial.max_attempts {
            return Ok(match self.rollback()? {
                Some(to) => BootOutcome::RolledBack { from: current, to },
                None => {
                    // Nothing to fall back to: stop counting and carry on.
                    let _ = fs::remove_file(self.root.join(TRIAL_FILE));
                    BootOutcome::Healthy { version: current }
                }
            });
        }
        if !launcher_counted {
            write_atomic(&self.root.join(TRIAL_FILE), trial.render().as_bytes())?;
        }
        Ok(BootOutcome::Trial {
            version: current,
            attempt: trial.attempts,
        })
    }

    /// Start a health watch for this process (see [`LaunchGuard`]).
    pub fn launch_guard(&self, policy: &HealthPolicy) -> LaunchGuard {
        LaunchGuard {
            layout: self.clone(),
            started: Instant::now(),
            healthy_after: policy.healthy_after,
            done: self.trial().is_none(),
        }
    }

    /// Move an extracted release tree (`<tree>/versions/<ver>/…`, optional
    /// `<tree>/frameplayer.sh`) into the layout and activate it.
    /// `tree` must be on the same filesystem as the root (use `staging/`).
    pub fn install_tree(&self, tree: &Path, ver: &str, policy: &HealthPolicy) -> Result<()> {
        validate_version(ver)?;
        let src = tree.join(VERSIONS_DIR).join(ver);
        if !src.is_dir() {
            return Err(UpdateError::Layout(format!(
                "release tree has no {VERSIONS_DIR}/{ver} directory"
            )));
        }
        fs::create_dir_all(self.versions_dir())?;
        let dest = self.version_dir(ver);
        if self.current_version().as_deref() == Some(ver) {
            return Err(UpdateError::Layout(format!(
                "{ver} is already the running version"
            )));
        }
        if dest.exists() {
            let trash = self
                .versions_dir()
                .join(format!(".{ver}.old-{}", std::process::id()));
            fs::rename(&dest, &trash)?;
            fs::remove_dir_all(&trash)?;
        }
        fs::rename(&src, &dest)?;
        let launcher = tree.join(LAUNCHER);
        if launcher.is_file() {
            let tmp = self.root.join(format!(".{LAUNCHER}.tmp"));
            fs::copy(&launcher, &tmp)?;
            fs::rename(&tmp, self.launcher())?;
        }
        // Record the adoption first so the launcher never re-switches.
        write_atomic(
            &self.root.join(INSTALLED_RELEASE_FILE),
            format!("{ver}\n").as_bytes(),
        )?;
        write_atomic(&self.root.join(RELEASE_FILE), format!("{ver}\n").as_bytes())?;
        self.activate(ver, policy)
    }

    /// Delete versions that are neither current, previous nor on trial, and
    /// clear staging. Returns removed versions.
    pub fn gc(&self) -> Result<Vec<String>> {
        let keep: Vec<String> = [
            self.current_version(),
            self.previous_version(),
            self.trial().map(|t| t.version),
        ]
        .into_iter()
        .flatten()
        .collect();
        let mut removed = Vec::new();
        for v in self.installed_versions() {
            if !keep.contains(&v) {
                fs::remove_dir_all(self.version_dir(&v))?;
                removed.push(v);
            }
        }
        // Leftover temp dirs from interrupted installs.
        for e in fs::read_dir(self.versions_dir())
            .into_iter()
            .flatten()
            .flatten()
        {
            if e.file_name().to_string_lossy().starts_with('.') {
                let _ = fs::remove_dir_all(e.path());
            }
        }
        let _ = fs::remove_dir_all(self.staging_dir());
        Ok(removed)
    }

    /// Replace this process with the launcher (after a rollback at boot).
    #[cfg(unix)]
    pub fn reexec_launcher(&self) -> std::io::Error {
        use std::os::unix::process::CommandExt;
        std::process::Command::new(self.launcher())
            .args(std::env::args_os().skip(1))
            .env_remove(LAUNCHER_COUNTED_ENV)
            .exec()
    }
}

/// Marks the running version healthy once it has been up long enough.
/// Call [`LaunchGuard::poll`] from the main loop (cheap; touches disk once).
#[derive(Debug)]
pub struct LaunchGuard {
    layout: InstallLayout,
    started: Instant,
    healthy_after: Duration,
    done: bool,
}

impl LaunchGuard {
    /// Returns `true` once the version has been marked healthy.
    pub fn poll(&mut self) -> bool {
        if !self.done && self.started.elapsed() >= self.healthy_after {
            self.mark_healthy_now();
        }
        self.done
    }

    /// Mark healthy immediately (e.g. after the first frame was presented).
    pub fn mark_healthy_now(&mut self) {
        match self.layout.mark_healthy() {
            Ok(()) => self.done = true,
            Err(e) => tracing::warn!("could not mark version healthy: {e}"),
        }
    }

    pub fn is_healthy(&self) -> bool {
        self.done
    }
}

/// Write via temp file + rename so readers never see a torn file.
pub(crate) fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_file_name(format!(
        ".{}.tmp-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        std::process::id()
    ));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(f) = fs::File::open(dir) {
        let _ = f.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_version(l: &InstallLayout, v: &str) {
        let bin = l.version_dir(v).join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("frameplayer"), v).unwrap();
    }

    fn policy() -> HealthPolicy {
        HealthPolicy {
            max_attempts: 2,
            healthy_after: Duration::from_millis(0),
        }
    }

    #[test]
    fn first_activation_has_no_trial() {
        let d = tempfile::tempdir().unwrap();
        let l = InstallLayout::new(d.path());
        assert_eq!(
            l.boot_check_with(&policy(), false).unwrap(),
            BootOutcome::NotInstalled
        );
        fake_version(&l, "0.1.0");
        l.activate("0.1.0", &policy()).unwrap();
        assert_eq!(l.current_version().as_deref(), Some("0.1.0"));
        assert!(l.previous_version().is_none());
        assert!(l.trial().is_none());
        // `current` resolves through the symlink.
        assert_eq!(
            fs::read_to_string(d.path().join("current/bin/frameplayer")).unwrap(),
            "0.1.0"
        );
        assert_eq!(
            l.boot_check_with(&policy(), false).unwrap(),
            BootOutcome::Healthy {
                version: "0.1.0".into()
            }
        );
    }

    #[test]
    fn healthy_upgrade_clears_trial() {
        let d = tempfile::tempdir().unwrap();
        let l = InstallLayout::new(d.path());
        fake_version(&l, "0.1.0");
        fake_version(&l, "0.2.0");
        l.activate("0.1.0", &policy()).unwrap();
        l.activate("0.2.0", &policy()).unwrap();
        assert_eq!(l.previous_version().as_deref(), Some("0.1.0"));
        assert_eq!(l.trial().unwrap().version, "0.2.0");
        assert_eq!(
            l.boot_check_with(&policy(), false).unwrap(),
            BootOutcome::Trial {
                version: "0.2.0".into(),
                attempt: 1
            }
        );
        let mut g = l.launch_guard(&policy());
        assert!(g.poll());
        assert!(l.trial().is_none());
        assert_eq!(
            l.boot_check_with(&policy(), false).unwrap(),
            BootOutcome::Healthy {
                version: "0.2.0".into()
            }
        );
    }

    #[test]
    fn unhealthy_version_rolls_back_and_is_blocked() {
        let d = tempfile::tempdir().unwrap();
        let l = InstallLayout::new(d.path());
        fake_version(&l, "0.1.0");
        fake_version(&l, "0.2.0");
        l.activate("0.1.0", &policy()).unwrap();
        l.activate("0.2.0", &policy()).unwrap();
        for attempt in 1..=2 {
            assert_eq!(
                l.boot_check_with(&policy(), false).unwrap(),
                BootOutcome::Trial {
                    version: "0.2.0".into(),
                    attempt
                }
            );
        }
        assert_eq!(
            l.boot_check_with(&policy(), false).unwrap(),
            BootOutcome::RolledBack {
                from: "0.2.0".into(),
                to: "0.1.0".into()
            }
        );
        assert_eq!(l.current_version().as_deref(), Some("0.1.0"));
        assert!(l.is_blocked("0.2.0"));
        assert!(l.trial().is_none());
        l.unblock("0.2.0").unwrap();
        assert!(!l.is_blocked("0.2.0"));
    }

    #[test]
    fn launcher_counted_launches_are_not_double_counted() {
        let d = tempfile::tempdir().unwrap();
        let l = InstallLayout::new(d.path());
        fake_version(&l, "1.0.0");
        fake_version(&l, "1.1.0");
        l.activate("1.0.0", &policy()).unwrap();
        l.activate("1.1.0", &policy()).unwrap();
        // Launcher wrote attempts=1.
        fs::write(d.path().join(TRIAL_FILE), "1.1.0 1 2\n").unwrap();
        assert_eq!(
            l.boot_check_with(&policy(), true).unwrap(),
            BootOutcome::Trial {
                version: "1.1.0".into(),
                attempt: 1
            }
        );
        assert_eq!(l.trial().unwrap().attempts, 1);
    }

    #[test]
    fn stale_trial_discarded() {
        let d = tempfile::tempdir().unwrap();
        let l = InstallLayout::new(d.path());
        fake_version(&l, "1.0.0");
        l.activate("1.0.0", &policy()).unwrap();
        fs::write(d.path().join(TRIAL_FILE), "9.9.9 1 3\n").unwrap();
        assert_eq!(
            l.boot_check_with(&policy(), false).unwrap(),
            BootOutcome::Healthy {
                version: "1.0.0".into()
            }
        );
        assert!(l.trial().is_none());
    }

    #[test]
    fn install_tree_moves_files_and_updates_launcher() {
        let d = tempfile::tempdir().unwrap();
        let l = InstallLayout::new(d.path().join("root"));
        fs::create_dir_all(l.root()).unwrap();
        fake_version(&l, "0.1.0");
        l.activate("0.1.0", &policy()).unwrap();

        let tree = l.staging_dir().join("tree");
        crate::archive::extract_tar(
            crate::archive::tests::sample_tar("0.2.0").as_slice(),
            &tree,
            &Default::default(),
        )
        .unwrap();
        l.install_tree(&tree, "0.2.0", &policy()).unwrap();
        assert_eq!(l.current_version().as_deref(), Some("0.2.0"));
        assert_eq!(l.previous_version().as_deref(), Some("0.1.0"));
        assert!(l.launcher().is_file());
        assert_eq!(
            fs::read_to_string(l.root().join(RELEASE_FILE))
                .unwrap()
                .trim(),
            "0.2.0"
        );
        assert!(
            l.install_tree(&tree, "0.2.0", &policy()).is_err(),
            "running version"
        );
        assert!(l.install_tree(&tree, "../x", &policy()).is_err());
    }

    #[test]
    fn adopts_flat_external_install() {
        let d = tempfile::tempdir().unwrap();
        let l = InstallLayout::new(d.path());
        // First flat install (Frame Control): RELEASE + versions/, no links.
        crate::archive::extract_tar(
            crate::archive::tests::sample_tar("0.1.0").as_slice(),
            d.path(),
            &Default::default(),
        )
        .unwrap();
        assert!(matches!(
            l.boot_check_with(&policy(), false).unwrap(),
            BootOutcome::Healthy { .. }
        ));
        assert_eq!(l.current_version().as_deref(), Some("0.1.0"));
        // A newer flat install on top goes on trial.
        crate::archive::extract_tar(
            crate::archive::tests::sample_tar("0.2.0").as_slice(),
            d.path(),
            &Default::default(),
        )
        .unwrap();
        assert_eq!(
            l.boot_check_with(&policy(), false).unwrap(),
            BootOutcome::Trial {
                version: "0.2.0".into(),
                attempt: 1
            }
        );
        assert_eq!(l.previous_version().as_deref(), Some("0.1.0"));
    }

    #[test]
    fn gc_keeps_current_previous_and_trial() {
        let d = tempfile::tempdir().unwrap();
        let l = InstallLayout::new(d.path());
        for v in ["0.1.0", "0.2.0", "0.3.0", "0.10.0"] {
            fake_version(&l, v);
        }
        assert_eq!(
            l.installed_versions(),
            ["0.1.0", "0.2.0", "0.3.0", "0.10.0"]
        );
        l.activate("0.2.0", &policy()).unwrap();
        l.activate("0.3.0", &policy()).unwrap();
        let mut removed = l.gc().unwrap();
        removed.sort();
        assert_eq!(removed, ["0.1.0", "0.10.0"]);
        assert_eq!(l.installed_versions(), ["0.2.0", "0.3.0"]);
    }

    #[test]
    fn rollback_without_previous_is_noop() {
        let d = tempfile::tempdir().unwrap();
        let l = InstallLayout::new(d.path());
        fake_version(&l, "0.1.0");
        l.activate("0.1.0", &policy()).unwrap();
        assert_eq!(l.rollback().unwrap(), None);
        assert!(l.activate("0.9.0", &policy()).is_err(), "not installed");
    }

    #[test]
    fn version_validation() {
        assert!(validate_version("1.2.3-beta.1+build5").is_ok());
        for bad in ["", "..", "1.2", "1.2.3/..", "a b"] {
            assert!(validate_version(bad).is_err(), "{bad}");
        }
    }
}
