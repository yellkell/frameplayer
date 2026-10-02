//! Release tooling used by `tools/release.sh` and `.github/workflows/release.yml`
//! through the `frameplayer-install` subcommands `gen-key`, `make-manifest`,
//! `make-delta`, `sign-manifest`, `verify-manifest` and `site-manifest`.

use crate::site_manifest::SiteManifest;
use anyhow::{bail, Context, Result};
use fp_updater::archive;
use fp_updater::manifest::{
    ArchiveFormat, Artifact, Channel, DeltaPatch, ReleaseManifest, MANIFEST_SCHEMA, PACKAGE_NAME,
};
use fp_updater::signing::{self, ManifestVerifier};
use std::path::{Path, PathBuf};
use url::Url;

/// A delta patch to list in the manifest.
#[derive(Debug, Clone)]
pub struct DeltaInput {
    pub from: semver::Version,
    pub path: PathBuf,
    pub url: Url,
}

impl std::str::FromStr for DeltaInput {
    type Err = anyhow::Error;
    /// `FROM=PATH=URL`, e.g. `0.1.0=dist/out/0.2.0-from-0.1.0.fpd=https://…`.
    fn from_str(s: &str) -> Result<Self> {
        let mut it = s.splitn(3, '=');
        let (Some(from), Some(path), Some(url)) = (it.next(), it.next(), it.next()) else {
            bail!("--delta expects FROM=PATH=URL, got {s:?}");
        };
        Ok(Self {
            from: from.parse()?,
            path: path.into(),
            url: Url::parse(url)?,
        })
    }
}

/// Inputs for [`build_manifest`].
#[derive(Debug, Clone)]
pub struct ManifestInput {
    pub version: semver::Version,
    pub channel: Channel,
    pub arch: String,
    pub tarball: PathBuf,
    pub url: Url,
    pub min_steamos: Option<String>,
    pub notes: String,
    pub notes_url: Option<Url>,
    pub deltas: Vec<DeltaInput>,
}

/// Hash a tarball both packed and unpacked, returning the artifact entry.
pub fn describe_tarball(tarball: &Path, arch: &str, url: Url) -> Result<Artifact> {
    let format = archive::detect_format(tarball)?;
    let size = std::fs::metadata(tarball)?.len();
    let sha256 = fp_updater::sha256_file(tarball)?;
    let tmp = tempfile_in(tarball)?;
    let (unpacked_size, unpacked_sha256) = archive::decompress_to_tar(tarball, format, &tmp)?;
    let _ = std::fs::remove_file(&tmp);
    Ok(Artifact {
        arch: arch.to_string(),
        format,
        url,
        size,
        sha256,
        unpacked_size: Some(unpacked_size),
        unpacked_sha256: Some(unpacked_sha256),
        deltas: vec![],
    })
}

fn tempfile_in(near: &Path) -> Result<PathBuf> {
    let dir = near
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Ok(dir.join(format!(".fp-release-{}.tar", std::process::id())))
}

/// Assemble (but do not sign) a release manifest.
pub fn build_manifest(input: &ManifestInput) -> Result<ReleaseManifest> {
    let mut artifact = describe_tarball(&input.tarball, &input.arch, input.url.clone())?;
    for d in &input.deltas {
        artifact.deltas.push(DeltaPatch {
            from: d.from.clone(),
            url: d.url.clone(),
            size: std::fs::metadata(&d.path)
                .with_context(|| d.path.display().to_string())?
                .len(),
            sha256: fp_updater::sha256_file(&d.path)?,
        });
    }
    let m = ReleaseManifest {
        schema: MANIFEST_SCHEMA,
        name: PACKAGE_NAME.into(),
        version: input.version.clone(),
        channel: input.channel,
        published: chrono::Utc::now(),
        min_steamos: input.min_steamos.clone(),
        notes: input.notes.clone(),
        notes_url: input.notes_url.clone(),
        artifacts: vec![artifact],
    };
    m.validate()?;
    Ok(m)
}

