//! Launching WebXR pages (web games and experiences) in a WebXR-capable
//! browser on the Frame, handing the headset over and coming back after.
//!
//! Only one app can drive the headset, so FramePlayer exits its OpenXR
//! session first: it writes the browser command to [`handoff_path`] and
//! exits with [`HANDOFF_EXIT_CODE`]; `frameplayer.sh` runs the browser,
//! waits for it to close and starts FramePlayer again. Started without the
//! launcher, FramePlayer starts the browser itself and quits.
//!
//! The browser is FramePlayer's WebXR Chromium build for the Steam Frame
//! (tools/webxr, released as chromium-xr-frame-* with FramePlayer), installed
//! in `~/chromium-xr-frame`. saphid's build
//! (github.com/saphid/chromium-webxr-steam-frame, `~/.local/bin/chromium-xr`)
//! also works, without the Frame rendering and controller fixes.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Exit code telling `frameplayer.sh` to run the hand-off command.
pub const HANDOFF_EXIT_CODE: i32 = 75;
/// Set by `frameplayer.sh`, so FramePlayer knows it will be restarted.
pub const LAUNCHER_ENV: &str = "FRAMEPLAYER_LAUNCHER";
pub const BROWSER_PROJECT: &str = "https://github.com/yellkell/frameplayer/releases";
/// Where FramePlayer's Chromium XR build is installed, under `$HOME`.
pub const FRAME_BROWSER: &str = "chromium-xr-frame/chromium-xr.sh";

/// A web page with WebXR content, as listed in the Web XR tab.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WebApp {
    pub name: String,
    pub url: String,
}

pub fn default_apps() -> Vec<WebApp> {
    vec![WebApp {
        name: "Fish & Chips".into(),
        url: "https://yellkell.com/fac".into(),
    }]
}

/// Fixes entries saved by earlier versions: the default app was listed as
/// "Factory Fight".
pub fn migrate_apps(apps: &mut [WebApp]) {
    for app in apps {
        if app.name == "Factory Fight" && app.url == "https://yellkell.com/fac" {
            app.name = "Fish & Chips".into();
        }
    }
}

/// A WebXR-capable browser found on this device.
#[derive(Clone, Debug, PartialEq)]
pub enum Browser {
    /// A launcher script that adds the Frame-specific flags itself
    /// (`chromium-xr`, or `$FP_WEBXR_BROWSER`).
    Launcher(PathBuf),
    /// The bare Chromium XR binary; FramePlayer passes the flags.
    Chrome(PathBuf),
}

impl Browser {
    pub fn path(&self) -> &Path {
        match self {
            Browser::Launcher(p) | Browser::Chrome(p) => p,
        }
    }
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Looks for a WebXR browser: `$FP_WEBXR_BROWSER`, FramePlayer's build in
/// `~/chromium-xr-frame`, `~/.local/bin/chromium-xr`, `chromium-xr` on
/// `PATH`, then the bare build in `$CHROMIUM_XR_HOME` or `~/chromium-xr`.
pub fn find_browser() -> Option<Browser> {
    find_browser_in(
        std::env::var_os("HOME").map(PathBuf::from).as_deref(),
        std::env::var_os("FP_WEBXR_BROWSER").map(PathBuf::from),
        std::env::var_os("CHROMIUM_XR_HOME").map(PathBuf::from),
        std::env::var_os("PATH"),
    )
}

fn find_browser_in(
    home: Option<&Path>,
    explicit: Option<PathBuf>,
    chromium_home: Option<PathBuf>,
    path: Option<std::ffi::OsString>,
) -> Option<Browser> {
    if let Some(p) = explicit.filter(|p| is_executable(p)) {
        return Some(Browser::Launcher(p));
    }
    if let Some(p) = home
        .map(|h| h.join(FRAME_BROWSER))
        .filter(|p| is_executable(p))
    {
        return Some(Browser::Launcher(p));
    }
    if let Some(p) = home
        .map(|h| h.join(".local/bin/chromium-xr"))
        .filter(|p| is_executable(p))
    {
        return Some(Browser::Launcher(p));
    }
    if let Some(p) = path
        .iter()
        .flat_map(std::env::split_paths)
        .map(|d| d.join("chromium-xr"))
        .find(|p| is_executable(p))
    {
        return Some(Browser::Launcher(p));
    }
    chromium_home
        .or_else(|| home.map(|h| h.join("chromium-xr")))
        .map(|d| d.join("chrome"))
        .filter(|p| is_executable(p))
        .map(Browser::Chrome)
}

/// Checks and normalises a page address: http(s) only.
pub fn normalize_url(url: &str) -> Result<String, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("Enter the page address".into());
    }
    if !url.contains("://") {
        // "scheme:..." without "//" (javascript:, data:, mailto:) is not a
        // host name; "host:port/..." is.
        if let Some((scheme, rest)) = url.split_once(':') {
            let port = rest.split(['/', '?', '#']).next().unwrap_or("");
            if !scheme.contains(['/', '.'])
                && (port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()))
            {
                return Err("Only http:// and https:// pages can be opened".into());
            }
        }
    }
    let url = if url.contains("://") {
        url.to_string()
    } else {
        format!("https://{url}")
    };
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or("Only http:// and https:// pages can be opened")?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if host.is_empty() || host.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("That address has no valid host".into());
    }
    Ok(url)
}

