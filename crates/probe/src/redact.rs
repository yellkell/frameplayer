//! Privacy: the report is meant to be pasted into a public GitHub issue.
//!
//! Checks already avoid collecting identifying data (no IPs, MACs,
//! hostnames, serials, env values outside an allowlist). [`redact_text`] is
//! the defence-in-depth pass over the final JSON and summary text: it
//! replaces IPv4/IPv6 addresses (except loopback and the SSDP group), MAC
//! addresses, long hex/base64 runs (tokens, UUIDs), home directories and
//! the user name. The replacements never contain JSON-significant
//! characters, so redacting serialized JSON keeps it valid.
//!
//! [`finalize`] also enforces the size budget (GitHub issue bodies are
//! limited to 65 536 characters, summary included).

use crate::report::Report;
use serde_json::json;

/// Aim for reports below this many bytes of JSON.
pub const TARGET_JSON_BYTES: usize = 40_000;
/// Never write more JSON than this (leaves room for the text summary).
pub const MAX_JSON_BYTES: usize = 56_000;
/// GitHub issue body limit (summary + JSON + a little markdown).
pub const ISSUE_BODY_BYTES: usize = 64_000;

/// Account names that are platform defaults rather than personal choices;
/// these are kept (the login name is itself a platform question, I3).
pub const DEFAULT_USERS: &[&str] = &["deck", "steam", "steamos", "frame", "valve", "user"];

/// Who is running the probe, so their name and home can be scrubbed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RedactContext {
    pub username: Option<String>,
    pub home: Option<String>,
}

impl RedactContext {
    /// From `$HOME` / `$USER` / `getpwuid`.
    pub fn from_env() -> RedactContext {
        RedactContext {
            username: crate::util::username(),
            home: std::env::var("HOME").ok().filter(|h| h.len() > 1),
        }
    }

    /// The user name if it is safe to publish (a platform default).
    pub fn public_username(&self) -> Option<&str> {
        self.username
            .as_deref()
            .filter(|u| DEFAULT_USERS.contains(u))
    }
}

/// Replace `home` (or any `/home/<name>`) at the start of a path with `~`.
pub fn tilde(path: &str, home: Option<&str>) -> String {
    redact_paths(path, home)
}

/// Apply every redaction rule to `s`.
pub fn redact_text(s: &str, ctx: &RedactContext) -> String {
    let s = redact_paths(s, ctx.home.as_deref());
    let s = match ctx.username.as_deref() {
        Some(u) if u.len() >= 2 && !DEFAULT_USERS.contains(&u) => replace_word(&s, u, "[user]"),
        _ => s,
    };
    let s = redact_mac_ipv6(&s);
    let s = redact_ipv4(&s);
    redact_long_tokens(&s)
}

fn is_path_char(c: char) -> bool {
    !(c == '/' || c == '"' || c == '\\' || c == '\'' || c.is_whitespace() || c == ':')
}

/// Length of the path segment at the start of `s`.
fn segment_len(s: &str) -> usize {
    s.find(|c: char| !is_path_char(c)).unwrap_or(s.len())
}

fn redact_paths(s: &str, home: Option<&str>) -> String {
    let mut s = s.to_string();
    if let Some(h) = home.filter(|h| h.len() > 1 && *h != "/root") {
        let h = h.trim_end_matches('/');
        // Only whole path prefixes: "/home/al" must not eat "/home/alice".
        let mut out = String::with_capacity(s.len());
        let mut rest = s.as_str();
        while let Some(i) = rest.find(h) {
            let after = &rest[i + h.len()..];
            out.push_str(&rest[..i]);
            if after.chars().next().is_none_or(|c| !is_path_char(c)) {
                out.push('~');
            } else {
                out.push_str(h);
            }
            rest = after;
        }
        out.push_str(rest);
        s = out;
    }
    for prefix in ["/var/home/", "/home/"] {
        s = replace_prefixed_segment(&s, prefix, |_| "~".to_string(), true);
    }
    // /run/media/<user>/<label>: both parts can be personal.
    s = replace_prefixed_segment(
        &s,
        "/run/media/",
        |seg| {
            if DEFAULT_USERS.contains(&seg) || seg.starts_with('[') {
                format!("/run/media/{seg}")
            } else {
                "/run/media/[user]".to_string()
            }
        },
        true,
    );
    s
}

