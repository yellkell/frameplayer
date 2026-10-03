//! Opening web pages (WebXR games and experiences) in the Frame's WebXR
//! browser, Chromium XR, which takes over the headset.
//!
//! The browser is started as its own Steam library entry (Chromium XR, set
//! up by its installer): Steam only shows an app's windows as a panel in the
//! headset when it launched that app, so a browser started by FramePlayer
//! itself ran invisibly. FramePlayer writes the page to [`home_url_path`],
//! which the browser's launcher opens, asks Steam to start the entry and
//! quits. Without a Steam entry it falls back to the hand-off: it writes the
//! browser command to [`handoff_path`] and exits with [`HANDOFF_EXIT_CODE`];
//! `frameplayer.sh` runs the browser and starts FramePlayer again after.
//!
//! The browser is FramePlayer's WebXR Chromium build for the Steam Frame
//! (tools/webxr, released as chromium-xr-frame-* with FramePlayer), installed
//! in `~/chromium-xr-frame`. saphid's build
//! (github.com/saphid/chromium-webxr-steam-frame, `~/.local/bin/chromium-xr`)
//! also works, without the Frame rendering and controller fixes.

use std::path::{Path, PathBuf};

/// Exit code telling `frameplayer.sh` to run the hand-off command.
pub const HANDOFF_EXIT_CODE: i32 = 75;
/// Set by `frameplayer.sh`, so FramePlayer knows it will be restarted.
pub const LAUNCHER_ENV: &str = "FRAMEPLAYER_LAUNCHER";
pub const BROWSER_PROJECT: &str = "https://github.com/yellkell/frameplayer/releases";
/// Where FramePlayer's Chromium XR build is installed, under `$HOME`.
pub const FRAME_BROWSER: &str = "chromium-xr-frame/chromium-xr.sh";

/// The page the Web XR tab opens unless the user types another: Fish & Chips.
pub const DEFAULT_URL: &str = "https://yellkell.com/fac";
/// Under `$HOME`: the page Chromium XR's launcher opens when Steam starts it
/// (`tools/webxr/frame-title/launch.sh`).
pub const HOME_URL_FILE: &str = ".config/chromium-xr-frame/home-url";
/// Under `$HOME`: Chromium XR's Steam shortcut app id, written by its
/// installer (saphid's steam-shortcut.py).
pub const STEAM_APPID_FILE: &str = ".local/share/chromium-xr-frame/steam-appid";

/// The `steam://rungameid/` id of a non-Steam shortcut with this app id.
pub fn shortcut_game_id(appid: u32) -> u64 {
    (u64::from(appid) << 32) | 0x0200_0000
}

/// Chromium XR's Steam shortcut app id, if its installer recorded one.
pub fn steam_appid(home: &Path) -> Option<u32> {
    std::fs::read_to_string(home.join(STEAM_APPID_FILE))
        .ok()?
        .trim()
        .parse()
        .ok()
}

pub fn home_url_path(home: &Path) -> PathBuf {
    home.join(HOME_URL_FILE)
}

/// Records the page for the browser's launcher to open.
pub fn write_home_url(home: &Path, url: &str) -> std::io::Result<()> {
    let path = home_url_path(home);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, format!("{url}\n"))
}

/// Asks Steam to start Chromium XR's library entry once FramePlayer (also a
/// Steam app) has gone: a shell that waits two seconds, so Steam isn't asked
/// while FramePlayer is still running.
pub fn launch_via_steam(appid: u32) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    let url = format!("steam://rungameid/{}", shortcut_game_id(appid));
    std::process::Command::new("sh")
        .args(["-c", "sleep 2; exec steam -ifrunning \"$0\"", &url])
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
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
    // A normal window, with tabs and an address bar to go elsewhere.
    argv.push(url.to_string());
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
    fn steam_entry_and_home_page() {
        // Matches steam://rungameid for shortcut 3269230940 on the Frame.
        assert_eq!(shortcut_game_id(3_269_230_940), 14_041_239_970_404_892_672);
        let home = std::env::temp_dir().join(format!("fp-webhome-{}", std::process::id()));
        assert_eq!(steam_appid(&home), None);
        std::fs::create_dir_all(home.join(".local/share/chromium-xr-frame")).unwrap();
        std::fs::write(home.join(STEAM_APPID_FILE), "3269230940\n").unwrap();
        assert_eq!(steam_appid(&home), Some(3_269_230_940));
        write_home_url(&home, DEFAULT_URL).unwrap();
        assert_eq!(
            std::fs::read_to_string(home_url_path(&home)).unwrap(),
            "https://yellkell.com/fac\n"
        );
        std::fs::remove_dir_all(&home).unwrap();
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
            vec!["/home/deck/.local/bin/chromium-xr", "https://yellkell.com/fac"]
        );
        let c = command(
            &Browser::Chrome("/home/deck/chromium-xr/chrome".into()),
            "https://x.y/",
            home,
        );
        assert!(c.contains(&"--enable-features=OpenXR".to_string()));
        assert!(c.contains(&"--disable-seccomp-filter-sandbox".to_string()));
        assert_eq!(c.last().unwrap(), "https://x.y/");

        let path = std::env::temp_dir().join(format!("fp-handoff-{}", std::process::id()));
        write_handoff(&path, &l).unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"/home/deck/.local/bin/chromium-xr\0https://yellkell.com/fac\0"
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_file(&path).unwrap();
    }
}
