//! Self-test reports on the PC side: where to save them (the Desktop), what
//! to call them, and the single paste-ready text file the owner sends us.
//!
//! The probe writes `$HOME/frameplayer-probe-report.json` and `.txt` on the
//! headset. We copy both back, then build `FramePlayer-report-<date>.txt`:
//! a header line, the human summary, then the full JSON (redacted, capped at
//! [`PASTE_LIMIT`] bytes so it pastes cleanly into a chat). The separate
//! `.json` is kept next to it.

use crate::redact::{redact, RedactContext};
use std::path::{Path, PathBuf};

/// Report files on the headset, relative to `$HOME`.
pub const REMOTE_REPORT_JSON: &str = "frameplayer-probe-report.json";
pub const REMOTE_REPORT_TXT: &str = "frameplayer-probe-report.txt";

/// Size cap of the paste-ready text file.
pub const PASTE_LIMIT: usize = 60 * 1024;
/// GitHub caps issue bodies at 65536 characters.
pub const ISSUE_BODY_LIMIT: usize = 65_000;
/// Project issue tracker (public).
pub const ISSUES_NEW_URL: &str = "https://github.com/yellkell/frameplayer/issues/new";

/// Where to save files for the user: the Desktop (Windows Known Folder,
/// which follows OneDrive redirection), else `~/Desktop`, else the
/// installer's folder, else home, else the current directory. Pure so it
/// can be tested.
pub fn pick_output_dir(
    desktop: Option<PathBuf>,
    exe_dir: Option<PathBuf>,
    home: Option<PathBuf>,
) -> PathBuf {
    let home_desktop = home.as_ref().map(|h| h.join("Desktop"));
    [desktop, home_desktop, exe_dir, home]
        .into_iter()
        .flatten()
        .find(|d| d.is_dir())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// [`pick_output_dir`] for this machine. `dirs::desktop_dir` uses
/// `SHGetKnownFolderPath(FOLDERID_Desktop)` on Windows, so a Desktop moved
/// into OneDrive is found.
pub fn output_dir() -> PathBuf {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    pick_output_dir(dirs::desktop_dir(), exe_dir, dirs::home_dir())
}

/// `2026-10-03_14-05` (no colons: invalid in Windows file names).
pub fn stamp(t: chrono::DateTime<chrono::Local>) -> String {
    t.format("%Y-%m-%d_%H-%M").to_string()
}

/// `FramePlayer-report-<stamp>.json` / `.txt`.
pub fn report_names(stamp: &str) -> (String, String) {
    (
        format!("FramePlayer-report-{stamp}.json"),
        format!("FramePlayer-report-{stamp}.txt"),
    )
}

/// `FramePlayer-install-log-<stamp>.txt`.
pub fn log_name(stamp: &str) -> String {
    format!("FramePlayer-install-log-{stamp}.txt")
}

/// `dir/name`, or `dir/name (2).ext`, … if that file already exists.
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    if !p.exists() {
        return p;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) => (s, format!(".{e}")),
        None => (name, String::new()),
    };
    (2..1000)
        .map(|i| dir.join(format!("{stem} ({i}){ext}")))
        .find(|p| !p.exists())
        .unwrap_or(p)
}