/// Replace `prefix<segment>` with `f(segment)`. With `whole` the prefix is
/// included in what gets replaced.
fn replace_prefixed_segment(
    s: &str,
    prefix: &str,
    f: impl Fn(&str) -> String,
    whole: bool,
) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(prefix) {
        // "/var/home/x" is handled by its own rule; don't re-match "/home/" inside it.
        let before = &rest[..i];
        let after = &rest[i + prefix.len()..];
        let n = segment_len(after);
        let preceded_by_path = before.chars().next_back().is_some_and(is_path_char);
        if n == 0 || preceded_by_path {
            out.push_str(before);
            out.push_str(prefix);
            rest = after;
            continue;
        }
        out.push_str(before);
        let seg = &after[..n];
        if whole {
            out.push_str(&f(seg));
        } else {
            out.push_str(prefix);
            out.push_str(&f(seg));
        }
        rest = &after[n..];
    }
    out.push_str(rest);
    out
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn replace_word(s: &str, word: &str, with: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    let mut prev: Option<char> = None;
    while let Some(i) = rest.find(word) {
        let before = &rest[..i];
        let after = &rest[i + word.len()..];
        let p = before.chars().next_back().or(prev);
        let ok = p.is_none_or(|c| !is_word(c)) && after.chars().next().is_none_or(|c| !is_word(c));
        out.push_str(before);
        out.push_str(if ok { with } else { word });
        prev = word.chars().next_back();
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Split `s` into runs where `class` holds and runs where it does not;
/// `f(run, prev_char, next_char)` may replace each matching run.
fn map_runs(
    s: &str,
    class: impl Fn(char) -> bool,
    f: impl Fn(&str, Option<char>, Option<char>) -> Option<String>,
) -> String {
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let (start, c) = chars[i];
        if !class(c) {
            out.push(c);
            i += 1;
            continue;
        }
        let mut j = i;
        while j < chars.len() && class(chars[j].1) {
            j += 1;
        }
        let end = chars.get(j).map_or(s.len(), |&(b, _)| b);
        let run = &s[start..end];
        let prev = if i > 0 { Some(chars[i - 1].1) } else { None };
        let next = chars.get(j).map(|&(_, c)| c);
        match f(run, prev, next) {
            Some(r) => out.push_str(&r),
            None => out.push_str(run),
        }
        i = j;
    }
    out
}

fn redact_mac_ipv6(s: &str) -> String {
    map_runs(
        s,
        |c| c.is_ascii_hexdigit() || c == ':',
        |run, prev, next| {
            if prev.is_some_and(is_word) || next.is_some_and(is_word) {
                return None;
            }
            if is_mac(run) {
                return Some("[mac]".into());
            }
            if is_ipv6(run) {
                return Some("[ipv6]".into());
            }
            None
        },
    )
}

fn is_mac(run: &str) -> bool {
    let parts: Vec<&str> = run.split(':').collect();
    parts.len() == 6
        && parts
            .iter()
            .all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()))
}

fn is_ipv6(run: &str) -> bool {
    let run = run.trim_end_matches(':');
    if run == "::1" || run == "::" || run.is_empty() {
        return false;
    }
    let colons = run.matches(':').count();
    let groups: Vec<&str> = run.split(':').collect();
    let nonempty = groups.iter().filter(|g| !g.is_empty()).count();
    colons >= 2
        && nonempty >= 2
        && groups.iter().all(|g| g.len() <= 4)
        && (run.contains("::") || colons == 7)
}

fn redact_ipv4(s: &str) -> String {
    map_runs(
        s,
        |c| c.is_ascii_digit() || c == '.',
        |run, prev, next| {
            if prev.is_some_and(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                || next.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                return None;
            }
            let core = run.trim_end_matches('.');
            let parts: Vec<&str> = core.split('.').collect();
            if parts.len() != 4
                || !parts
                    .iter()
                    .all(|p| (1..=3).contains(&p.len()) && p.parse::<u16>().is_ok_and(|n| n <= 255))
            {
                return None;
            }
            let keep = core.starts_with("127.")
                || core == "0.0.0.0"
                || core == "255.255.255.255"
                || core == "239.255.255.250";
            if keep {
                None
            } else {
                Some(format!("[ip]{}", &run[core.len()..]))
            }
        },
    )
}

