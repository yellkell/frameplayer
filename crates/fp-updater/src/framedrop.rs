//! The community installers' manifest (`framedrop.install/v1`) and install
//! links.
//!
//! Format and rules as documented by Frame Control (`docs/web-install.md`):
//!
//! ```json
//! { "schema": "framedrop.install/v1", "name": "My Game",
//!   "files": [ { "url": "https://...", "sha256": "<64 hex>", "size": 123,
//!                "exe": "path/inside/zip" } ] }
//! ```
//!
//! Required: `schema`, `files[0].url`. Optional: `name` (at most 120
//! characters), `sha256`, `size`, `exe` (for `.zip` titles). The whole
//! document is at most 256 KiB. URLs must be `https` to a public host with
//! no credentials, and name a `.apk`, `.zip` or `.exe` file of at most
//! 4 GiB.

use serde::{Deserialize, Serialize};

use crate::url::{check_public_https_url, percent_encode_component};

/// Value of the `schema` field.
pub const FRAMEDROP_SCHEMA: &str = "framedrop.install/v1";
/// Largest accepted manifest document, in bytes.
pub const MAX_MANIFEST_BYTES: usize = 256 * 1024;
/// Largest accepted file, in bytes.
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Longest accepted `name`, in characters.
pub const MAX_NAME_CHARS: usize = 120;
/// File extensions the installers accept.
pub const ALLOWED_EXTENSIONS: [&str; 3] = [".apk", ".zip", ".exe"];

/// A `framedrop.install/v1` manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FramedropManifest {
    /// Must be [`FRAMEDROP_SCHEMA`].
    #[serde(default)]
    pub schema: String,
    /// Display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Files to install; the first is the title itself.
    #[serde(default)]
    pub files: Vec<FramedropFile>,
}

/// One file entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FramedropFile {
    /// Download URL (https, public host).
    #[serde(default)]
    pub url: String,
    /// Hex SHA-256 of the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Size in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// For `.zip` titles: path inside the zip of the program to launch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exe: Option<String>,
}

/// Why a framedrop manifest was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FramedropError {
    /// Document larger than [`MAX_MANIFEST_BYTES`].
    #[error("manifest is {0} bytes; the limit is 256 KiB")]
    TooLarge(usize),
    /// Not JSON, or the wrong JSON types.
    #[error("manifest is not valid JSON: {0}")]
    NotJson(String),
    /// `schema` missing or different.
    #[error("schema is {0:?}, expected \"framedrop.install/v1\"")]
    WrongSchema(String),
    /// `files` missing or empty.
    #[error("files is missing or empty")]
    NoFiles,
    /// `name` present but empty or blank.
    #[error("name is empty")]
    EmptyName,
    /// `name` longer than [`MAX_NAME_CHARS`].
    #[error("name is {0} characters; the limit is 120")]
    NameTooLong(usize),
    /// A file has no URL.
    #[error("files[{0}].url is missing")]
    MissingUrl(usize),
    /// A file URL breaks a rule.
    #[error("files[{index}].url {reason}")]
    BadUrl {
        /// Index into `files`.
        index: usize,
        /// Which rule was broken.
        reason: String,
    },
    /// `sha256` is not 64 hex digits.
    #[error("files[{0}].sha256 is not 64 hex digits")]
    BadSha256(usize),
    /// `size` over [`MAX_FILE_BYTES`].
    #[error("files[{index}].size {size} exceeds 4 GiB")]
    TooBig {
        /// Index into `files`.
        index: usize,
        /// Declared size.
        size: u64,
    },
    /// `exe` is malformed or used on a non-zip file.
    #[error("files[{index}].exe {reason}")]
    BadExe {
        /// Index into `files`.
        index: usize,
        /// Which rule was broken.
        reason: String,
    },
}

/// Lower-cased URL path with query and fragment removed.
fn url_path_lower(url: &str) -> String {
    let end = url.find(['?', '#']).unwrap_or(url.len());
    url[..end].to_ascii_lowercase()
}

impl FramedropManifest {
    /// A one-file manifest for a `.zip` title.
    pub fn for_zip(name: &str, url: &str, sha256: &str, size: u64, exe: &str) -> Self {
        FramedropManifest {
            schema: FRAMEDROP_SCHEMA.into(),
            name: Some(name.into()),
            files: vec![FramedropFile {
                url: url.into(),
                sha256: Some(sha256.to_ascii_lowercase()),
                size: Some(size),
                exe: Some(exe.into()),
            }],
        }
    }