/// Split a JSON object's top level into `(key, raw value text)` in source
/// order, without reformatting nested values. `None` if not an object.
pub fn split_top_level(json: &str) -> Option<Vec<(String, String)>> {
    let b = json.as_bytes();
    let mut i = json.find('{')? + 1;
    let mut out = Vec::new();
    let skip_ws = |i: &mut usize| {
        while *i < b.len() && (b[*i] as char).is_whitespace() {
            *i += 1;
        }
    };
    // End index (exclusive) of the string starting at b[i] == '"'.
    let string_end = |mut i: usize| -> Option<usize> {
        i += 1;
        while i < b.len() {
            match b[i] {
                b'\\' => i += 2,
                b'"' => return Some(i + 1),
                _ => i += 1,
            }
        }
        None
    };
    loop {
        skip_ws(&mut i);
        match b.get(i)? {
            b'}' => return Some(out),
            b',' => {
                i += 1;
                continue;
            }
            b'"' => {}
            _ => return None,
        }
        let kend = string_end(i)?;
        let key: String = serde_json::from_str(&json[i..kend]).ok()?;
        i = kend;
        skip_ws(&mut i);
        if b.get(i)? != &b':' {
            return None;
        }
        i += 1;
        skip_ws(&mut i);
        let vstart = i;
        let mut depth = 0i32;
        while i < b.len() {
            match b[i] {
                b'"' => {
                    i = string_end(i)?;
                    continue;
                }
                b'{' | b'[' => depth += 1,
                b'}' | b']' if depth == 0 => break,
                b'}' | b']' => depth -= 1,
                b',' if depth == 0 => break,
                _ => {}
            }
            i += 1;
        }
        out.push((key, json[vstart..i].trim_end().to_string()));
    }
}

/// Keys that survive truncation: metadata before `summary`, `summary`
/// itself, and the video/Vulkan/OpenXR checks.
fn is_essential(key: &str, before_summary: bool) -> bool {
    let k = key.to_ascii_lowercase();
    before_summary
        || k == "summary"
        || ["video", "vulkan", "openxr", "xr"]
            .iter()
            .any(|w| k.contains(w))
}

/// Shrink a JSON report to at most `limit` bytes by dropping the least
/// important top-level sections (the probe orders checks by importance, so
/// from the end), keeping essentials. Returns the text and the dropped keys.
pub fn fit_json(json: &str, limit: usize) -> (String, Vec<String>) {
    if json.len() <= limit {
        return (json.to_string(), Vec::new());
    }
    let Some(mut entries) = split_top_level(json) else {
        return (hard_cut(json, limit), vec!["(cut)".into()]);
    };
    let render = |e: &[(String, String)]| {
        let body: Vec<String> = e
            .iter()
            .map(|(k, v)| format!("  {}: {v}", serde_json::to_string(k).unwrap_or_default()))
            .collect();
        format!("{{\n{}\n}}", body.join(",\n"))
    };
    let summary_at = entries.iter().position(|(k, _)| k == "summary");
    let mut essential: Vec<bool> = entries
        .iter()
        .enumerate()
        .map(|(i, (k, _))| is_essential(k, summary_at.is_some_and(|s| i < s)))
        .collect();
    let mut dropped = Vec::new();
    let mut text = render(&entries);
    while text.len() > limit {
        if let Some(i) = essential.iter().rposition(|e| !e) {
            dropped.push(entries.remove(i).0);
            essential.remove(i);
        } else if entries.len() > 1 {
            // Still too big: drop essentials too, keeping the first section.
            dropped.push(entries.pop().expect("non-empty").0);
            essential.pop();
        } else {
            return (hard_cut(&text, limit), dropped);
        }
        text = render(&entries);
    }
    (text, dropped)
}

fn hard_cut(s: &str, limit: usize) -> String {
    let mut end = limit.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…(cut)", &s[..end])
}

/// Inputs for the paste-ready report.
pub struct ReportText<'a> {
    pub summary: &'a str,
    pub json: &'a str,
    /// e.g. `"FramePlayer self-test report, 2026-10-03 14:05, probe 0.1.0"`.
    pub header: &'a str,
}

/// Build the single paste-ready text (redacted, within `limit` bytes).
pub fn paste_ready(r: &ReportText, ctx: &RedactContext, limit: usize) -> String {
    let summary = redact(r.summary.trim(), ctx);
    let json = redact(r.json.trim(), ctx);
    let head = format!(
        "{}\n\n=== Summary ===\n{summary}\n\n=== Full report (JSON) ===\n",
        r.header
    );
    let budget = limit.saturating_sub(head.len() + 200).max(1024);
    let (json, dropped) = fit_json(&json, budget);
    let mut out = head;
    out.push_str(&json);
    out.push('\n');
    if !dropped.is_empty() {
        out.push_str(&format!(
            "\n(truncated to fit: left out {}; the full file is on the Desktop as .json)\n",
            dropped.join(", ")
        ));
    }
    out
}