fn redact_long_tokens(s: &str) -> String {
    map_runs(
        s,
        |c| c.is_ascii_alphanumeric() || matches!(c, '+' | '_' | '-'),
        |run, _, _| {
            if run.len() < 32 {
                return None;
            }
            let stripped: String = run.chars().filter(|&c| c != '-').collect();
            let hex = stripped.len() >= 32 && stripped.chars().all(|c| c.is_ascii_hexdigit());
            let digits = run.chars().filter(|c| c.is_ascii_digit()).count();
            let upper = run.chars().any(|c| c.is_ascii_uppercase());
            let lower = run.chars().any(|c| c.is_ascii_lowercase());
            // Identifiers (VK_EXT_..., G10X6_B10X6R10X6_...) are words joined by '_'.
            let identifier = run.contains('_') && run.split('_').all(|w| w.len() <= 16);
            if hex || (!identifier && digits >= 4 && upper && lower) {
                Some("[redacted]".into())
            } else {
                None
            }
        },
    )
}

/// Shrink `report` until its pretty JSON fits `cap` bytes: drop the
/// `data` of the least important checks first, then long finding lists and
/// stderr tails. Returns the notes about what was dropped.
pub fn fit_to_budget(report: &mut Report, cap: usize) -> Vec<String> {
    let mut notes = Vec::new();
    let size = |r: &Report| r.to_json_pretty().len();
    if size(report) <= cap {
        return notes;
    }
    for c in report.checks.iter_mut() {
        if let Some(t) = c.stderr_tail.as_mut() {
            if t.len() > 400 {
                let cut = t.len() - 400;
                let cut = (cut..t.len()).find(|&i| t.is_char_boundary(i)).unwrap_or(0);
                *t = format!("…{}", &t[cut..]);
            }
        }
    }
    let n = report.checks.len();
    for i in (0..n).rev() {
        if size(report) <= cap {
            return notes;
        }
        let c = &mut report.checks[i];
        if !c.data.is_null() && c.data.get("truncated").is_none() {
            c.data =
                json!({ "truncated": "details dropped to keep the report under the size limit" });
            notes.push(format!("dropped details of {}", c.id));
        }
    }
    for c in report.checks.iter_mut().rev() {
        if c.findings.len() <= 24 {
            continue;
        }
        c.findings.truncate(24);
        notes.push(format!("truncated findings of {}", c.id));
    }
    for c in report.checks.iter_mut() {
        c.stderr_tail = None;
    }
    notes
}

/// The shareable outputs of a run.
#[derive(Debug, Clone)]
pub struct Finalized {
    pub json: String,
    pub text: String,
    pub notes: Vec<String>,
}

