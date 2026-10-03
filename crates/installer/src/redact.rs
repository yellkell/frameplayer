//! Scrub personal and network details from text before it leaves the PC
//! (clipboard, report files meant for pasting into a chat or a public issue).
//!
//! Defence in depth: the probe already redacts at the source. Replaced:
//! IPv4/IPv6 addresses (loopback kept), MAC addresses, home-directory paths
//! (`/home/<u>` and `/Users/<u>` → `~`, `C:\Users\<u>` → `%USERPROFILE%`),
//! known names (Windows user, PC hostname, headset hostname and login),
//! SSH fingerprints and public keys, and token-like values (`token=…`,
//! `key=…`, long hex runs, long base64 runs).

use regex::{Captures, Regex};
use std::sync::OnceLock;

/// Names to remove in addition to the generic patterns.
#[derive(Debug, Clone, Default)]
pub struct RedactContext {
    /// `(value, placeholder)` pairs, e.g. `("JaneDoe", "<user>")`.
    pub names: Vec<(String, &'static str)>,
}

/// Values too generic to scrub without destroying the report (SteamOS
/// account and default host names).
const GENERIC: &[&str] = &[
    "steamos",
    "deck",
    "steam",
    "frame",
    "steamframe",
    "root",
    "user",
    "admin",
    "localhost",
    "desktop",
    "linux",
    "windows",
    "home",
];

impl RedactContext {
    /// Context for this PC: `%USERNAME%`/`$USER` and the hostname.
    pub fn for_this_pc() -> Self {
        let mut c = Self::default();
        for v in ["USERNAME", "USER", "LOGNAME"] {
            if let Ok(u) = std::env::var(v) {
                c.add(&u, "<user>");
            }
        }
        for v in ["COMPUTERNAME", "HOSTNAME"] {
            if let Ok(h) = std::env::var(v) {
                c.add(&h, "<pc>");
            }
        }
        if let Some(h) = dirs::home_dir() {
            if let Some(n) = h.file_name() {
                c.add(&n.to_string_lossy(), "<user>");
            }
        }
        c
    }

    /// Add a name to scrub (ignored when short or generic).
    pub fn add(&mut self, value: &str, placeholder: &'static str) {
        let v = value
            .trim()
            .trim_end_matches(".local")
            .trim_end_matches('.');
        if v.chars().count() < 3 || GENERIC.contains(&v.to_ascii_lowercase().as_str()) {
            return;
        }
        if !self.names.iter().any(|(n, _)| n.eq_ignore_ascii_case(v)) {
            self.names.push((v.to_string(), placeholder));
        }
    }
}

struct Patterns {
    win_home_sep: Regex,
    win_home: Regex,
    unix_home: Regex,
    fingerprint: Regex,
    pubkey: Regex,
    kv_secret: Regex,
    mac: Regex,
    ipv6: Regex,
    ipv4: Regex,
    hex: Regex,
    b64: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| Patterns {
        // C:\Users\name, C:\\Users\\name (JSON-escaped), C:/Users/name
        // With a trailing separator the name may contain spaces.
        win_home_sep: Regex::new(
            r#"(?i)\b[a-z]:(?:\\\\|\\|/)users(?:\\\\|\\|/)[^\\/:*?<>|"\r\n]+(\\\\|\\|/)"#,
        )
        .unwrap(),
        win_home: Regex::new(r"(?i)\b[a-z]:(?:\\\\|\\|/)users(?:\\\\|\\|/)[^\\/:*?<>|\s]+").unwrap(),
        unix_home: Regex::new(r"(?:/home|/Users|/var/home)/[A-Za-z0-9._-]+").unwrap(),
        fingerprint: Regex::new(r"\b(SHA256|MD5):[A-Za-z0-9+/:=]{16,}").unwrap(),
        pubkey: Regex::new(r"\b(ssh-(?:ed25519|rsa|dss)|ecdsa-sha2-nistp\d+) AAAA[A-Za-z0-9+/=]+")
            .unwrap(),
        kv_secret: Regex::new(
            r#"(?i)\b(token|access_token|api_key|apikey|key|secret|password|passwd|pwd|auth|authorization|cookie|session)("?\s*[=:]\s*"?)([^\s"',;&]+)"#,
        )
        .unwrap(),
        mac: Regex::new(r"\b[0-9A-Fa-f]{2}(?:[:-][0-9A-Fa-f]{2}){5}\b").unwrap(),
        // Candidates; validated in code (needs "::" or 7 colons).
        ipv6: Regex::new(r"(?i)(?:[0-9a-f]{0,4}:){2,7}[0-9a-f]{0,4}(?:%[0-9a-z]+)?").unwrap(),
        ipv4: Regex::new(r"\b(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})\b").unwrap(),
        hex: Regex::new(r"\b[0-9a-fA-F]{32,}\b").unwrap(),
        b64: Regex::new(r"[A-Za-z0-9+]{32,}={0,2}").unwrap(),
    })
}