    /// Checks every rule except the document-size limit (see
    /// [`FramedropManifest::to_json`] and [`parse_framedrop_manifest`],
    /// which check that too).
    pub fn validate(&self) -> Result<(), FramedropError> {
        if self.schema != FRAMEDROP_SCHEMA {
            return Err(FramedropError::WrongSchema(self.schema.clone()));
        }
        if let Some(name) = &self.name {
            if name.trim().is_empty() {
                return Err(FramedropError::EmptyName);
            }
            let n = name.chars().count();
            if n > MAX_NAME_CHARS {
                return Err(FramedropError::NameTooLong(n));
            }
        }
        if self.files.is_empty() {
            return Err(FramedropError::NoFiles);
        }
        for (index, f) in self.files.iter().enumerate() {
            if f.url.is_empty() {
                return Err(FramedropError::MissingUrl(index));
            }
            let bad_url = |reason: String| FramedropError::BadUrl { index, reason };
            check_public_https_url(&f.url).map_err(bad_url)?;
            let path = url_path_lower(&f.url);
            if !ALLOWED_EXTENSIONS.iter().any(|ext| path.ends_with(ext)) {
                return Err(bad_url("must point to a .apk, .zip or .exe file".into()));
            }
            if let Some(h) = &f.sha256 {
                if !crate::manifest::is_sha256_hex(h) {
                    return Err(FramedropError::BadSha256(index));
                }
            }
            if let Some(size) = f.size {
                if size > MAX_FILE_BYTES {
                    return Err(FramedropError::TooBig { index, size });
                }
            }
            if let Some(exe) = &f.exe {
                let bad_exe = |reason: &str| FramedropError::BadExe {
                    index,
                    reason: reason.into(),
                };
                if !path.ends_with(".zip") {
                    return Err(bad_exe("is only allowed for .zip files"));
                }
                if exe.is_empty() || exe.len() > 1024 {
                    return Err(bad_exe("must be 1 to 1024 characters"));
                }
                if exe.starts_with('/') || exe.contains('\\') || exe.contains('\0') {
                    return Err(bad_exe("must be a relative path inside the zip"));
                }
                if exe
                    .split('/')
                    .any(|c| c.is_empty() || c == "." || c == "..")
                {
                    return Err(bad_exe("must not contain empty, '.' or '..' components"));
                }
            }
        }
        Ok(())
    }

    /// Validates and serialises (pretty JSON with a trailing newline),
    /// enforcing the 256 KiB limit.
    pub fn to_json(&self) -> Result<String, FramedropError> {
        self.validate()?;
        let mut s = serde_json::to_string_pretty(self)
            .map_err(|e| FramedropError::NotJson(e.to_string()))?;
        s.push('\n');
        if s.len() > MAX_MANIFEST_BYTES {
            return Err(FramedropError::TooLarge(s.len()));
        }
        Ok(s)
    }
}

/// Parses and validates a manifest document exactly as an installer would.
pub fn parse_framedrop_manifest(bytes: &[u8]) -> Result<FramedropManifest, FramedropError> {
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(FramedropError::TooLarge(bytes.len()));
    }
    let m: FramedropManifest =
        serde_json::from_slice(bytes).map_err(|e| FramedropError::NotJson(e.to_string()))?;
    m.validate()?;
    Ok(m)
}

/// `frame-control://install?manifest=<url-encoded manifest URL>`
/// (format verified against Frame Control's documentation).
pub fn frame_control_install_link(manifest_url: &str) -> String {
    format!(
        "frame-control://install?manifest={}",
        percent_encode_component(manifest_url)
    )
}