/// The command line that opens `url` (already normalised) in `browser`.
pub fn command(browser: &Browser, url: &str, home: &Path) -> Vec<String> {
    let mut argv = vec![browser.path().display().to_string()];
    if let Browser::Chrome(_) = browser {
        // The same flags saphid's chromium-xr launcher passes.
        argv.extend([
            format!(
                "--user-data-dir={}",
                home.join(".config/chromium-xr").display()
            ),
            "--enable-features=OpenXR".into(),
            "--ozone-platform=x11".into(),
            "--no-first-run".into(),
            "--no-default-browser-check".into(),
            "--password-store=basic".into(),
            // SteamVR refuses sessions from the XR process otherwise;
            // see docs/webxr/README.md for the proper fix.
            "--disable-seccomp-filter-sandbox".into(),
        ]);
    }
    // App mode: just the page, no tabs or address bar.
    argv.push(format!("--app={url}"));
    argv
}

pub fn handoff_path() -> PathBuf {
    fp_core::dirs::data_dir().join("handoff")
}

/// Writes the command for `frameplayer.sh`: arguments separated by NUL,
/// readable only by the user.
pub fn write_handoff(path: &Path, argv: &[String]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    for a in argv {
        f.write_all(a.as_bytes())?;
        f.write_all(&[0])?;
    }
    f.sync_all()
}

/// Starts the browser detached (when FramePlayer was not started by
/// `frameplayer.sh` and so will not be restarted).
pub fn spawn_detached(argv: &[String]) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]).process_group(0);
    // Steam's overlay library crashes Chromium's zygote.
    if let Ok(pre) = std::env::var("LD_PRELOAD") {
        let keep: Vec<&str> = pre
            .split([' ', ':'])
            .filter(|l| !l.is_empty() && !l.ends_with("gameoverlayrenderer.so"))
            .collect();
        if keep.is_empty() {
            cmd.env_remove("LD_PRELOAD");
        } else {
            cmd.env("LD_PRELOAD", keep.join(":"));
        }
    }
    cmd.stdin(std::process::Stdio::null()).spawn().map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(
            normalize_url("yellkell.com/fac").unwrap(),
            "https://yellkell.com/fac"
        );
        assert_eq!(
            normalize_url(" http://10.0.0.5:8080/game ").unwrap(),
            "http://10.0.0.5:8080/game"
        );
        assert!(normalize_url("file:///etc/passwd").is_err());
        assert!(normalize_url("javascript:alert(1)").is_err());
        assert!(normalize_url("data:text/html,hi").is_err());
        assert_eq!(
            normalize_url("localhost:8080/game").unwrap(),
            "https://localhost:8080/game"
        );
        assert!(normalize_url("https:///nohost").is_err());
        assert!(normalize_url("").is_err());
    }

    fn exe(p: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn finds_browsers_in_order() {
        let tmp = std::env::temp_dir().join(format!("fp-webxr-{}", std::process::id()));
        let home = tmp.join("home");
        assert_eq!(find_browser_in(Some(&home), None, None, None), None);
        exe(&home.join("chromium-xr/chrome"));
        assert_eq!(
            find_browser_in(Some(&home), None, None, None),
            Some(Browser::Chrome(home.join("chromium-xr/chrome")))
        );
        exe(&home.join(".local/bin/chromium-xr"));
        assert_eq!(
            find_browser_in(Some(&home), None, None, None),
            Some(Browser::Launcher(home.join(".local/bin/chromium-xr")))
        );
        exe(&home.join(FRAME_BROWSER));
        assert_eq!(
            find_browser_in(Some(&home), None, None, None),
            Some(Browser::Launcher(home.join(FRAME_BROWSER)))
        );
        exe(&tmp.join("other"));
        assert_eq!(
            find_browser_in(Some(&home), Some(tmp.join("other")), None, None),
            Some(Browser::Launcher(tmp.join("other")))
        );
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn renames_the_old_default_app() {
        let mut apps = vec![
            WebApp {
                name: "Factory Fight".into(),
                url: "https://yellkell.com/fac".into(),
            },
            WebApp {
                name: "Factory Fight".into(),
                url: "https://example.com/".into(),
            },
        ];
        migrate_apps(&mut apps);
        assert_eq!(apps[0].name, "Fish & Chips");
        assert_eq!(apps[1].name, "Factory Fight");
        assert_eq!(default_apps()[0].name, "Fish & Chips");
    }

    #[test]
    fn commands_and_handoff_file() {
        let home = Path::new("/home/deck");
        let l = command(
            &Browser::Launcher("/home/deck/.local/bin/chromium-xr".into()),
            "https://yellkell.com/fac",
            home,
        );
        assert_eq!(
            l,
            vec![
                "/home/deck/.local/bin/chromium-xr",
                "--app=https://yellkell.com/fac"
            ]
        );
        let c = command(
            &Browser::Chrome("/home/deck/chromium-xr/chrome".into()),
            "https://x.y/",
            home,
        );
        assert!(c.contains(&"--enable-features=OpenXR".to_string()));
        assert!(c.contains(&"--disable-seccomp-filter-sandbox".to_string()));
        assert_eq!(c.last().unwrap(), "--app=https://x.y/");

        let path = std::env::temp_dir().join(format!("fp-handoff-{}", std::process::id()));
        write_handoff(&path, &l).unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"/home/deck/.local/bin/chromium-xr\0--app=https://yellkell.com/fac\0"
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_file(&path).unwrap();
    }
}