/// Create a delta patch between two release tarballs (packed or plain).
pub fn make_delta(from: &Path, to: &Path, out: &Path) -> Result<u64> {
    let plain = |p: &Path, tag: &str| -> Result<(PathBuf, bool)> {
        match archive::detect_format(p)? {
            ArchiveFormat::Tar => Ok((p.to_path_buf(), false)),
            f => {
                let t = out.with_extension(format!("{tag}.tar"));
                archive::decompress_to_tar(p, f, &t)?;
                Ok((t, true))
            }
        }
    };
    let (a, ta) = plain(from, "from")?;
    let (b, tb) = plain(to, "to")?;
    let res = fp_updater::delta::create_files(&a, &b, out, fp_updater::delta::DEFAULT_BLOCK_SIZE);
    if ta {
        let _ = std::fs::remove_file(&a);
    }
    if tb {
        let _ = std::fs::remove_file(&b);
    }
    Ok(res?)
}

/// Resolve a signing key argument: either a path to a file holding the hex
/// key, or the hex itself. Returns the hex string (validated).
pub fn load_secret(hex_or_file: &str) -> Result<String> {
    let s = if Path::new(hex_or_file).is_file() {
        std::fs::read_to_string(hex_or_file)?
    } else {
        hex_or_file.to_string()
    };
    let s = s.trim().to_string();
    signing::parse_secret_key(&s)?;
    Ok(s)
}

/// Sign `manifest_path`, writing `<manifest>.sig`. Returns the signer's
/// public key hex and whether this build trusts it.
pub fn sign_manifest(manifest_path: &Path, secret_hex: &str) -> Result<(PathBuf, String, bool)> {
    let bytes = std::fs::read(manifest_path)?;
    ReleaseManifest::parse(&bytes).context("refusing to sign an invalid manifest")?;
    let sk = signing::parse_secret_key(secret_hex.trim())?;
    let sig = signing::sign(&sk, &bytes);
    let sig_path = sig_path(manifest_path);
    std::fs::write(&sig_path, &sig)?;
    let trusted = ManifestVerifier::default()
        .verify(&bytes, sig.as_bytes())
        .is_ok();
    Ok((sig_path, signing::public_key_hex(&sk), trusted))
}

pub fn sig_path(manifest_path: &Path) -> PathBuf {
    let mut s = manifest_path.as_os_str().to_owned();
    s.push(".sig");
    PathBuf::from(s)
}

/// Verify a manifest + signature with the compiled-in keys or `pubkey_hex`.
pub fn verify_manifest(
    manifest_path: &Path,
    sig: Option<&Path>,
    pubkey_hex: Option<&str>,
) -> Result<ReleaseManifest> {
    let bytes = std::fs::read(manifest_path)?;
    let sig_bytes = std::fs::read(
        sig.map(Path::to_path_buf)
            .unwrap_or_else(|| sig_path(manifest_path)),
    )?;
    let v = match pubkey_hex {
        Some(k) => ManifestVerifier::with_keys(signing::parse_public_keys(k)?),
        None => ManifestVerifier::default(),
    };
    Ok(v.verify_and_parse(&bytes, &sig_bytes)?)
}

