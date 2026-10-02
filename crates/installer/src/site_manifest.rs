//! The website install manifest (`dist/frameplayer.json`) consumed by the
//! community installers: Frame Control (`frame-control://install?manifest=<url>`)
//! and FrameDrop ("Install with FrameDrop").
//!
//! [verify] Neither tool's manifest schema is formally documented. This
//! format is a deliberate superset: a structured core (`tarball`, `launch`,
//! `artwork`) plus flat aliases (`url`, `download_url`, `sha256`, `size`,
//! `launch_command`, `executable`) holding the same values, so whichever
//! spelling a tool reads, it finds a correct value. [`SiteManifest::validate`]
//! enforces that aliases never drift from the core fields. The JSON Schema
//! lives at `dist/frameplayer.schema.json`; field docs in docs/adr/0003.

use anyhow::{bail, ensure, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use url::Url;

pub const SITE_MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tarball {
    pub url: Url,
    pub sha256: String,
    pub size: u64,
    /// `tar.gz` or `tar.zst`.
    #[serde(default = "default_format")]
    pub format: String,
}

fn default_format() -> String {
    "tar.gz".into()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Launch {
    /// Relative to the install dir (`~/devkit-game/<install_dir>`).
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_workdir")]
    pub working_dir: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

fn default_workdir() -> String {
    ".".into()
}

/// One Steam artwork image: a URL for tools that download art, and the path
/// inside the installed tree for tools that read it from disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtworkImage {
    pub url: Url,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artwork {
    /// Portrait capsule 600×900.
    pub grid: ArtworkImage,
    /// Landscape capsule 920×430.
    pub grid_horizontal: ArtworkImage,
    /// Hero 3840×1240.
    pub hero: ArtworkImage,
    /// Transparent logo 1280×720 max.
    pub logo: ArtworkImage,
    /// Square icon 256×256.
    pub icon: ArtworkImage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteManifest {
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub manifest_version: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub homepage: Url,
    pub license: String,
    pub platform: String,
    pub arch: String,
    /// Directory name under `~/devkit-game/`.
    pub install_dir: String,
    pub tarball: Tarball,
    pub launch: Launch,
    pub artwork: Artwork,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_steamos: Option<String>,
    /// Signed in-app update manifest (fp-updater format).
    pub update_manifest: Url,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_notes_url: Option<Url>,
    pub published: String,
    // ---- flat aliases (must equal the structured fields) ----
    pub url: Url,
    pub download_url: Url,
    pub sha256: String,
    pub size: u64,
    pub launch_command: String,
    pub executable: String,
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

impl SiteManifest {
    pub fn parse(json: &str) -> Result<Self> {
        let m: Self = serde_json::from_str(json)?;
        m.validate()?;
        Ok(m)
    }

    pub fn to_json_pretty(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)? + "\n")
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.manifest_version == SITE_MANIFEST_VERSION,
            "unsupported manifest_version"
        );
        ensure!(
            semver::Version::parse(&self.version).is_ok(),
            "version {:?} is not semver",
            self.version
        );
        ensure!(self.arch == "aarch64", "arch must be aarch64 (Steam Frame)");
        ensure!(self.platform == "linux", "platform must be linux");
        ensure!(
            is_hex64(&self.tarball.sha256),
            "tarball.sha256 must be 64 hex chars"
        );
        ensure!(
            self.tarball.url.scheme() == "https",
            "tarball.url must be https"
        );
        ensure!(self.tarball.size > 0, "tarball.size must be > 0");
        ensure!(
            matches!(self.tarball.format.as_str(), "tar.gz" | "tar.zst"),
            "tarball.format"
        );
        ensure!(
            !self.install_dir.is_empty()
                && self
                    .install_dir
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)),
            "install_dir must be a plain directory name"
        );
        for (what, p) in [
            ("launch.command", &self.launch.command),
            ("executable", &self.executable),
        ] {
            ensure!(
                !p.starts_with('/') && !p.contains(".."),
                "{what} must be relative to the install dir"
            );
        }
        // Aliases.
        ensure!(
            self.url == self.tarball.url && self.download_url == self.tarball.url,
            "url aliases differ from tarball.url"
        );
        ensure!(
            self.sha256.eq_ignore_ascii_case(&self.tarball.sha256),
            "sha256 alias differs"
        );
        ensure!(self.size == self.tarball.size, "size alias differs");
        ensure!(
            self.launch_command == self.launch.command,
            "launch_command alias differs"
        );
        ensure!(
            self.executable == self.launch.command.trim_start_matches("./"),
            "executable alias differs from launch.command"
        );
        let a = &self.artwork;
        for img in [&a.grid, &a.grid_horizontal, &a.hero, &a.logo, &a.icon] {
            ensure!(img.url.scheme() == "https", "artwork URLs must be https");
            if img.path.starts_with('/') || img.path.contains("..") {
                bail!("artwork path {:?} must be relative", img.path);
            }
        }
        Ok(())
    }

    /// Point the manifest at a new release, keeping aliases in sync and
    /// rewriting `versions/<old>/` artwork paths.
    pub fn set_release(
        &mut self,
        version: &str,
        url: Url,
        sha256: &str,
        size: u64,
        published: &str,
    ) {
        let old = format!("versions/{}/", self.version);
        let new = format!("versions/{version}/");
        let a = &mut self.artwork;
        for img in [
            &mut a.grid,
            &mut a.grid_horizontal,
            &mut a.hero,
            &mut a.logo,
            &mut a.icon,
        ] {
            img.path = img.path.replace(&old, &new);
        }
        self.version = version.to_string();
        self.tarball.url = url.clone();
        self.tarball.sha256 = sha256.to_ascii_lowercase();
        self.tarball.size = size;
        self.url = url.clone();
        self.download_url = url;
        self.sha256 = self.tarball.sha256.clone();
        self.size = size;
        self.published = published.to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIST: &str = include_str!("../../../dist/frameplayer.json");
    const SCHEMA: &str = include_str!("../../../dist/frameplayer.schema.json");

    #[test]
    fn dist_manifest_is_valid() {
        let m = SiteManifest::parse(DIST).unwrap();
        assert_eq!(m.install_dir, crate::GAME_DIR);
        assert_eq!(m.launch.command, "./frameplayer.sh");
    }

    #[test]
    fn schema_requires_fields_present_in_manifest() {
        let schema: serde_json::Value = serde_json::from_str(SCHEMA).unwrap();
        let manifest: serde_json::Value = serde_json::from_str(DIST).unwrap();
        let req = schema["required"].as_array().unwrap();
        assert!(req.len() > 10);
        for k in req {
            let k = k.as_str().unwrap();
            assert!(manifest.get(k).is_some(), "manifest lacks required {k}");
            assert!(
                schema["properties"].get(k).is_some(),
                "schema lacks property {k}"
            );
        }
        // Every manifest key is described by the schema.
        for k in manifest.as_object().unwrap().keys() {
            assert!(schema["properties"].get(k).is_some(), "schema lacks {k}");
        }
    }

    #[test]
    fn set_release_keeps_aliases_in_sync() {
        let mut m = SiteManifest::parse(DIST).unwrap();
        let url = Url::parse("https://github.com/yellkell/frameplayer/releases/download/v9.9.9/frameplayer-9.9.9-aarch64.tar.gz").unwrap();
        m.set_release(
            "9.9.9",
            url.clone(),
            &"AB".repeat(32),
            1234,
            "2026-10-02T00:00:00Z",
        );
        m.validate().unwrap();
        assert_eq!(m.download_url, url);
        assert_eq!(m.sha256, "ab".repeat(32));
        assert!(m.artwork.hero.path.contains("versions/9.9.9/"));
    }

    #[test]
    fn drifted_alias_rejected() {
        let mut m = SiteManifest::parse(DIST).unwrap();
        m.size += 1;
        assert!(m.validate().is_err());
        let mut m = SiteManifest::parse(DIST).unwrap();
        m.launch_command = "/usr/bin/evil".into();
        assert!(m.validate().is_err());
        let mut m = SiteManifest::parse(DIST).unwrap();
        m.install_dir = "../x".into();
        assert!(m.validate().is_err());
    }
}