fn is_ipv6_candidate(s: &str) -> bool {
    let core = s.split('%').next().unwrap_or(s);
    let colons = core.matches(':').count();
    let groups = core.split(':').filter(|g| !g.is_empty()).count();
    let has_digit = core.bytes().any(|b| b.is_ascii_digit());
    has_digit && groups >= 1 && (colons == 7 || (core.contains("::") && !core.contains(":::")))
}

/// Redact `text`.
pub fn redact(text: &str, ctx: &RedactContext) -> String {
    let p = patterns();
    let mut s = p
        .win_home_sep
        .replace_all(text, "%USERPROFILE%$1")
        .into_owned();
    s = p.win_home.replace_all(&s, "%USERPROFILE%").into_owned();
    s = p.unix_home.replace_all(&s, "~").into_owned();
    s = p
        .fingerprint
        .replace_all(&s, "$1:<fingerprint>")
        .into_owned();
    s = p.pubkey.replace_all(&s, "$1 <public-key>").into_owned();
    s = p
        .kv_secret
        .replace_all(&s, |c: &Captures| format!("{}{}<redacted>", &c[1], &c[2]))
        .into_owned();
    s = p.mac.replace_all(&s, "<mac>").into_owned();
    s = replace_ipv6(&s);
    s = p
        .ipv4
        .replace_all(&s, |c: &Captures| {
            let oct: Vec<u32> = (1..=4).map(|i| c[i].parse().unwrap_or(999)).collect();
            if oct.iter().any(|&o| o > 255) || oct[0] == 127 || oct == [0, 0, 0, 0] {
                c[0].to_string()
            } else {
                "<ip>".to_string()
            }
        })
        .into_owned();
    s = p.hex.replace_all(&s, "<hex>").into_owned();
    s = p
        .b64
        .replace_all(&s, |c: &Captures| {
            let m = &c[0];
            let mixed = m.bytes().any(|b| b.is_ascii_digit())
                && m.bytes().any(|b| b.is_ascii_uppercase())
                && m.bytes().any(|b| b.is_ascii_lowercase());
            if mixed {
                "<token>".to_string()
            } else {
                m.to_string()
            }
        })
        .into_owned();
    for (name, ph) in &ctx.names {
        s = replace_word_ci(&s, name, ph);
    }
    s
}

fn replace_ipv6(s: &str) -> String {
    let p = patterns();
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for m in p.ipv6.find_iter(s) {
        let (a, b) = (m.start(), m.end());
        // Must not be glued to other word characters (e.g. `fp_core::detect`).
        let before = s[..a].chars().next_back();
        let after = s[b..].chars().next();
        let glued = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
        let text = m.as_str();
        let core = text.split('%').next().unwrap_or(text);
        let loopback = core == "::1" || core == "0:0:0:0:0:0:0:1";
        if glued(before) || glued(after) || loopback || !is_ipv6_candidate(text) {
            continue;
        }
        out.push_str(&s[last..a]);
        out.push_str("<ip>");
        last = b;
    }
    out.push_str(&s[last..]);
    out
}

