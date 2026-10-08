//! In-app self-update for FramePlayer.
//!
//! The update flow:
//!
//! 1. [`check`] fetches the release manifest (`manifest.json`) and its
//!    detached ed25519 signature (`manifest.json.sig`), verifies the
//!    signature against [`RELEASE_PUBLIC_KEY`], and compares versions with
//!    semver for the user's [`Channel`].
//! 2. [`download`] streams the zip to disk with resume (HTTP `Range`) and
//!    verifies size and SHA-256 from the signed manifest.
//! 3. [`install`] extracts into a staging directory beside the install
//!    directory, validates it, and swaps it in with renames, keeping one
//!    previous version for [`rollback`].
//!
//! The crate also carries the [`framedrop`] manifest used by the community
//! installers (Frame Control, FrameDrop) and the `fp-release` maintainer
//! tool that generates keys, manifests and signatures.

pub mod client;
pub mod error;
pub mod framedrop;
pub mod install;
pub mod manifest;
pub mod url;

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

pub use client::{Updater, agent_with_proxy, default_agent, download_with, hash_file};
pub use ed25519_dalek::{SigningKey, VerifyingKey};
pub use error::{Error, Result};
pub use install::{
    ArchiveInfo, extract_zip, inspect_zip, install, install_expecting, installed_version, rollback,
    validate_install_dir,
};
pub use manifest::{
    Artifact, Channel, ReleaseManifest, Update, select_update, sign_manifest, verify_manifest,
};
pub use semver::Version;

/// Where the latest stable release manifest is published. The signature is
/// expected at the same URL plus `.sig`.
pub const DEFAULT_MANIFEST_URL: &str =
    "https://github.com/yellkell/frameplayer/releases/latest/download/manifest.json";

/// The ed25519 public key release manifests are signed with.
///
/// `None` in builds made before a release key exists: updates are then
/// refused with [`Error::NoPublicKey`]. To set it, run
/// `fp-release keygen`, keep the private key offline, and paste the line it
/// prints here.
pub const RELEASE_PUBLIC_KEY: Option<[u8; 32]> = Some([
    0xe3, 0xc3, 0xfc, 0x45, 0xf0, 0x2f, 0x39, 0xc5, 0xc7, 0xc1, 0x05, 0x48, 0x56, 0x6e, 0xd0, 0x4c,
    0x15, 0xb5, 0x57, 0x5f, 0x22, 0x21, 0xeb, 0x4e, 0xbc, 0xcc, 0xba, 0xb3, 0xee, 0x00, 0xde, 0x2f,
]);

/// The compiled-in release key, parsed.
pub fn release_key() -> Result<VerifyingKey> {
    match RELEASE_PUBLIC_KEY {
        Some(bytes) => manifest::public_key_from_bytes(&bytes),
        None => Err(Error::NoPublicKey),
    }
}

/// Parses a semver version string (surrounding whitespace allowed).
pub fn parse_version(s: &str) -> Result<Version> {
    Version::parse(s.trim()).map_err(|e| Error::InvalidManifest(format!("bad version {s:?}: {e}")))
}

/// Checks `url` for an update newer than `current_version` on `channel`,
/// for this CPU architecture, trusting [`RELEASE_PUBLIC_KEY`].
///
/// Returns `Ok(None)` when up to date. Unsigned or badly signed manifests
/// are errors, never "no update".
pub fn check(url: &str, current_version: &str, channel: Channel) -> Result<Option<Update>> {
    let current = parse_version(current_version)?;
    Updater::new()?.check(url, &current, channel)
}

/// Downloads `update` into the directory `dest` with resume and SHA-256
/// verification. See [`Updater::download`].
pub fn download(
    update: &Update,
    dest: &Path,
    progress: impl FnMut(u64, u64),
    cancel: &AtomicBool,
) -> Result<PathBuf> {
    download_with(&default_agent(), update, dest, progress, cancel)
}

/// The directory holding the running executable, which is the install
/// directory for a FramePlayer started from `frameplayer.sh`.
pub fn current_install_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe().map_err(|e| Error::io("cannot locate", "current exe", e))?;
    exe.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| Error::InvalidInstall("executable has no parent directory".into()))
}
