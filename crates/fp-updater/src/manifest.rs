//! FramePlayer's own release manifest, its detached ed25519 signature, and
//! the rules for picking an update from it.
//!
//! The manifest is published next to the release zip as `manifest.json`,
//! with `manifest.json.sig` holding the base64 ed25519 signature over the
//! exact bytes of `manifest.json`. Nothing in the manifest is trusted until
//! the signature has been verified against a key compiled into the app.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::url::check_download_url;

/// The `name` every FramePlayer release manifest must carry.
pub const MANIFEST_NAME: &str = "frameplayer";

/// Release channel. Stable users only see stable releases; beta users see
/// both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    /// Regular releases.
    #[default]
    Stable,
    /// Pre-releases for testers.
    Beta,
}

impl Channel {
    /// Lower-case name as used in manifests and on the command line.
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Beta => "beta",
        }
    }
}

impl std::fmt::Display for Channel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Channel {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "stable" => Ok(Channel::Stable),
            "beta" => Ok(Channel::Beta),
            other => Err(format!(
                "unknown channel {other:?} (expected stable or beta)"
            )),
        }
    }
}

/// One downloadable build inside a release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    /// CPU architecture as reported by `std::env::consts::ARCH`, e.g. `aarch64`.
    pub arch: String,
    /// Where to download the zip from (https).
    pub url: String,
    /// Lower-case hex SHA-256 of the zip.
    pub sha256: String,
    /// Size of the zip in bytes.
    pub size: u64,
}

/// The release manifest (`manifest.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseManifest {
    /// Always [`MANIFEST_NAME`].
    pub name: String,
    /// Release version (semver).
    pub version: Version,
    /// Channel this release belongs to.
    pub channel: Channel,
    /// Publication time, RFC 3339 (e.g. `2026-10-02T12:00:00Z`).
    pub published: String,
    /// Release notes in Markdown.
    #[serde(default)]
    pub notes: String,
    /// Builds, one per architecture.
    pub artifacts: Vec<Artifact>,
}

impl ReleaseManifest {
    /// Checks the fields a signature cannot vouch for being sensible:
    /// name, timestamp shape, URL safety, digest format, sizes, and no
    /// duplicate architectures.
    pub fn validate(&self) -> Result<()> {
        let bad = |m: String| Err(Error::InvalidManifest(m));
        if self.name != MANIFEST_NAME {
            return bad(format!(
                "name is {:?}, expected {MANIFEST_NAME:?}",
                self.name
            ));
        }
        if !looks_like_rfc3339(&self.published) {
            return bad(format!(
                "published {:?} is not an RFC 3339 timestamp",
                self.published
            ));
        }
        if self.artifacts.is_empty() {
            return bad("no artifacts".into());
        }
        for (i, a) in self.artifacts.iter().enumerate() {
            if a.arch.is_empty() {
                return bad(format!("artifact {i} has an empty arch"));
            }
            if self.artifacts[..i].iter().any(|b| b.arch == a.arch) {
                return bad(format!("artifact arch {} listed twice", a.arch));
            }
            if !is_sha256_hex(&a.sha256) {
                return bad(format!("artifact {} sha256 is not 64 hex digits", a.arch));
            }
            if a.size == 0 {
                return bad(format!("artifact {} has size 0", a.arch));
            }
            check_download_url(&a.url)?;
        }
        Ok(())
    }

    /// The artifact for `arch`, if any.
    pub fn artifact(&self, arch: &str) -> Option<&Artifact> {
        self.artifacts.iter().find(|a| a.arch == arch)
    }
}

/// An update that is newer than the running version and suits the user's
/// channel and architecture. Produced by [`crate::Updater::check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    /// Version on offer.
    pub version: Version,
    /// Channel the release was published on.
    pub channel: Channel,
    /// Publication time (RFC 3339).
    pub published: String,
    /// Release notes (Markdown).
    pub notes: String,
    /// The build to download.
    pub artifact: Artifact,
}

