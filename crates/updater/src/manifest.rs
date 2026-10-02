//! Release manifest: what the updater downloads (`<channel>.json`) before
//! anything else, always alongside its detached signature (`<channel>.json.sig`).
//!
//! ```json
//! {
//!   "schema": 1,
//!   "name": "frameplayer",
//!   "version": "0.2.0",
//!   "channel": "stable",
//!   "published": "2026-10-01T12:00:00Z",
//!   "min_steamos": "3.8.0",
//!   "notes": "Markdown release notes",
//!   "artifacts": [{
//!     "arch": "aarch64",
//!     "format": "tar.gz",
//!     "url": "https://github.com/.../frameplayer-0.2.0-aarch64.tar.gz",
//!     "size": 41234567,
//!     "sha256": "…64 hex…",
//!     "unpacked_size": 98765432,
//!     "unpacked_sha256": "…sha256 of the decompressed .tar…",
//!     "deltas": [{ "from": "0.1.0", "url": "…", "size": 123456, "sha256": "…" }]
//!   }]
//! }
//! ```
//!
//! Delta patches transform the *uncompressed* tar of `from` into the
//! uncompressed tar of `version` (compressed tarballs defeat block matching),
//! so any artifact with deltas must also carry `unpacked_sha256`.

use crate::{Result, UpdateError};
use chrono::{DateTime, Utc};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use url::Url;

/// Highest manifest schema this build understands.
pub const MANIFEST_SCHEMA: u32 = 1;
/// Package name every manifest must carry.
pub const PACKAGE_NAME: &str = "frameplayer";

/// Release channel. Beta users follow `beta.json`, which the release tool
/// keeps at least as new as `stable.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    #[default]
    Stable,
    Beta,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Beta => "beta",
        }
    }

    /// File name of this channel's manifest relative to the update base URL.
    pub fn manifest_file(self) -> String {
        format!("{}.json", self.as_str())
    }
}

impl std::str::FromStr for Channel {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "stable" => Ok(Channel::Stable),
            "beta" => Ok(Channel::Beta),
            other => Err(format!(
                "unknown channel {other:?} (expected stable or beta)"
            )),
        }
    }
}

impl std::fmt::Display for Channel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Compression of a release tarball.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ArchiveFormat {
    #[serde(rename = "tar")]
    Tar,
    #[default]
    #[serde(rename = "tar.gz")]
    TarGz,
    #[serde(rename = "tar.zst")]
    TarZst,
}

impl ArchiveFormat {
    /// Guess from a file name / URL path.
    pub fn from_name(name: &str) -> Option<Self> {
        let n = name.to_ascii_lowercase();
        if n.ends_with(".tar.gz") || n.ends_with(".tgz") {
            Some(Self::TarGz)
        } else if n.ends_with(".tar.zst") || n.ends_with(".tzst") {
            Some(Self::TarZst)
        } else if n.ends_with(".tar") {
            Some(Self::Tar)
        } else {
            None
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Tar => "tar",
            Self::TarGz => "tar.gz",
            Self::TarZst => "tar.zst",
        }
    }
}

/// A delta patch from an older version's uncompressed tar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeltaPatch {
    pub from: Version,
    pub url: Url,
    pub size: u64,
    pub sha256: String,
}

/// One downloadable build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    /// Rust-style arch name: `aarch64` for the Frame.
    pub arch: String,
    #[serde(default)]
    pub format: ArchiveFormat,
    pub url: Url,
    pub size: u64,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unpacked_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unpacked_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deltas: Vec<DeltaPatch>,
}

impl Artifact {
    /// The delta patch that starts from `from`, if published.
    pub fn delta_from(&self, from: &Version) -> Option<&DeltaPatch> {
        self.deltas.iter().find(|d| &d.from == from)
    }
}

/// The signed release manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseManifest {
    pub schema: u32,
    pub name: String,
    pub version: Version,
    pub channel: Channel,
    pub published: DateTime<Utc>,
    /// Minimum SteamOS `VERSION_ID` (from `/etc/os-release`) this build runs on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_steamos: Option<String>,
    #[serde(default)]
    pub notes: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes_url: Option<Url>,
    pub artifacts: Vec<Artifact>,
}