/// `framedrop://install?manifest=<url-encoded manifest URL>`.
///
/// UNVERIFIED: FrameDrop's own link scheme has not been confirmed; this
/// mirrors Frame Control's. Check it against FrameDrop before publishing.
pub fn framedrop_install_link_unverified(manifest_url: &str) -> String {
    format!(
        "framedrop://install?manifest={}",
        percent_encode_component(manifest_url)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn good() -> FramedropManifest {
        FramedropManifest::for_zip(
            "FramePlayer",
            "https://github.com/yellkell/frameplayer/releases/download/v1.0.0/frameplayer-1.0.0-aarch64.zip",
            SHA,
            12345,
            "frameplayer/frameplayer.sh",
        )
    }

    fn err_of(m: &FramedropManifest) -> FramedropError {
        m.validate().unwrap_err()
    }

    #[test]
    fn good_manifest_round_trips() {
        let m = good();
        let json = m.to_json().unwrap();
        assert_eq!(parse_framedrop_manifest(json.as_bytes()).unwrap(), m);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["schema"], "framedrop.install/v1");
        assert_eq!(v["files"][0]["exe"], "frameplayer/frameplayer.sh");
    }

    #[test]
    fn minimal_manifest_is_valid() {
        let json =
            br#"{"schema":"framedrop.install/v1","files":[{"url":"https://example.com/a.APK"}]}"#;
        let m = parse_framedrop_manifest(json).unwrap();
        assert_eq!(m.name, None);
    }

    #[test]
    fn required_fields() {
        assert!(matches!(
            parse_framedrop_manifest(br#"{"files":[{"url":"https://example.com/a.zip"}]}"#),
            Err(FramedropError::WrongSchema(_))
        ));
        assert!(matches!(
            parse_framedrop_manifest(br#"{"schema":"framedrop.install/v2","files":[{"url":"https://example.com/a.zip"}]}"#),
            Err(FramedropError::WrongSchema(_))
        ));
        assert_eq!(
            parse_framedrop_manifest(br#"{"schema":"framedrop.install/v1"}"#),
            Err(FramedropError::NoFiles)
        );
        assert_eq!(
            parse_framedrop_manifest(br#"{"schema":"framedrop.install/v1","files":[]}"#),
            Err(FramedropError::NoFiles)
        );
        assert_eq!(
            parse_framedrop_manifest(br#"{"schema":"framedrop.install/v1","files":[{}]}"#),
            Err(FramedropError::MissingUrl(0))
        );
        assert!(matches!(
            parse_framedrop_manifest(b"not json"),
            Err(FramedropError::NotJson(_))
        ));
        assert!(matches!(
            parse_framedrop_manifest(br#"{"schema":"framedrop.install/v1","files":[{"url":"https://example.com/a.zip","size":"big"}]}"#),
            Err(FramedropError::NotJson(_))
        ));
    }

    #[test]
    fn name_rules() {
        let mut m = good();
        m.name = Some("é".repeat(120));
        assert!(m.validate().is_ok(), "120 characters (240 bytes) is fine");
        m.name = Some("x".repeat(121));
        assert_eq!(err_of(&m), FramedropError::NameTooLong(121));
        m.name = Some("  ".into());
        assert_eq!(err_of(&m), FramedropError::EmptyName);
    }

    #[test]
    fn url_rules() {
        let cases = [
            "http://example.com/a.zip",
            "https://user:pw@example.com/a.zip",
            "https://192.168.1.10/a.zip",
            "https://localhost/a.zip",
            "https://nas.local/a.zip",
            "https://example.com/a.tar.gz",
            "https://example.com/a.zip.txt",
            "https://example.com/",
        ];
        for url in cases {
            let mut m = good();
            m.files[0].url = url.into();
            m.files[0].exe = None;
            assert!(
                matches!(m.validate(), Err(FramedropError::BadUrl { index: 0, .. })),
                "{url}"
            );
        }
        let mut m = good();
        m.files[0].url = "https://example.com/dl/a.ZIP?token=1#x".into();
        assert!(m.validate().is_ok());
        // Rules apply to every file, not just the first.
        m.files.push(FramedropFile {
            url: "https://10.0.0.1/b.apk".into(),
            sha256: None,
            size: None,
            exe: None,
        });
        assert!(matches!(
            m.validate(),
            Err(FramedropError::BadUrl { index: 1, .. })
        ));
    }

    #[test]
    fn sha_and_size_rules() {
        let mut m = good();
        m.files[0].sha256 = Some("abc".into());
        assert_eq!(err_of(&m), FramedropError::BadSha256(0));
        m.files[0].sha256 = Some("g".repeat(64));
        assert_eq!(err_of(&m), FramedropError::BadSha256(0));
        m.files[0].sha256 = Some(SHA.to_uppercase());
        assert!(m.validate().is_ok());
        m.files[0].size = Some(MAX_FILE_BYTES);
        assert!(m.validate().is_ok());
        m.files[0].size = Some(MAX_FILE_BYTES + 1);
        assert!(matches!(err_of(&m), FramedropError::TooBig { .. }));
    }

    #[test]
    fn exe_rules() {
        for bad in ["", "/abs/run.sh", "../run.sh", "a/../b", "a\\b", "a//b"] {
            let mut m = good();
            m.files[0].exe = Some(bad.into());
            assert!(
                matches!(err_of(&m), FramedropError::BadExe { .. }),
                "{bad:?}"
            );
        }
        let mut m = good();
        m.files[0].url = "https://example.com/a.apk".into();
        assert!(matches!(err_of(&m), FramedropError::BadExe { .. }));
    }

    #[test]
    fn size_limit() {
        let mut big = br#"{"schema":"framedrop.install/v1","files":[{"url":"https://example.com/a.zip"}],"pad":""#.to_vec();
        big.extend(std::iter::repeat_n(b'x', MAX_MANIFEST_BYTES));
        big.extend(b"\"}");
        assert!(matches!(
            parse_framedrop_manifest(&big),
            Err(FramedropError::TooLarge(_))
        ));
        let mut m = good();
        m.files = (0..3000).map(|_| good().files[0].clone()).collect();
        assert!(matches!(m.to_json(), Err(FramedropError::TooLarge(_))));
    }

    #[test]
    fn install_links() {
        let url = "https://example.com/fp/framedrop.json?v=1";
        assert_eq!(
            frame_control_install_link(url),
            "frame-control://install?manifest=https%3A%2F%2Fexample.com%2Ffp%2Fframedrop.json%3Fv%3D1"
        );
        assert!(
            framedrop_install_link_unverified(url)
                .starts_with("framedrop://install?manifest=https%3A%2F%2F")
        );
    }
}