/// Decides whether `manifest` is an update for a user running `current` on
/// `channel` and `arch`. Pure function: no I/O, no signature check (the
/// caller must have verified the manifest already).
///
/// Rules:
/// - stable users ignore beta-channel manifests and any version with a
///   pre-release tag (`1.3.0-rc.1`);
/// - the manifest version must be strictly greater than `current`
///   (semver precedence, build metadata ignored);
/// - an otherwise-eligible release without an artifact for `arch` is an
///   error, not "no update", so the user learns their build is orphaned.
pub fn select_update(
    manifest: &ReleaseManifest,
    current: &Version,
    channel: Channel,
    arch: &str,
) -> Result<Option<Update>> {
    if channel == Channel::Stable
        && (manifest.channel == Channel::Beta || !manifest.version.pre.is_empty())
    {
        return Ok(None);
    }
    if manifest.version.cmp_precedence(current) != std::cmp::Ordering::Greater {
        return Ok(None);
    }
    let artifact = manifest.artifact(arch).ok_or_else(|| Error::NoArtifact {
        version: manifest.version.to_string(),
        arch: arch.to_string(),
    })?;
    Ok(Some(Update {
        version: manifest.version.clone(),
        channel: manifest.channel,
        published: manifest.published.clone(),
        notes: manifest.notes.clone(),
        artifact: artifact.clone(),
    }))
}

/// Builds a verifying key from 32 raw bytes, rejecting malformed points.
pub fn public_key_from_bytes(bytes: &[u8; 32]) -> Result<VerifyingKey> {
    let key = VerifyingKey::from_bytes(bytes).map_err(|e| Error::InvalidKey(e.to_string()))?;
    if key.is_weak() {
        return Err(Error::InvalidKey("weak (small-order) public key".into()));
    }
    Ok(key)
}

/// Verifies the detached base64 signature `signature_b64` over exactly
/// `manifest_bytes`, then parses and validates the manifest.
///
/// Uses strict verification (rejects non-canonical and small-order
/// signatures).
pub fn verify_manifest(
    manifest_bytes: &[u8],
    signature_b64: &str,
    key: &VerifyingKey,
) -> Result<ReleaseManifest> {
    let sig_bytes = BASE64
        .decode(signature_b64.trim())
        .map_err(|e| Error::MalformedSignature(format!("not base64: {e}")))?;
    let sig_array: [u8; 64] = sig_bytes.as_slice().try_into().map_err(|_| {
        Error::MalformedSignature(format!("{} bytes, expected 64", sig_bytes.len()))
    })?;
    let signature = Signature::from_bytes(&sig_array);
    key.verify_strict(manifest_bytes, &signature)
        .map_err(|_| Error::SignatureMismatch)?;
    let manifest: ReleaseManifest = serde_json::from_slice(manifest_bytes)
        .map_err(|e| Error::InvalidManifest(format!("not valid JSON: {e}")))?;
    manifest.validate()?;
    Ok(manifest)
}

/// Signs `manifest_bytes` and returns the base64 signature text written to
/// `manifest.json.sig` (with a trailing newline).
pub fn sign_manifest(manifest_bytes: &[u8], key: &SigningKey) -> String {
    let sig = key.sign(manifest_bytes);
    let mut s = BASE64.encode(sig.to_bytes());
    s.push('\n');
    s
}

