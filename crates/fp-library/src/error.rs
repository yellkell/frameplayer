//! Error type shared by every part of the library crate.

use std::path::PathBuf;

use crate::record::{MediaId, PlaylistId};

/// Everything that can go wrong in the media library.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// SQLite reported an error.
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A file system operation failed.
    #[error("I/O error on {path}: {source}")]
    Io {
        /// The file or directory involved.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// A JSON column or an import document could not be (de)serialised.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// Encoding a thumbnail as JPEG failed.
    #[error("JPEG encoding failed: {0}")]
    Jpeg(String),
    /// The database was written by a newer FramePlayer.
    #[error("database schema version {found} is newer than this build supports ({supported})")]
    SchemaTooNew {
        /// `PRAGMA user_version` found in the file.
        found: i64,
        /// Latest version this build knows.
        supported: i64,
    },
    /// No media row with this id.
    #[error("media {0} not found")]
    MediaNotFound(MediaId),
    /// No playlist with this id.
    #[error("playlist {0} not found")]
    PlaylistNotFound(PlaylistId),
    /// A caller passed a value outside its allowed range.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    /// A [`MediaProber`](crate::MediaProber) could not read the media.
    #[error("probe failed: {0}")]
    Probe(String),
    /// The background worker thread could not be started.
    #[error("could not start worker thread: {0}")]
    Spawn(#[source] std::io::Error),
}

impl Error {
    /// Wraps an I/O error with the path it concerns.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Error {
        Error::Io {
            path: path.into(),
            source,
        }
    }
}

/// Result alias used throughout the crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;