/// Probe version from the report JSON (`probe_version` / `version` /
/// `probe.version`), if present.
pub fn probe_version(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let s = v
        .get("probe_version")
        .or_else(|| v.get("version"))
        .or_else(|| v.pointer("/probe/version"))?;
    s.as_str().map(str::to_string)
}

/// GitHub issue body: header, summary, then the JSON in a fenced block,
/// within [`ISSUE_BODY_LIMIT`].
pub fn issue_body(r: &ReportText, ctx: &RedactContext) -> String {
    let summary = redact(r.summary.trim(), ctx);
    let head = format!("{}\n\n```text\n{summary}\n```\n\n", r.header);
    let budget = ISSUE_BODY_LIMIT.saturating_sub(head.len() + 200).max(1024);
    let (json, dropped) = fit_json(&redact(r.json.trim(), ctx), budget);
    let mut out = format!("{head}```json\n{json}\n```\n");
    if !dropped.is_empty() {
        out.push_str(&format!(
            "\n_truncated (left out {}); full file on Desktop_\n",
            dropped.join(", ")
        ));
    }
    out
}

/// `issues/new` URL with a title, label and a short instruction body (the
/// report itself goes through the clipboard: URLs must stay short).
pub fn issue_url(title: &str, label: &str) -> String {
    let mut u = url::Url::parse(ISSUES_NEW_URL).expect("valid");
    u.query_pairs_mut()
        .append_pair("title", title)
        .append_pair("labels", label)
        .append_pair(
            "body",
            "Paste the report here (press Ctrl+V), then click Submit new issue.",
        );
    u.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn output_dir_preference() {
        let d = tempfile::tempdir().unwrap();
        let desk = d.path().join("OneDrive").join("Desktop");
        std::fs::create_dir_all(&desk).unwrap();
        let exe = d.path().join("Downloads");
        std::fs::create_dir_all(&exe).unwrap();
        assert_eq!(
            pick_output_dir(Some(desk.clone()), Some(exe.clone()), None),
            desk
        );
        // Missing Desktop (or none reported): next to the exe.
        assert_eq!(
            pick_output_dir(Some(d.path().join("nope")), Some(exe.clone()), None),
            exe
        );
        assert_eq!(
            pick_output_dir(None, None, Some(d.path().to_path_buf())),
            d.path()
        );
        // Unregistered Desktop (Linux without user-dirs.dirs): ~/Desktop.
        let home = d.path().join("home");
        std::fs::create_dir_all(home.join("Desktop")).unwrap();
        assert_eq!(
            pick_output_dir(None, Some(exe.clone()), Some(home.clone())),
            home.join("Desktop")
        );
        assert_eq!(pick_output_dir(None, None, None), PathBuf::from("."));
    }

    #[test]
    fn names_and_stamp() {
        let t = chrono::Local
            .with_ymd_and_hms(2026, 10, 3, 14, 5, 9)
            .unwrap();
        let s = stamp(t);
        assert_eq!(s, "2026-10-03_14-05");
        let (j, x) = report_names(&s);
        assert_eq!(j, "FramePlayer-report-2026-10-03_14-05.json");
        assert_eq!(x, "FramePlayer-report-2026-10-03_14-05.txt");
        assert_eq!(log_name(&s), "FramePlayer-install-log-2026-10-03_14-05.txt");
        assert!(!s.contains(':'));
    }

    #[test]
    fn unique_paths() {
        let d = tempfile::tempdir().unwrap();
        let a = unique_path(d.path(), "r.txt");
        assert_eq!(a, d.path().join("r.txt"));
        std::fs::write(&a, "x").unwrap();
        assert_eq!(unique_path(d.path(), "r.txt"), d.path().join("r (2).txt"));
    }

    #[test]
    fn splits_top_level_in_order() {
        let j = r#"{ "schema": "x", "summary": {"a": [1, {"b": "}"}]}, "video": "q\"uote,", "z": null }"#;
        let e = split_top_level(j).unwrap();
        let keys: Vec<&str> = e.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["schema", "summary", "video", "z"]);
        assert_eq!(e[1].1, r#"{"a": [1, {"b": "}"}]}"#);
        assert_eq!(e[2].1, r#""q\"uote,""#);
        assert!(split_top_level("[1]").is_none());
        assert_eq!(split_top_level("{}").unwrap(), vec![]);
    }

    fn big_report() -> String {
        let pad = |n: usize| format!("\"{}\"", "x".repeat(n));
        format!(
            "{{\"schema\":\"frameplayer-probe-report\",\"probe_version\":\"0.1.0\",\"summary\":{{\"counts\":{{}}}},\
             \"vulkan\":{{\"d\":{}}},\"openxr\":{{\"d\":{}}},\"video_decode\":{{\"d\":{}}},\
             \"audio\":{{\"d\":{}}},\"network\":{{\"d\":{}}},\"storage\":{{\"d\":{}}}}}",
            pad(5000),
            pad(5000),
            pad(5000),
            pad(30000),
            pad(30000),
            pad(30000)
        )
    }

    #[test]
    fn fit_json_drops_least_important_sections() {
        let j = big_report();
        let (small, dropped) = fit_json(&j, 40_000);
        assert!(small.len() <= 40_000, "{}", small.len());
        assert_eq!(dropped, ["storage", "network", "audio"]);
        let v: serde_json::Value = serde_json::from_str(&small).expect("still valid JSON");
        for k in [
            "schema",
            "probe_version",
            "summary",
            "vulkan",
            "openxr",
            "video_decode",
        ] {
            assert!(v.get(k).is_some(), "{k} kept");
        }
        let (same, none) = fit_json("{\"a\":1}", 100);
        assert_eq!((same.as_str(), none.len()), ("{\"a\":1}", 0));
        let (cut, _) = fit_json(&"y".repeat(500), 100);
        assert!(cut.len() < 120);
    }

    #[test]
    fn paste_ready_text_is_redacted_and_capped() {
        let json = big_report().replace("\"counts\":{}", "\"counts\":{},\"ip\":\"192.168.1.9\"");
        let r = ReportText {
            summary: "PASS vulkan\nheadset 192.168.1.9 user /home/jane",
            json: &json,
            header: "FramePlayer self-test report, 2026-10-03 14:05, probe 0.1.0",
        };
        let t = paste_ready(&r, &RedactContext::default(), PASTE_LIMIT);
        assert!(t.len() <= PASTE_LIMIT);
        assert!(t.starts_with("FramePlayer self-test report"));
        assert!(t.contains("=== Summary ===\nPASS vulkan\nheadset <ip> user ~"));
        assert!(!t.contains("192.168.1.9"));
        assert!(t.contains("truncated to fit"));
        let small = paste_ready(
            &ReportText {
                summary: "ok",
                json: "{\"a\":1}",
                header: "h",
            },
            &RedactContext::default(),
            PASTE_LIMIT,
        );
        assert!(!small.contains("truncated"));
        assert_eq!(probe_version(&json).as_deref(), Some("0.1.0"));
    }

    #[test]
    fn issue_body_and_url() {
        let json = big_report().replace("30000", "1");
        let r = ReportText {
            summary: "PASS",
            json: &json,
            header: "Self-test report",
        };
        let b = issue_body(&r, &RedactContext::default());
        assert!(b.len() <= 65_536);
        assert!(b.contains("```json\n{"));
        let u = issue_url("Self-test report 2026-10-03 0.1.0", "probe-report");
        assert!(u.starts_with("https://github.com/yellkell/frameplayer/issues/new?title=Self-test+report+2026-10-03+0.1.0&labels=probe-report&body=Paste"));
        assert!(u.len() < 8000);
    }
}