/// Update the website manifest template for a new release.
pub fn update_site_manifest(
    template: &str,
    version: &str,
    tarball: &Path,
    url: Url,
) -> Result<SiteManifest> {
    let mut m: SiteManifest = serde_json::from_str(template)?;
    let size = std::fs::metadata(tarball)?.len();
    let sha = fp_updater::sha256_file(tarball)?;
    let published = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    m.set_release(version, url, &sha, size, &published);
    if let Some(notes) = &mut m.release_notes_url {
        if let Ok(u) = Url::parse(&format!(
            "https://github.com/yellkell/frameplayer/releases/tag/v{version}"
        )) {
            if notes.host_str() == Some("github.com") {
                *notes = u;
            }
        }
    }
    m.validate()?;
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_release_tarball(dir: &Path, version: &str, payload: &[u8]) -> PathBuf {
        let mut b = tar::Builder::new(Vec::new());
        for (path, data) in [
            ("frameplayer.sh".to_string(), b"#!/bin/sh\n".to_vec()),
            ("RELEASE".to_string(), format!("{version}\n").into_bytes()),
            (
                format!("versions/{version}/bin/frameplayer"),
                payload.to_vec(),
            ),
        ] {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o755);
            b.append_data(&mut h, path, data.as_slice()).unwrap();
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&b.into_inner().unwrap()).unwrap();
        let p = dir.join(format!("frameplayer-{version}-aarch64.tar.gz"));
        std::fs::write(&p, gz.finish().unwrap()).unwrap();
        p
    }

    #[test]
    fn manifest_sign_verify_pipeline() {
        let d = tempfile::tempdir().unwrap();
        let payload1: Vec<u8> = (0..50_000u32).map(|i| (i % 253) as u8).collect();
        let mut payload2 = payload1.clone();
        payload2[10] = 7;
        let t1 = write_release_tarball(d.path(), "0.1.0", &payload1);
        let t2 = write_release_tarball(d.path(), "0.2.0", &payload2);
        let patch = d.path().join("0.2.0-from-0.1.0.fpd");
        let size = make_delta(&t1, &t2, &patch).unwrap();
        assert!(size > 0 && size < 10_000);

        let input = ManifestInput {
            version: "0.2.0".parse().unwrap(),
            channel: Channel::Stable,
            arch: "aarch64".into(),
            tarball: t2.clone(),
            url: Url::parse("https://example.org/frameplayer-0.2.0-aarch64.tar.gz").unwrap(),
            min_steamos: Some("3.8".into()),
            notes: "notes".into(),
            notes_url: None,
            deltas: vec![
                format!("0.1.0={}=https://example.org/p.fpd", patch.display())
                    .parse()
                    .unwrap(),
            ],
        };
        let m = build_manifest(&input).unwrap();
        let a = &m.artifacts[0];
        assert_eq!(a.format, ArchiveFormat::TarGz);
        assert_eq!(a.deltas.len(), 1);
        let mpath = d.path().join("stable.json");
        std::fs::write(&mpath, m.to_json_pretty().unwrap()).unwrap();

        let (sk, pk) = signing::generate_keypair();
        let (sig, signer, trusted) = sign_manifest(&mpath, &sk).unwrap();
        assert_eq!(signer, pk);
        assert!(!trusted, "random key is not the compiled-in key");
        assert!(sig.exists());
        let back = verify_manifest(&mpath, None, Some(&pk)).unwrap();
        assert_eq!(back.version, m.version);
        assert!(verify_manifest(&mpath, None, None).is_err());

        // The patch reproduces the new tar exactly.
        let plain1 = d.path().join("1.tar");
        let plain2 = d.path().join("2.tar");
        archive::decompress_to_tar(&t1, ArchiveFormat::TarGz, &plain1).unwrap();
        fp_updater::delta::apply_files(&plain1, &patch, &plain2).unwrap();
        assert_eq!(
            fp_updater::sha256_file(&plain2).unwrap(),
            a.unpacked_sha256.clone().unwrap()
        );
    }

    #[test]
    fn site_manifest_update() {
        let d = tempfile::tempdir().unwrap();
        let t = write_release_tarball(d.path(), "0.3.0", b"bin");
        let url = Url::parse("https://github.com/yellkell/frameplayer/releases/download/v0.3.0/frameplayer-0.3.0-aarch64.tar.gz").unwrap();
        let m = update_site_manifest(
            include_str!("../../../dist/frameplayer.json"),
            "0.3.0",
            &t,
            url,
        )
        .unwrap();
        assert_eq!(m.version, "0.3.0");
        assert_eq!(m.sha256, fp_updater::sha256_file(&t).unwrap());
        assert!(m.release_notes_url.unwrap().as_str().ends_with("/v0.3.0"));
    }

    #[test]
    fn delta_input_parsing() {
        let d: DeltaInput = "0.1.0=a/b.fpd=https://x.org/b.fpd".parse().unwrap();
        assert_eq!(d.from.to_string(), "0.1.0");
        assert!("0.1.0=a".parse::<DeltaInput>().is_err());
    }
}
