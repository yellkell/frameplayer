//! FramePlayer self-updater (OUTLINE §2.2 "in-app self-update", §3.6 security).
//!
//! The pieces, in the order an update flows through them:
//!
//! 1. [`manifest`]: the release manifest (`stable.json` / `beta.json`) that
//!    lists the newest version per channel with per-arch artifacts, SHA-256
//!    digests and optional delta patches.
//! 2. [`signing`]: every manifest is published with a detached ed25519
//!    signature (`<manifest>.sig`). The verifying key is compiled in; the
//!    release tool uses [`signing::generate_keypair`] / [`signing::sign`].
//! 3. [`download`]: resumable HTTP download (Range requests) into a staging
//!    directory, with SHA-256 verification.
//! 4. [`delta`]: an rsync-style block delta between two uncompressed release
//!    tarballs, so small releases are small downloads.
//! 5. [`archive`]: tarball extraction (`.tar`, `.tar.gz`, `.tar.zst`) that
//!    refuses path traversal, absolute paths, devices and escaping links.
//! 6. [`layout`]: the on-disk install layout (`versions/<ver>`, `current`
//!    symlink switched atomically with `rename(2)`, `previous` kept for
//!    rollback) plus the first-launch health check that rolls back a version
//!    which never marks itself healthy.
//! 7. [`updater`]: the orchestrator the app talks to ([`Updater`]).
//!
//! The same tarball is used by every install tier: Frame Control / FrameDrop
//! extract it flat into `~/devkit-game/frameplayer`, our `frameplayer-install`
//! CLI does the same over SSH, and the in-app updater installs it into the
//! versioned layout. `dist/frameplayer.sh` (the launcher inside the tarball)
//! and [`layout`] agree on file names so all three paths interoperate.

pub mod archive;
pub mod delta;
pub mod download;
pub mod layout;
pub mod manifest;
pub mod platform;
pub mod signing;
pub mod updater;

pub use layout::{BootOutcome, HealthPolicy, InstallLayout, LaunchGuard};
pub use manifest::{Artifact, Channel, DeltaPatch, ReleaseManifest};
pub use signing::ManifestVerifier;
pub use updater::{
    DownloadProgress, StagedUpdate, UpdateCheck, UpdatePlan, Updater, UpdaterConfig,
};

/// Errors produced anywhere in the update pipeline.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("unexpected HTTP status {status} for {url}")]
    HttpStatus { status: u16, url: String },
    #[error("manifest is not valid JSON: {0}")]
    ManifestJson(#[from] serde_json::Error),
    #[error("invalid manifest: {0}")]
    InvalidManifest(String),
    #[error("signature verification failed: {0}")]
    BadSignature(String),
    #[error("SHA-256 mismatch for {what}: expected {expected}, got {actual}")]
    HashMismatch {
        what: String,
        expected: String,
        actual: String,
    },
    #[error("size mismatch for {what}: expected {expected} bytes, got {actual}")]
    SizeMismatch {
        what: String,
        expected: u64,
        actual: u64,
    },
    #[error("unsafe archive entry {path:?}: {reason}")]
    UnsafeArchive { path: String, reason: String },
    #[error("invalid delta patch: {0}")]
    BadDelta(String),
    #[error("install layout error: {0}")]
    Layout(String),
    #[error("no artifact for architecture {0}")]
    NoArtifact(String),
}

pub type Result<T, E = UpdateError> = std::result::Result<T, E>;

/// Lower-case hex SHA-256 of a byte slice.
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(data))
}

/// Lower-case hex SHA-256 of a file, streamed.
pub fn sha256_file(path: &std::path::Path) -> Result<String> {
    use sha2::Digest;
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Compare a computed digest against an expected one (case-insensitive).
pub fn check_sha256(what: &str, expected: &str, actual: &str) -> Result<()> {
    if expected.eq_ignore_ascii_case(actual) {
        Ok(())
    } else {
        Err(UpdateError::HashMismatch {
            what: what.to_string(),
            expected: expected.to_ascii_lowercase(),
            actual: actual.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha256_file_matches_slice() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, b"hello world").unwrap();
        assert_eq!(sha256_file(&p).unwrap(), sha256_hex(b"hello world"));
    }

    #[test]
    fn check_sha256_case_insensitive() {
        assert!(check_sha256("x", "ABCD", "abcd").is_ok());
        assert!(matches!(
            check_sha256("x", "abcd", "abce"),
            Err(UpdateError::HashMismatch { .. })
        ));
    }
}