/// Fit the budget, render the summary and redact both outputs.
pub fn finalize(mut report: Report, ctx: &RedactContext) -> Finalized {
    // The text summary and the JSON must fit one issue body together.
    let text_estimate = crate::summary::render(&report, 0, &[]).len() + 600;
    let cap = MAX_JSON_BYTES.min(ISSUE_BODY_BYTES.saturating_sub(text_estimate));
    let notes = fit_to_budget(&mut report, cap);
    let mut json = redact_text(&report.to_json_pretty(), ctx);
    if json.len() > cap {
        // Last resort: compact form.
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        json = serde_json::to_string(&v).expect("serializes");
    }
    let text = redact_text(&crate::summary::render(&report, json.len(), &notes), ctx);
    Finalized { json, text, notes }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> RedactContext {
        RedactContext {
            username: Some("alice".into()),
            home: Some("/home/alice".into()),
        }
    }

    #[test]
    fn ipv4() {
        let c = RedactContext::default();
        assert_eq!(redact_text("ip 192.168.1.23 up", &c), "ip [ip] up");
        assert_eq!(redact_text("\"10.0.0.5\"", &c), "\"[ip]\"");
        assert_eq!(redact_text("at 10.0.0.5.", &c), "at [ip].");
        for keep in [
            "127.0.0.1",
            "0.0.0.0",
            "239.255.255.250",
            "Mesa 24.1.0",
            "v1.2.3.4",
            "1.4.309.0",
            "6.1.52-valve16",
            "1.2.3.4.5",
            "999.1.1.1",
        ] {
            assert_eq!(redact_text(keep, &c), keep, "{keep}");
        }
    }

    #[test]
    fn mac_and_ipv6() {
        let c = RedactContext::default();
        assert_eq!(redact_text("ether 3c:22:fb:01:aa:9f", &c), "ether [mac]");
        assert_eq!(
            redact_text("fe80::1c2b:3cff:fe4d:5e6f%wlan0", &c),
            "[ipv6]%wlan0"
        );
        assert_eq!(
            redact_text("2001:0db8:85a3:0000:0000:8a2e:0370:7334", &c),
            "[ipv6]"
        );
        for keep in [
            "::1",
            "12:34:56",
            "VkResult::ERROR_UNKNOWN",
            "fp_xr::select",
            "1d6b:0002",
            "dead:beef",
        ] {
            assert_eq!(redact_text(keep, &c), keep, "{keep}");
        }
    }

    #[test]
    fn long_tokens() {
        let c = RedactContext::default();
        assert_eq!(
            redact_text("uuid 123e4567-e89b-12d3-a456-426614174000 x", &c),
            "uuid [redacted] x"
        );
        assert_eq!(
            redact_text("id=0123456789abcdef0123456789abcdef", &c),
            "id=[redacted]"
        );
        assert_eq!(
            redact_text("tok aGVsbG8gd29ybGQgdGhpcyBpcyBhIHRva2VuMTIzNDU2", &c),
            "tok [redacted]"
        );
        for keep in [
            "VK_EXT_image_drm_format_modifier",
            "G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16",
            "XR_KHR_composition_layer_cylinder_extension",
            "0x0500000000000001",
            "VK_KHR_shader_float16_int8_and_more_words",
        ] {
            assert_eq!(redact_text(keep, &c), keep, "{keep}");
        }
    }

    #[test]
    fn homes_and_users() {
        let c = ctx();
        assert_eq!(
            redact_text("/home/alice/.config/openxr/1/active_runtime.json", &c),
            "~/.config/openxr/1/active_runtime.json"
        );
        assert_eq!(
            redact_text("/home/bob/x and /var/home/carol", &c),
            "~/x and ~"
        );
        assert_eq!(redact_text("/home/alicesmith/x", &c), "~/x");
        assert_eq!(
            redact_text("user alice ok; malice stays", &c),
            "user [user] ok; malice stays"
        );
        assert_eq!(
            redact_text("/run/media/alice/MY_CARD", &c),
            "/run/media/[user]/MY_CARD"
        );
        assert_eq!(
            redact_text("/run/media/deck/SD", &RedactContext::default()),
            "/run/media/deck/SD"
        );
        assert_eq!(tilde("/home/alice", Some("/home/alice")), "~");
        // Platform-default account names are kept.
        let d = RedactContext {
            username: Some("deck".into()),
            home: Some("/home/deck".into()),
        };
        assert_eq!(redact_text("login deck", &d), "login deck");
        assert_eq!(d.public_username(), Some("deck"));
        assert_eq!(c.public_username(), None);
    }

    #[test]
    fn json_stays_valid() {
        let c = ctx();
        let v = serde_json::json!({
            "a": "/home/alice/x 10.1.2.3 3c:22:fb:01:aa:9f",
            "b": ["fe80::1", "0123456789abcdef0123456789abcdef"],
            "n": 1.5,
        });
        let s = serde_json::to_string_pretty(&v).unwrap();
        let r = redact_text(&s, &c);
        let back: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(back["a"], "~/x [ip] [mac]");
        assert_eq!(back["b"][0], "[ipv6]");
        assert_eq!(back["b"][1], "[redacted]");
        assert_eq!(back["n"], 1.5);
    }

    #[test]
    fn oversized_report_is_cut_to_budget() {
        use crate::report::{CheckOutput, CheckResult, Mode, Report, Status};
        let big: Vec<String> = (0..4000)
            .map(|i| format!("VK_EXT_some_long_extension_{i}"))
            .collect();
        let checks = ["video_decode", "vulkan", "system", "network"]
            .iter()
            .map(|id| {
                let mut c = CheckResult::from_output(
                    id,
                    id,
                    CheckOutput {
                        status: Status::Pass,
                        summary: "ok 192.168.0.9".into(),
                        findings: vec![],
                        data: serde_json::json!({ "list": big }),
                    },
                    1,
                );
                c.stderr_tail = Some("x".repeat(5000));
                c
            })
            .collect();
        let r = Report {
            probe_version: "0".into(),
            generated_at: "now".into(),
            mode: Mode::Headless,
            arch: "aarch64".into(),
            duration_ms: 0,
            checks,
        };
        let f = finalize(r, &ctx());
        assert!(f.json.len() <= MAX_JSON_BYTES, "{}", f.json.len());
        assert!(f.json.len() + f.text.len() < 65_536);
        let back = crate::report::Report::from_json(&f.json).unwrap();
        // Least important checks lose their details first.
        assert!(back
            .check("network")
            .unwrap()
            .data
            .get("truncated")
            .is_some());
        assert!(!f.notes.is_empty());
        assert!(f.text.contains("Note: dropped details of network"));
        assert!(!f.json.contains("192.168.0.9") && !f.text.contains("192.168.0.9"));
    }
}
