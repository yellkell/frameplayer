//! Shell scripts executed on the headset (sent to `sh -s` over SSH).
//!
//! Everything installs under `~/devkit-game/frameplayer`, the directory
//! Valve's devkit flow and the community installers use. The tarball is laid
//! out so a flat extraction is a valid install (see `fp_updater::layout`); the
//! bundled `frameplayer.sh` launcher adopts the new version on first start.

use crate::ssh::shell_quote;
use std::collections::BTreeMap;

/// `$HOME`-relative directory used for uploads.
pub const UPLOAD_DIR: &str = "devkit-game";
/// Where the launcher and app write logs on the headset.
pub const LOG_DIR: &str = "${XDG_DATA_HOME:-$HOME/.local/share}/frameplayer/logs";
/// Valve devkit client helper scripts, uploaded by the SteamOS Devkit Client.
// [verify] `~/devkit-utils/steam-client-create-shortcut --parms <json>` is how
// the SteamOS Devkit Client registers a "Devkit Game" on the Deck; check it
// exists on the Frame after pairing with Frame Control / FrameDrop / Valve's
// client, and the exact JSON keys it accepts.
pub const DEVKIT_UTILS: &str = "$HOME/devkit-utils";

fn root(game_dir: &str) -> String {
    format!("\"$HOME/devkit-game/\"{}", shell_quote(game_dir))
}

/// Ensure the upload directory exists.
pub fn prepare_upload_script() -> String {
    format!("set -eu\nmkdir -p \"$HOME/{UPLOAD_DIR}\"\n")
}

/// Extract an uploaded tarball (path relative to `$HOME`) into the game dir.
/// Prints `release=<RELEASE>` and `home=<$HOME>` on success.
// [verify] SteamOS ships GNU tar with gzip; `zstd` (only needed for .tar.zst
// uploads) is expected because pacman depends on it.
pub fn install_script(upload_rel: &str, game_dir: &str) -> String {
    let up = format!("\"$HOME/\"{}", shell_quote(upload_rel));
    let root = root(game_dir);
    format!(
        r#"set -eu
ROOT={root}
TARBALL={up}
mkdir -p "$ROOT"
case "$TARBALL" in
  *.zst) zstd -dc "$TARBALL" | tar -xf - -C "$ROOT" ;;
  *) tar -xzf "$TARBALL" -C "$ROOT" ;;
esac
rm -f "$TARBALL"
chmod +x "$ROOT/frameplayer.sh"
echo "release=$(head -n1 "$ROOT/RELEASE")"
echo "home=$HOME"
"#
    )
}

/// Register a Devkit Game shortcut through Valve's devkit-utils if present.
/// Prints `devkit-utils: ok` or `devkit-utils: missing`.
pub fn devkit_shortcut_script(game_dir: &str, display_name: &str) -> String {
    let params = serde_json::json!({
        "gameid": game_dir,
        "directory": format!("__HOME__/devkit-game/{game_dir}"),
        "argv": ["./frameplayer.sh"],
        "settings": { "steam_play": "0" },
        "name": display_name,
    })
    .to_string();
    let params = shell_quote(&params);
    format!(
        r#"set -eu
U="{DEVKIT_UTILS}/steam-client-create-shortcut"
if [ -f "$U" ]; then
  P=$(printf '%s' {params} | sed "s#__HOME__#$HOME#g")
  python3 "$U" --parms "$P" >/dev/null
  echo "devkit-utils: ok"
else
  echo "devkit-utils: missing"
fi
"#
    )
}

/// Remove the install (and with `purge`, user data/config too).
pub fn uninstall_script(game_dir: &str, purge: bool) -> String {
    let root = root(game_dir);
    let mut s = format!(
        r#"set -eu
ROOT={root}
pkill -f "$ROOT/" 2>/dev/null || true
if [ -f "{DEVKIT_UTILS}/steamos-delete" ]; then
  python3 "{DEVKIT_UTILS}/steamos-delete" --delete-title {gd} >/dev/null 2>&1 || true
fi
rm -rf "$ROOT"
"#,
        gd = shell_quote(game_dir)
    );
    if purge {
        s.push_str(
            "rm -rf \"${XDG_DATA_HOME:-$HOME/.local/share}/frameplayer\" \"${XDG_CONFIG_HOME:-$HOME/.config}/frameplayer\" \"${XDG_CACHE_HOME:-$HOME/.cache}/frameplayer\"\n",
        );
    }
    s.push_str("echo removed\n");
    s
}

