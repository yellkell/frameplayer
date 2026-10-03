//! Learning about the headset in one round trip: architecture, OS, home
//! directory, available tools, free space and Steam accounts.

use crate::error::{InstallError, Result};
use crate::remote::{Remote, check_remote_path, run_checked};

/// Steam's per-user data, relative to the home directory.
pub const STEAM_USERDATA: &str = ".local/share/Steam/userdata";
/// Valve's devkit helper that registers a shortcut, relative to home.
pub const DEVKIT_SHORTCUT_TOOL: &str = "devkit-utils/steam-client-create-shortcut";

/// The probe script. Prints `key=value` lines and nothing else.
pub const PROBE_SCRIPT: &str = r#"
echo "arch=$(uname -m)"
echo "home=$HOME"
( . /etc/os-release 2>/dev/null; echo "os_id=${ID:-}"; echo "os_name=${PRETTY_NAME:-${NAME:-}}" )
command -v unzip >/dev/null 2>&1 && echo "unzip=1"
command -v python3 >/dev/null 2>&1 && echo "python3=1"
command -v sha256sum >/dev/null 2>&1 && echo "sha256sum=1"
[ -x "$HOME/devkit-utils/steam-client-create-shortcut" ] && echo "devkit_shortcut=1"
df -Pk "$HOME" 2>/dev/null | awk 'NR==2 {print "free_kb=" $4}'
for d in "$HOME/.local/share/Steam/userdata"/*; do
  n=$(basename "$d")
  case "$n" in ''|0|*[!0-9]*) continue ;; esac
  [ -d "$d" ] || continue
  echo "steam_user=$n"
  [ -f "$d/config/shortcuts.vdf" ] && echo "steam_vdf=$n"
done
pgrep -x steam >/dev/null 2>&1 && echo "steam_running=1"
exit 0
"#;

/// What [`probe`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceInfo {
    /// `uname -m`, e.g. `aarch64`.
    pub arch: String,
    /// Home directory of the ssh user.
    pub home: String,
    /// `/etc/os-release` `ID`, e.g. `steamos`.
    pub os_id: String,
    /// `/etc/os-release` `PRETTY_NAME`.
    pub os_name: String,
    /// `unzip` is available.
    pub has_unzip: bool,
    /// `python3` is available.
    pub has_python3: bool,
    /// `sha256sum` is available.
    pub has_sha256sum: bool,
    /// `~/devkit-utils/steam-client-create-shortcut` is executable.
    pub has_devkit_shortcut: bool,
    /// Free space in the home file system, in bytes.
    pub free_bytes: Option<u64>,
    /// Numeric Steam account folders under `userdata/`.
    pub steam_users: Vec<String>,
    /// Accounts that already have a `shortcuts.vdf`.
    pub steam_users_with_shortcuts: Vec<String>,
    /// A `steam` process is running.
    pub steam_running: bool,
}

impl DeviceInfo {
    /// Parses the probe's `key=value` output. Unknown keys are ignored.
    pub fn parse(stdout: &str) -> DeviceInfo {
        let mut d = DeviceInfo::default();
        for line in stdout.lines() {
            let Some((k, v)) = line.trim_end_matches('\r').split_once('=') else {
                continue;
            };
            let v = v.trim();
            match k.trim() {
                "arch" => d.arch = v.into(),
                "home" => d.home = v.trim_end_matches('/').into(),
                "os_id" => d.os_id = v.into(),
                "os_name" => d.os_name = v.into(),
                "unzip" => d.has_unzip = v == "1",
                "python3" => d.has_python3 = v == "1",
                "sha256sum" => d.has_sha256sum = v == "1",
                "devkit_shortcut" => d.has_devkit_shortcut = v == "1",
                "free_kb" => d.free_bytes = v.parse::<u64>().ok().map(|kb| kb * 1024),
                "steam_user" => d.steam_users.push(v.into()),
                "steam_vdf" => d.steam_users_with_shortcuts.push(v.into()),
                "steam_running" => d.steam_running = v == "1",
                _ => {}
            }
        }
        d
    }

    /// Checks this is an ARM64 Linux device with a usable home directory.
    /// With `force`, only the home directory is required.
    pub fn check(&self, force: bool) -> Result<()> {
        check_remote_path(&self.home).map_err(|_| {
            InstallError::WrongDevice(format!(
                "home directory {:?} is missing or has unusual characters",
                self.home
            ))
        })?;
        if self.home == "/" {
            return Err(InstallError::WrongDevice(
                "the ssh user's home directory is /".into(),
            ));
        }
        if !force && self.arch != "aarch64" {
            return Err(InstallError::WrongDevice(format!(
                "its CPU is {:?}; FramePlayer needs aarch64 (ARM64)",
                self.arch
            )));
        }
        Ok(())
    }

    /// True when the OS identifies as SteamOS (the Frame runs SteamOS).
    pub fn is_steamos(&self) -> bool {
        self.os_id.eq_ignore_ascii_case("steamos")
    }

    /// Absolute path of `rel` under the home directory.
    pub fn home_path(&self, rel: &str) -> String {
        format!("{}/{}", self.home, rel.trim_start_matches('/'))
    }

    /// `userdata/<user>/config` for a Steam account.
    pub fn steam_config_dir(&self, user: &str) -> String {
        self.home_path(&format!("{STEAM_USERDATA}/{user}/config"))
    }
}

/// Runs [`PROBE_SCRIPT`] and parses the answer.
pub fn probe(remote: &mut dyn Remote) -> Result<DeviceInfo> {
    let out = run_checked(remote, "checking the headset", PROBE_SCRIPT)?;
    Ok(DeviceInfo::parse(&out.stdout))
}

/// Resolves the user's `--dir` against the remote home directory.
///
/// `frameplayer`, `~/frameplayer` and `/home/<user>/frameplayer` all mean
/// the same thing. The result must lie strictly inside the home directory:
/// the installer replaces and the uninstaller deletes this directory, so it
/// must never be home itself or anything above it.
pub fn resolve_install_dir(home: &str, dir: &str) -> Result<String> {
    let rel = dir.trim();
    let abs = if let Some(r) = rel.strip_prefix("~/") {
        format!("{home}/{r}")
    } else if rel.starts_with('/') {
        rel.to_string()
    } else {
        format!("{home}/{rel}")
    };
    let abs = abs.trim_end_matches('/').to_string();
    check_remote_path(&abs)?;
    let inside = abs
        .strip_prefix(home)
        .is_some_and(|r| r.starts_with('/') && r.len() > 1);
    if !inside {
        return Err(InstallError::BadPath(format!(
            "install folder {abs} must be inside the home directory {home}"
        )));
    }
    if abs.contains("//") {
        return Err(InstallError::BadPath(format!(
            "{abs} has an empty component"
        )));
    }
    Ok(abs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::tests::FakeRemote;

    const SAMPLE: &str = "arch=aarch64\nhome=/home/steam\nos_id=steamos\n\
        os_name=SteamOS\nunzip=1\npython3=1\nsha256sum=1\nfree_kb=1000\n\
        steam_user=12345\nsteam_vdf=12345\nsteam_user=678\nsteam_running=1\nnoise\n";

    #[test]
    fn parses_probe_output() {
        let d = DeviceInfo::parse(SAMPLE);
        assert_eq!(d.arch, "aarch64");
        assert_eq!(d.home, "/home/steam");
        assert!(d.is_steamos());
        assert!(d.has_unzip && d.has_python3 && d.has_sha256sum);
        assert!(!d.has_devkit_shortcut);
        assert_eq!(d.free_bytes, Some(1_024_000));
        assert_eq!(d.steam_users, ["12345", "678"]);
        assert_eq!(d.steam_users_with_shortcuts, ["12345"]);
        assert!(d.steam_running);
        assert!(d.check(false).is_ok());
        assert_eq!(
            d.steam_config_dir("12345"),
            "/home/steam/.local/share/Steam/userdata/12345/config"
        );
        // Windows-style line endings from odd shells are tolerated.
        let d = DeviceInfo::parse("arch=aarch64\r\nhome=/home/deck/\r\n");
        assert_eq!(d.home, "/home/deck");
    }

    #[test]
    fn device_checks() {
        let mut d = DeviceInfo::parse(SAMPLE);
        d.arch = "x86_64".into();
        assert!(matches!(d.check(false), Err(InstallError::WrongDevice(_))));
        assert!(d.check(true).is_ok());
        d.home = String::new();
        assert!(d.check(true).is_err());
        d.home = "/".into();
        assert!(d.check(true).is_err());
    }

    #[test]
    fn probe_runs_script() {
        let mut f = FakeRemote::default();
        f.on("uname -m", SAMPLE);
        let d = probe(&mut f).unwrap();
        assert_eq!(d.home, "/home/steam");
        assert_eq!(f.scripts.len(), 1);
    }

    #[test]
    fn install_dir_resolution() {
        let h = "/home/steam";
        for (input, want) in [
            ("frameplayer", "/home/steam/frameplayer"),
            ("~/frameplayer", "/home/steam/frameplayer"),
            ("/home/steam/frameplayer/", "/home/steam/frameplayer"),
            (
                "devkit-game/frameplayer",
                "/home/steam/devkit-game/frameplayer",
            ),
        ] {
            assert_eq!(resolve_install_dir(h, input).unwrap(), want, "{input}");
        }
        for bad in [
            "",
            "~",
            "~/",
            "/",
            "/home/steam",
            "/home",
            "/opt/frameplayer",
            "/home/steamy/frameplayer",
            "../frameplayer",
            "a/../../x",
            "my apps/fp",
            "a//b",
        ] {
            assert!(resolve_install_dir(h, bad).is_err(), "{bad:?}");
        }
    }
}