/// Case-insensitive whole-word replacement (ASCII word boundaries).
fn replace_word_ci(s: &str, word: &str, ph: &str) -> String {
    let re = Regex::new(&format!(
        r"(?i)(^|[^A-Za-z0-9_]){}($|[^A-Za-z0-9_])",
        regex::escape(word)
    ));
    let Ok(re) = re else {
        return s.to_string();
    };
    // Loop because adjacent matches share a boundary character.
    let mut cur = s.to_string();
    loop {
        let next = re
            .replace_all(&cur, |c: &Captures| format!("{}{ph}{}", &c[1], &c[2]))
            .into_owned();
        if next == cur {
            return next;
        }
        cur = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(s: &str) -> String {
        redact(s, &RedactContext::default())
    }

    #[test]
    fn ipv4_addresses() {
        assert_eq!(r("headset at 192.168.1.23:22 ok"), "headset at <ip>:22 ok");
        assert_eq!(r("10.0.0.5,172.16.4.1"), "<ip>,<ip>");
        assert_eq!(r("loopback 127.0.0.1 stays"), "loopback 127.0.0.1 stays");
        assert_eq!(r("bind 0.0.0.0"), "bind 0.0.0.0");
        assert_eq!(
            r("version 1.2.3 and 300.1.1.1"),
            "version 1.2.3 and 300.1.1.1"
        );
    }

    #[test]
    fn ipv6_addresses() {
        assert_eq!(r("addr fe80::1c2b:3aff:fe4d:5e6f%wlan0 up"), "addr <ip> up");
        assert_eq!(r("2001:db8:0:0:0:0:2:1"), "<ip>");
        assert_eq!(r("[2001:db8::7]:32000"), "[<ip>]:32000");
        assert_eq!(r("loop ::1 kept"), "loop ::1 kept");
        assert_eq!(r("at 12:34:56 today"), "at 12:34:56 today");
        assert_eq!(r("fp_core::detect::run"), "fp_core::detect::run");
        assert_eq!(r("a::b"), "a::b");
    }

    #[test]
    fn mac_addresses() {
        assert_eq!(r("wlan0 a4:5e:60:c1:22:9f"), "wlan0 <mac>");
        assert_eq!(r("A4-5E-60-C1-22-9F"), "<mac>");
    }

    #[test]
    fn home_paths() {
        assert_eq!(
            r("log at /home/jane/.local/share/frameplayer"),
            "log at ~/.local/share/frameplayer"
        );
        assert_eq!(r("/Users/bob/Desktop"), "~/Desktop");
        assert_eq!(
            r(r"saved C:\Users\Jane Doe\Desktop\x.txt"),
            r"saved %USERPROFILE%\Desktop\x.txt"
        );
        assert_eq!(
            r(r"C:\Users\jane\Desktop\x.txt"),
            r"%USERPROFILE%\Desktop\x.txt"
        );
        assert_eq!(
            r(r#"{"path":"C:\\Users\\jane\\AppData"}"#),
            r#"{"path":"%USERPROFILE%\\AppData"}"#
        );
        assert_eq!(r("c:/users/jane/x"), "%USERPROFILE%/x");
    }

    #[test]
    fn names_from_context() {
        let mut c = RedactContext::default();
        c.add("JaneDoe", "<user>");
        c.add("JANES-PC", "<pc>");
        c.add("janes-frame.local", "<headset>");
        c.add("steamos", "<login>"); // generic: ignored
        c.add("ab", "<x>"); // too short: ignored
        assert_eq!(c.names.len(), 3);
        let out = redact(
            "user janedoe on janes-pc paired janes-frame (SteamOS, login steamos) JaneDoeX",
            &c,
        );
        assert_eq!(
            out,
            "user <user> on <pc> paired <headset> (SteamOS, login steamos) JaneDoeX"
        );
    }

    #[test]
    fn keys_and_tokens() {
        assert_eq!(
            r("Key fingerprint: SHA256:8nWqJ1ZkVJmZ0l6c3n1X0c2bD4fVq7PZcRj2bq0xkzE"),
            "Key fingerprint: SHA256:<fingerprint>"
        );
        assert_eq!(
            r("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ me@pc"),
            "ssh-ed25519 <public-key> me@pc"
        );
        assert_eq!(r("url?token=abc123&x=1"), "url?token=<redacted>&x=1");
        assert_eq!(r(r#""api_key": "s3cr3t""#), r#""api_key": "<redacted>""#);
        assert_eq!(r("password=hunter2"), "password=<redacted>");
        assert_eq!(r(&format!("sha {}", "ab12".repeat(16))), "sha <hex>");
        assert_eq!(
            r("bearer ghp1A2b3C4d5E6f7G8h9I0j1K2l3M4n5O6p7Q8r9"),
            "bearer <token>"
        );
    }

    #[test]
    fn leaves_ordinary_report_text_alone() {
        let s = "PASS vulkan: Turnip (Adreno 750) VK_KHR_external_memory_capabilities, \
                 /usr/share/vulkan/icd.d/freedreno_icd.aarch64.json, \
                 /org/freedesktop/NetworkManager/Devices/12, mesa 24.1.0, 2026-10-03T12:34:56Z";
        assert_eq!(r(s), s);
    }
}