/// Print `key=value` lines describing the install; see [`RemoteStatus`].
pub fn status_script(game_dir: &str) -> String {
    let root = root(game_dir);
    format!(
        r#"ROOT={root}
rd() {{ [ -f "$1" ] && head -n1 "$1" | tr -d '\r\n' || true; }}
lv() {{ [ -L "$1" ] && basename "$(readlink "$1")" || true; }}
echo "installed=$([ -x "$ROOT/frameplayer.sh" ] && echo 1 || echo 0)"
echo "release=$(rd "$ROOT/RELEASE")"
echo "current=$(lv "$ROOT/current")"
echo "previous=$(lv "$ROOT/previous")"
echo "trial=$(rd "$ROOT/trial")"
echo "blocked=$( [ -f "$ROOT/blocked" ] && tr '\n' ',' < "$ROOT/blocked" | sed 's/,$//' || true)"
echo "running=$(pgrep -f "$ROOT/.*bin/frameplayer" 2>/dev/null | head -n1 || true)"
echo "os_version=$(. /etc/os-release 2>/dev/null; echo "${{VERSION_ID:-}}")"
echo "os_build=$(. /etc/os-release 2>/dev/null; echo "${{BUILD_ID:-}}")"
echo "devkit_utils=$([ -d "{DEVKIT_UTILS}" ] && echo 1 || echo 0)"
echo "free_kb=$(df -Pk "$HOME" 2>/dev/null | awk 'NR==2 {{print $4}}')"
"#
    )
}

/// Tail the launcher/app logs.
pub fn logs_script(lines: u32, follow: bool) -> String {
    let follow = if follow { " -F" } else { "" };
    format!(
        r#"D="{LOG_DIR}"
if ! ls "$D"/*.log >/dev/null 2>&1; then echo "no logs yet in $D (has FramePlayer been launched?)" >&2; exit 1; fi
exec tail -n {lines}{follow} "$D"/*.log
"#
    )
}

/// Parsed output of [`status_script`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteStatus {
    pub installed: bool,
    pub release: Option<String>,
    pub current: Option<String>,
    pub previous: Option<String>,
    pub trial: Option<String>,
    pub blocked: Vec<String>,
    pub running_pid: Option<u32>,
    pub os_version: Option<String>,
    pub os_build: Option<String>,
    pub devkit_utils: bool,
    pub free_kb: Option<u64>,
}

pub fn parse_status(out: &str) -> RemoteStatus {
    let kv: BTreeMap<&str, &str> = out
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim(), v.trim()))
        .collect();
    let opt = |k: &str| kv.get(k).filter(|v| !v.is_empty()).map(|v| v.to_string());
    RemoteStatus {
        installed: kv.get("installed") == Some(&"1"),
        release: opt("release"),
        current: opt("current"),
        previous: opt("previous"),
        trial: opt("trial"),
        blocked: opt("blocked")
            .map(|b| {
                b.split(',')
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default(),
        running_pid: opt("running").and_then(|p| p.parse().ok()),
        os_version: opt("os_version"),
        os_build: opt("os_build"),
        devkit_utils: kv.get("devkit_utils") == Some(&"1"),
        free_kb: opt("free_kb").and_then(|p| p.parse().ok()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_script_quotes_paths() {
        let s = install_script("devkit-game/.frameplayer-upload.tar.gz", "frameplayer");
        assert!(s.contains(r#"ROOT="$HOME/devkit-game/"frameplayer"#));
        assert!(s.contains(r#"TARBALL="$HOME/"devkit-game/.frameplayer-upload.tar.gz"#));
        assert!(s.contains("tar -xzf"));
        let evil = install_script("x'; rm -rf ~; '.tar.gz", "frameplayer");
        assert!(evil.contains(r#"'x'\''; rm -rf ~; '\''.tar.gz'"#));
    }

    #[test]
    fn uninstall_purge_option() {
        assert!(!uninstall_script("frameplayer", false).contains(".config"));
        assert!(uninstall_script("frameplayer", true).contains(".config}/frameplayer"));
    }

    #[test]
    fn shortcut_params_are_json() {
        let s = devkit_shortcut_script("frameplayer", "FramePlayer");
        let start = s.find("printf '%s' '").unwrap() + "printf '%s' '".len();
        let end = s[start..].find("' |").unwrap() + start;
        let v: serde_json::Value = serde_json::from_str(&s[start..end]).unwrap();
        assert_eq!(v["gameid"], "frameplayer");
        assert_eq!(v["argv"][0], "./frameplayer.sh");
    }

    #[test]
    fn logs_follow_flag() {
        assert!(logs_script(50, true).contains("tail -n 50 -F"));
        assert!(!logs_script(50, false).contains("-F"));
    }

    #[test]
    fn parses_status() {
        let s = parse_status(
            "installed=1\nrelease=0.2.0\ncurrent=0.2.0\nprevious=0.1.0\ntrial=0.2.0 1 3\nblocked=0.1.5,0.1.6\nrunning=4242\nos_version=3.8.0\nos_build=20260901.1\ndevkit_utils=0\nfree_kb=123456\n",
        );
        assert!(s.installed);
        assert_eq!(s.current.as_deref(), Some("0.2.0"));
        assert_eq!(s.trial.as_deref(), Some("0.2.0 1 3"));
        assert_eq!(s.blocked, ["0.1.5", "0.1.6"]);
        assert_eq!(s.running_pid, Some(4242));
        assert_eq!(s.free_kb, Some(123456));
        assert!(!s.devkit_utils);
        let empty = parse_status("installed=0\nrelease=\nrunning=\nblocked=\n");
        assert_eq!(empty, RemoteStatus::default());
    }
}
