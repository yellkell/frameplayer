//! Developer-mode install environment, as the desktop installer and the
//! launcher script assume it: devkit service and Steam DevTools ports,
//! `~/devkit-game` / `~/devkit-utils`, GNU tools, and the sysfs/log paths
//! `tools/perf-capture.sh` reads. Answers I1, I4, I5, I6, I10, I14.

use crate::report::{CheckOutput, Status};
use crate::runner::CheckContext;
use crate::util::{self, Out};
use serde_json::json;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

pub fn run(_ctx: &CheckContext) -> CheckOutput {
    let mut o = Out::new();
    let home = util::home();

    // I1: devkit service on :32000 (local HTTP GET of /properties.json).
    let devkit = http_get_local(32000, "/properties.json");
    match &devkit {
        Some((code, body)) => {
            let keys: Vec<String> = serde_json::from_str::<serde_json::Value>(body)
                .ok()
                .and_then(|v| v.as_object().map(|m| m.keys().cloned().collect()))
                .unwrap_or_default();
            o.set(
                "devkit_service",
                json!({ "port": 32000, "http_status": code, "properties_keys": keys }),
            );
            o.finding(
                "devkit_service",
                Status::Pass,
                &["I1", "I2"],
                format!(
                    "devkit service answers on 127.0.0.1:32000 (HTTP {code}; keys: {})",
                    keys.join(",")
                ),
            );
        }
        None => {
            o.set(
                "devkit_service",
                json!({ "port": 32000, "listening": false }),
            );
            o.finding(
                "devkit_service",
                Status::Fail,
                &["I1"],
                "nothing answers HTTP on 127.0.0.1:32000 (Developer Mode off, or another port)",
            );
        }
    }

    // I6: Steam CEF DevTools.
    let cef = http_get_local(8080, "/json/version");
    o.set(
        "steam_devtools",
        json!({ "port": 8080, "http_status": cef.as_ref().map(|c| c.0) }),
    );
    o.finding(
        "steam_devtools",
        if cef.is_some() {
            Status::Pass
        } else {
            Status::Fail
        },
        &["I6"],
        if cef.is_some() {
            "Steam CEF DevTools answer on 127.0.0.1:8080".to_string()
        } else {
            "no Steam DevTools on 127.0.0.1:8080".to_string()
        },
    );

    // I4 / I5: devkit directories.
    let game = home.join("devkit-game");
    let games: Vec<String> = util::list_prefixed(&game.to_string_lossy(), "")
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    let utils_dir = home.join("devkit-utils");
    let utils: Vec<String> = util::list_prefixed(&utils_dir.to_string_lossy(), "")
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    o.set(
        "devkit_game",
        json!({ "exists": game.is_dir(), "entries": games }),
    );
    o.set(
        "devkit_utils",
        json!({ "exists": utils_dir.is_dir(), "entries": utils }),
    );
    o.finding(
        "devkit_game",
        if game.is_dir() {
            Status::Pass
        } else {
            Status::Fail
        },
        &["I4"],
        if game.is_dir() {
            format!("~/devkit-game exists ({} entries)", games.len())
        } else {
            "~/devkit-game does not exist".into()
        },
    );
    let shortcut = utils.iter().any(|u| u == "steam-client-create-shortcut");
    o.finding(
        "create_shortcut",
        if shortcut { Status::Pass } else { Status::Fail },
        &["I5"],
        if shortcut {
            "~/devkit-utils/steam-client-create-shortcut exists".to_string()
        } else if utils_dir.is_dir() {
            format!(
                "~/devkit-utils has no steam-client-create-shortcut ({})",
                utils.join(", ")
            )
        } else {
            "~/devkit-utils does not exist".to_string()
        },
    );

    // I10: tools the launcher / installer use.
    let mut tools = serde_json::Map::new();
    let mut missing = Vec::new();
    for t in ["mv", "sort", "tar", "gzip", "zstd", "sha256sum", "readlink"] {
        let v = util::which(t).map(|_| first_line_of(t));
        if v.is_none() {
            missing.push(t);
        }
        tools.insert(t.into(), json!(v));
    }
    let gnu = ["mv", "tar"]
        .iter()
        .all(|t| tools[*t].as_str().is_some_and(|v| v.contains("GNU")));
    o.set("tools", tools);
    o.finding(
        "gnu_tools",
        if gnu && missing.is_empty() {
            Status::Pass
        } else {
            Status::Fail
        },
        &["I10"],
        format!(
            "GNU coreutils/tar: {}; missing: {}",
            if gnu { "yes" } else { "no" },
            if missing.is_empty() {
                "none".to_string()
            } else {
                missing.join(", ")
            }
        ),
    );

    // I14: perf-capture inputs.
    let thermal: Vec<String> = util::list_prefixed("/sys/class/thermal", "thermal_zone")
        .iter()
        .filter_map(|z| util::read_trim(z.join("type")))
        .collect();
    let devfreq: Vec<String> = util::list_prefixed("/sys/class/devfreq", "")
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    let kgsl = Path::new("/sys/class/kgsl/kgsl-3d0").exists();
    let steam_logs = home.join(".local/share/Steam/logs").is_dir();
    let n_thermal = thermal.len();
    o.set(
        "perf_paths",
        json!({
            "thermal_zone_types": thermal,
            "devfreq": devfreq,
            "kgsl_3d0": kgsl,
            "drm_card0_devfreq": Path::new("/sys/class/drm/card0/device/devfreq").exists(),
            "steam_logs_dir": steam_logs,
        }),
    );
    o.finding(
        "perf_paths",
        if n_thermal > 0 {
            Status::Pass
        } else {
            Status::Fail
        },
        &["I14"],
        format!(
            "{n_thermal} thermal zones, kgsl-3d0 {}, Steam logs dir {}",
            if kgsl { "present" } else { "absent" },
            if steam_logs { "present" } else { "absent" }
        ),
    );

    let passed = o
        .findings
        .iter()
        .filter(|f| f.status == Status::Pass)
        .count();
    let total = o.findings.len();
    let status = if devkit.is_none() && !game.is_dir() {
        Status::Unknown
    } else {
        crate::checks::combine(&o)
    };
    o.finish(
        status,
        format!("{passed}/{total} install-environment assumptions hold"),
    )
}

/// First line of `<tool> --version` (GNU tools print their package name).
fn first_line_of(tool: &str) -> String {
    Command::new(tool)
        .arg("--version")
        .output()
        .ok()
        .map(|o| {
            let s = String::from_utf8_lossy(&o.stdout).into_owned();
            let s = if s.trim().is_empty() {
                String::from_utf8_lossy(&o.stderr).into_owned()
            } else {
                s
            };
            s.lines().next().unwrap_or("").chars().take(80).collect()
        })
        .unwrap_or_default()
}

/// Minimal HTTP/1.0 GET to 127.0.0.1:`port`. Returns status and body.
fn http_get_local(port: u16, path: &str) -> Option<(u16, String)> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(800)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    s.set_write_timeout(Some(Duration::from_secs(2))).ok()?;
    write!(
        s,
        "GET {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut buf = Vec::new();
    let _ = s.take(256 << 10).read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf).into_owned();
    let code = text
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    let body = text
        .split_once("\r\n\r\n")
        .map(|x| x.1)
        .unwrap_or("")
        .to_string();
    Some((code, body))
}