/// True for exactly 64 ASCII hex digits.
pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Shape check for an RFC 3339 timestamp: `YYYY-MM-DDTHH:MM:SS`, optional
/// fraction, then `Z` or `±HH:MM`. Does not check calendar validity beyond
/// field ranges.
pub fn looks_like_rfc3339(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 20 {
        return false;
    }
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    let num = |r: std::ops::Range<usize>| -> u32 {
        b[r].iter().fold(0, |acc, d| acc * 10 + u32::from(d - b'0'))
    };
    if !(digits(0..4)
        && b[4] == b'-'
        && digits(5..7)
        && b[7] == b'-'
        && digits(8..10)
        && matches!(b[10], b'T' | b't')
        && digits(11..13)
        && b[13] == b':'
        && digits(14..16)
        && b[16] == b':'
        && digits(17..19))
    {
        return false;
    }
    if !(1..=12).contains(&num(5..7))
        || !(1..=31).contains(&num(8..10))
        || num(11..13) > 23
        || num(14..16) > 59
        || num(17..19) > 60
    {
        return false;
    }
    let mut i = 19;
    if b[i] == b'.' {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    match &b[i..] {
        [b'Z' | b'z'] => true,
        [b'+' | b'-', h1, h2, b':', m1, m2] => [h1, h2, m1, m2].iter().all(|d| d.is_ascii_digit()),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn manifest(version: &str, channel: Channel) -> ReleaseManifest {
        ReleaseManifest {
            name: MANIFEST_NAME.into(),
            version: Version::parse(version).unwrap(),
            channel,
            published: "2026-10-02T12:00:00Z".into(),
            notes: "Fixes".into(),
            artifacts: vec![Artifact {
                arch: "aarch64".into(),
                url: "https://example.com/frameplayer-1.2.3-aarch64.zip".into(),
                sha256: "a".repeat(64),
                size: 10,
            }],
        }
    }

    fn signed(m: &ReleaseManifest, k: &SigningKey) -> (Vec<u8>, String) {
        let bytes = serde_json::to_vec_pretty(m).unwrap();
        let sig = sign_manifest(&bytes, k);
        (bytes, sig)
    }

    #[test]
    fn good_signature_verifies() {
        let k = key(1);
        let m = manifest("1.2.3", Channel::Stable);
        let (bytes, sig) = signed(&m, &k);
        let got = verify_manifest(&bytes, &sig, &k.verifying_key()).unwrap();
        assert_eq!(got, m);
    }

    #[test]
    fn tampered_manifest_is_rejected() {
        let k = key(1);
        let (bytes, sig) = signed(&manifest("1.2.3", Channel::Stable), &k);
        let tampered = String::from_utf8(bytes)
            .unwrap()
            .replace("example.com", "evil.example")
            .into_bytes();
        assert!(matches!(
            verify_manifest(&tampered, &sig, &k.verifying_key()),
            Err(Error::SignatureMismatch)
        ));
        // Even whitespace changes break the signature: it covers exact bytes.
        let mut spaced = serde_json::to_vec_pretty(&manifest("1.2.3", Channel::Stable)).unwrap();
        spaced.push(b'\n');
        assert!(matches!(
            verify_manifest(&spaced, &sig, &k.verifying_key()),
            Err(Error::SignatureMismatch)
        ));
    }

    #[test]
    fn wrong_key_is_rejected() {
        let (bytes, sig) = signed(&manifest("1.2.3", Channel::Stable), &key(1));
        assert!(matches!(
            verify_manifest(&bytes, &sig, &key(2).verifying_key()),
            Err(Error::SignatureMismatch)
        ));
    }

    #[test]
    fn malformed_signatures_are_rejected() {
        let k = key(1);
        let (bytes, _) = signed(&manifest("1.2.3", Channel::Stable), &k);
        for sig in ["", "not base64!!", "AAAA"] {
            assert!(
                matches!(
                    verify_manifest(&bytes, sig, &k.verifying_key()),
                    Err(Error::MalformedSignature(_))
                ),
                "{sig:?}"
            );
        }
    }

    #[test]
    fn signed_but_invalid_manifest_is_rejected() {
        let k = key(1);
        let mut m = manifest("1.2.3", Channel::Stable);
        m.artifacts[0].url = "http://example.com/x.zip".into();
        let (bytes, sig) = signed(&m, &k);
        assert!(matches!(
            verify_manifest(&bytes, &sig, &k.verifying_key()),
            Err(Error::RejectedUrl { .. })
        ));
        let mut m = manifest("1.2.3", Channel::Stable);
        m.name = "other".into();
        let (bytes, sig) = signed(&m, &k);
        assert!(matches!(
            verify_manifest(&bytes, &sig, &k.verifying_key()),
            Err(Error::InvalidManifest(_))
        ));
    }

    #[test]
    fn parses_documented_json_shape() {
        let json = r#"{"name":"frameplayer","version":"1.2.3","channel":"beta",
            "published":"2026-10-02T12:00:00+02:00","notes":"* item",
            "artifacts":[{"arch":"aarch64","url":"https://h.example/a.zip",
            "sha256":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","size":5}]}"#;
        let m: ReleaseManifest = serde_json::from_str(json).unwrap();
        m.validate().unwrap();
        assert_eq!(m.channel, Channel::Beta);
        assert_eq!(m.version, Version::new(1, 2, 3));
    }

    #[test]
    fn semver_and_channel_selection() {
        let cur = Version::parse("1.2.3").unwrap();
        let pick = |v: &str, ch: Channel, user: Channel| {
            select_update(&manifest(v, ch), &cur, user, "aarch64")
                .unwrap()
                .map(|u| u.version.to_string())
        };
        // Newer stable: offered on both channels.
        assert_eq!(
            pick("1.2.4", Channel::Stable, Channel::Stable).as_deref(),
            Some("1.2.4")
        );
        assert_eq!(
            pick("1.10.0", Channel::Stable, Channel::Beta).as_deref(),
            Some("1.10.0")
        );
        // Same or older: never offered.
        assert_eq!(pick("1.2.3", Channel::Stable, Channel::Stable), None);
        assert_eq!(
            pick("1.2.3+build.7", Channel::Stable, Channel::Stable),
            None
        );
        assert_eq!(pick("1.2.2", Channel::Beta, Channel::Beta), None);
        assert_eq!(pick("0.9.0", Channel::Stable, Channel::Stable), None);
        // Beta releases only for beta users.
        assert_eq!(pick("1.3.0", Channel::Beta, Channel::Stable), None);
        assert_eq!(
            pick("1.3.0", Channel::Beta, Channel::Beta).as_deref(),
            Some("1.3.0")
        );
        // Pre-release versions are beta-only even if mislabelled stable.
        assert_eq!(pick("1.3.0-rc.1", Channel::Stable, Channel::Stable), None);
        assert_eq!(
            pick("1.3.0-rc.1", Channel::Beta, Channel::Beta).as_deref(),
            Some("1.3.0-rc.1")
        );
        // A pre-release is older than its release.
        let rc_user = Version::parse("1.3.0-rc.1").unwrap();
        let m = manifest("1.3.0", Channel::Stable);
        assert!(
            select_update(&m, &rc_user, Channel::Beta, "aarch64")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn missing_arch_is_an_error() {
        let cur = Version::parse("1.0.0").unwrap();
        let m = manifest("2.0.0", Channel::Stable);
        assert!(matches!(
            select_update(&m, &cur, Channel::Stable, "x86_64"),
            Err(Error::NoArtifact { .. })
        ));
    }

    #[test]
    fn rfc3339_shapes() {
        for ok in [
            "2026-10-02T12:00:00Z",
            "2026-10-02T12:00:00.123Z",
            "2026-10-02T12:00:00+02:00",
            "2026-10-02t12:00:00-05:30",
        ] {
            assert!(looks_like_rfc3339(ok), "{ok}");
        }
        for bad in [
            "2026-10-02",
            "2026-13-02T12:00:00Z",
            "2026-10-02 12:00:00Z",
            "2026-10-02T12:00:00",
            "2026-10-02T12:00:00.Z",
            "2026-10-02T25:00:00Z",
        ] {
            assert!(!looks_like_rfc3339(bad), "{bad}");
        }
    }

    #[test]
    fn weak_public_key_rejected() {
        // The identity point (y = 1) is small-order.
        let mut identity = [0u8; 32];
        identity[0] = 1;
        assert!(public_key_from_bytes(&identity).is_err());
        assert!(public_key_from_bytes(key(3).verifying_key().as_bytes()).is_ok());
    }
}
