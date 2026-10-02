//! Library error type.

#[derive(Debug, thiserror::Error)]
pub enum LibraryError {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("source error: {0}")]
    Source(#[from] fp_sources::SourceError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("zip error: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid: {0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, LibraryError>;
