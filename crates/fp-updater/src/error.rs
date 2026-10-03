//! Error type shared by every updater operation.

use std::io;
use std::path::PathBuf;

/// Everything that can go wrong while checking, downloading, installing or
/// rolling back an update.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The HTTP request itself failed (DNS, TLS, connection, timeout).
    #[error("network error fetching {url}: {source}")]
    Http {
        /// URL being fetched.
        url: String,
        /// Underlying client error.
        #[source]
        source: Box<ureq::Error>,
    },
    /// The server answered with a status we cannot use.
    #[error("server returned HTTP {status} for {url}")]
    HttpStatus {
        /// URL being fetched.
        url: String,
        /// HTTP status code.
        status: u16,
    },
    /// A file system operation failed.
    #[error("{action} {path}: {source}")]
    Io {
        /// What was being attempted, e.g. "cannot create".
        action: &'static str,
        /// File or directory involved.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// The release manifest parsed but is not acceptable.
    #[error("release manifest is not valid: {0}")]
    InvalidManifest(String),
    /// The detached signature is missing, not base64, or not 64 bytes.
    #[error("release manifest signature is malformed: {0}")]
    MalformedSignature(String),
    /// The signature does not verify against the trusted key.
    #[error("release manifest signature does not match the trusted release key")]
    SignatureMismatch,
    /// This build was compiled without a release public key.
    #[error("this build has no release public key, so updates are disabled")]
    NoPublicKey,
    /// A public or private key could not be decoded.
    #[error("invalid key: {0}")]
    InvalidKey(String),
    /// A URL was rejected before any request was made.
    #[error("refusing URL {url}: {reason}")]
    RejectedUrl {
        /// The rejected URL.
        url: String,
        /// Why it was rejected.
        reason: String,
    },
    /// The release has no artifact for the requested CPU architecture.
    #[error("release {version} has no build for {arch}")]
    NoArtifact {
        /// Release version.
        version: String,
        /// Requested architecture, e.g. `aarch64`.
        arch: String,
    },
    /// The server sent a different number of bytes than the manifest says.
    #[error("download of {url} has {actual} bytes, expected {expected}")]
    SizeMismatch {
        /// URL being downloaded.
        url: String,
        /// Size from the signed manifest.
        expected: u64,
        /// Bytes actually received.
        actual: u64,
    },
    /// The downloaded file's SHA-256 does not match the signed manifest.
    #[error("checksum mismatch for {path}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        /// File that failed verification (already deleted).
        path: PathBuf,
        /// Expected hex digest.
        expected: String,
        /// Actual hex digest.
        actual: String,
    },
    /// The caller set the cancel flag. The partial file is kept for resume.
    #[error("download cancelled")]
    Cancelled,
    /// The archive could not be read.
    #[error("cannot read archive: {0}")]
    Zip(#[from] zip::result::ZipError),
    /// An archive entry would land outside the extraction directory.
    #[error("archive entry has an unsafe path: {0:?}")]
    UnsafeArchivePath(String),
    /// An archive entry is a symbolic link or other special file.
    #[error("archive entry is a symbolic link or special file: {0:?}")]
    UnsupportedArchiveEntry(String),
    /// The archive exceeds the size or entry-count limits.
    #[error("archive is too large: {0}")]
    ArchiveTooLarge(String),
    /// The extracted tree is not a usable FramePlayer installation.
    #[error("not a valid FramePlayer build: {0}")]
    InvalidInstall(String),
    /// The archive's `VERSION` file names a different version than expected.
    #[error("archive contains version {found}, expected {expected}")]
    VersionMismatch {
        /// Version the caller expected.
        expected: String,
        /// Version found in the archive.
        found: String,
    },
    /// `rollback` was called but no previous version is kept.
    #[error("no previous version to roll back to ({0} does not exist)")]
    NoPreviousVersion(PathBuf),
}

/// Result alias for this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub(crate) fn io(action: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Error::Io {
            action,
            path: path.into(),
            source,
        }
    }
}

/// Extension to attach a path and action to `io::Result`s.
pub(crate) trait IoContext<T> {
    fn ctx(self, action: &'static str, path: &std::path::Path) -> Result<T>;
}

impl<T> IoContext<T> for io::Result<T> {
    fn ctx(self, action: &'static str, path: &std::path::Path) -> Result<T> {
        self.map_err(|e| Error::io(action, path, e))
    }
}