impl ReleaseManifest {
    /// Parse and validate. Callers must verify the signature over the same
    /// bytes first (see [`crate::signing::ManifestVerifier::verify_and_parse`]).
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let m: ReleaseManifest = serde_json::from_slice(bytes)?;
        m.validate()?;
        Ok(m)
    }

    pub fn to_json_pretty(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)? + "\n")
    }

    /// Structural checks beyond what serde enforces.
    pub fn validate(&self) -> Result<()> {
        let bad = |m: String| Err(UpdateError::InvalidManifest(m));
        if self.schema == 0 || self.schema > MANIFEST_SCHEMA {
            return bad(format!(
                "unsupported schema {} (this build understands up to {MANIFEST_SCHEMA})",
                self.schema
            ));
        }
        if self.name != PACKAGE_NAME {
            return bad(format!(
                "manifest is for {:?}, not {PACKAGE_NAME:?}",
                self.name
            ));
        }
        if self.channel == Channel::Stable && !self.version.pre.is_empty() {
            return bad(format!(
                "pre-release {} on the stable channel",
                self.version
            ));
        }
        if self.artifacts.is_empty() {
            return bad("no artifacts".into());
        }
        if let Some(min) = &self.min_steamos {
            if parse_dotted(min).is_none() {
                return bad(format!("min_steamos {min:?} is not a dotted version"));
            }
        }
        let mut arches = std::collections::HashSet::new();
        for a in &self.artifacts {
            if a.arch.is_empty() || !arches.insert(a.arch.as_str()) {
                return bad(format!("empty or duplicate arch {:?}", a.arch));
            }
            check_url(&a.url)?;
            check_hex(&a.sha256, "artifact sha256")?;
            if a.size == 0 {
                return bad(format!("artifact {} has size 0", a.arch));
            }
            if let Some(h) = &a.unpacked_sha256 {
                check_hex(h, "unpacked_sha256")?;
            }
            if !a.deltas.is_empty() && a.unpacked_sha256.is_none() {
                return bad(format!(
                    "artifact {} has deltas but no unpacked_sha256",
                    a.arch
                ));
            }
            for d in &a.deltas {
                check_url(&d.url)?;
                check_hex(&d.sha256, "delta sha256")?;
                if d.from >= self.version {
                    return bad(format!(
                        "delta from {} is not older than {}",
                        d.from, self.version
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn artifact_for(&self, arch: &str) -> Option<&Artifact> {
        self.artifacts.iter().find(|a| a.arch == arch)
    }

    pub fn is_newer_than(&self, current: &Version) -> bool {
        self.version > *current
    }

    /// Whether this release supports the running SteamOS version. Unknown
    /// host versions are accepted (dev machines, non-SteamOS test rigs).
    pub fn supports_steamos(&self, host: Option<&str>) -> bool {
        match (&self.min_steamos, host) {
            (Some(min), Some(host)) => compare_dotted(host, min) != Some(Ordering::Less),
            _ => true,
        }
    }
}

fn check_hex(s: &str, what: &str) -> Result<()> {
    if s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(UpdateError::InvalidManifest(format!(
            "{what} must be 64 hex characters, got {s:?}"
        )))
    }
}

/// Payload URLs must be HTTPS; plain HTTP is allowed only to loopback (tests,
/// local release dry-runs). Integrity never depends on TLS (SHA-256 + signed
/// manifest) but HTTPS keeps the download private.
fn check_url(u: &Url) -> Result<()> {
    match u.scheme() {
        "https" => Ok(()),
        "http" if matches!(u.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")) => Ok(()),
        other => Err(UpdateError::InvalidManifest(format!(
            "URL {u} uses disallowed scheme {other:?}"
        ))),
    }
}

/// Parse "3.8.0", "3.8", "0.3.0-20260901" (suffix after '-' or '+' ignored)
/// into numeric components.
pub fn parse_dotted(v: &str) -> Option<Vec<u64>> {
    let core = v.trim().split(['-', '+', '_', ' ']).next()?;
    if core.is_empty() {
        return None;
    }
    core.split('.').map(|p| p.parse().ok()).collect()
}

/// Compare dotted numeric versions, padding the shorter with zeros.
pub fn compare_dotted(a: &str, b: &str) -> Option<Ordering> {
    let (a, b) = (parse_dotted(a)?, parse_dotted(b)?);
    let n = a.len().max(b.len());
    for i in 0..n {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        match x.cmp(&y) {
            Ordering::Equal => continue,
            o => return Some(o),
        }
    }
    Some(Ordering::Equal)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn sample_json() -> String {
        let h = "a".repeat(64);
        format!(
            r#"{{
  "schema": 1,
  "name": "frameplayer",
  "version": "0.2.0",
  "channel": "stable",
  "published": "2026-10-01T12:00:00Z",
  "min_steamos": "3.8",
  "notes": "Faster seeking.",
  "artifacts": [{{
    "arch": "aarch64",
    "format": "tar.gz",
    "url": "https://example.org/frameplayer-0.2.0-aarch64.tar.gz",
    "size": 1000,
    "sha256": "{h}",
    "unpacked_sha256": "{h}",
    "deltas": [{{ "from": "0.1.0", "url": "https://example.org/d.fpd", "size": 10, "sha256": "{h}" }}]
  }}]
}}"#
        )
    }

    #[test]
    fn parse_sample() {
        let m = ReleaseManifest::parse(sample_json().as_bytes()).unwrap();
        assert_eq!(m.version, Version::new(0, 2, 0));
        assert_eq!(m.channel, Channel::Stable);
        let a = m.artifact_for("aarch64").unwrap();
        assert_eq!(a.format, ArchiveFormat::TarGz);
        assert!(a.delta_from(&Version::new(0, 1, 0)).is_some());
        assert!(a.delta_from(&Version::new(0, 0, 9)).is_none());
        assert!(m.artifact_for("x86_64").is_none());
        assert!(m.is_newer_than(&Version::new(0, 1, 5)));
        assert!(!m.is_newer_than(&Version::new(0, 2, 0)));
    }

    #[test]
    fn roundtrip_json() {
        let m = ReleaseManifest::parse(sample_json().as_bytes()).unwrap();
        let again = ReleaseManifest::parse(m.to_json_pretty().unwrap().as_bytes()).unwrap();
        assert_eq!(m, again);
    }

    #[test]
    fn rejects_bad_fields() {
        let base = sample_json();
        for (from, to) in [
            (r#""schema": 1"#, r#""schema": 99"#),
            (r#""name": "frameplayer""#, r#""name": "other""#),
            (r#""version": "0.2.0""#, r#""version": "0.2.0-beta.1""#),
            (
                "https://example.org/frameplayer",
                "ftp://example.org/frameplayer",
            ),
            (r#""size": 1000"#, r#""size": 0"#),
            (r#""from": "0.1.0""#, r#""from": "0.3.0""#),
            (r#""min_steamos": "3.8""#, r#""min_steamos": "latest""#),
        ] {
            let bad = base.replacen(from, to, 1);
            assert_ne!(bad, base, "replacement {from} did not apply");
            assert!(
                ReleaseManifest::parse(bad.as_bytes()).is_err(),
                "accepted {to}"
            );
        }
        let short_hash = base.replacen(&"a".repeat(64), "abc", 1);
        assert!(ReleaseManifest::parse(short_hash.as_bytes()).is_err());
    }

    #[test]
    fn beta_allows_prerelease() {
        let j = sample_json()
            .replace(r#""version": "0.2.0""#, r#""version": "0.2.0-beta.1""#)
            .replace(r#""channel": "stable""#, r#""channel": "beta""#);
        let m = ReleaseManifest::parse(j.as_bytes()).unwrap();
        assert!(m.is_newer_than(&Version::new(0, 1, 9)));
        assert!(!m.is_newer_than(&Version::new(0, 2, 0)));
    }

    #[test]
    fn loopback_http_allowed() {
        let j = sample_json().replace("https://example.org", "http://127.0.0.1:8000");
        assert!(ReleaseManifest::parse(j.as_bytes()).is_ok());
    }

    #[test]
    fn steamos_gate() {
        let m = ReleaseManifest::parse(sample_json().as_bytes()).unwrap();
        assert!(m.supports_steamos(Some("3.8.0")));
        assert!(m.supports_steamos(Some("3.10")));
        assert!(!m.supports_steamos(Some("3.7.13")));
        assert!(m.supports_steamos(None));
    }

    #[test]
    fn dotted_compare() {
        assert_eq!(compare_dotted("3.8", "3.8.0"), Some(Ordering::Equal));
        assert_eq!(
            compare_dotted("0.3.0-20260901", "0.3"),
            Some(Ordering::Equal)
        );
        assert_eq!(compare_dotted("3.10", "3.9"), Some(Ordering::Greater));
        assert_eq!(compare_dotted("x", "1"), None);
    }

    #[test]
    fn channel_and_format_helpers() {
        assert_eq!("BETA".parse::<Channel>().unwrap(), Channel::Beta);
        assert!("nightly".parse::<Channel>().is_err());
        assert_eq!(Channel::Stable.manifest_file(), "stable.json");
        assert_eq!(
            ArchiveFormat::from_name("a.tgz"),
            Some(ArchiveFormat::TarGz)
        );
        assert_eq!(
            ArchiveFormat::from_name("a.tar.zst"),
            Some(ArchiveFormat::TarZst)
        );
        assert_eq!(ArchiveFormat::from_name("a.zip"), None);
    }
}
