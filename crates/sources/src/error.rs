//! Error type shared by all sources.

/// Everything that can go wrong talking to a source.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("HTTP status {status} for {url}")]
    Status { status: u16, url: String },
    #[error("authentication failed or required")]
    Auth,
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid URI: {0}")]
    InvalidUri(String),
    /// A source type exists but this build or server can't do it, e.g.
    /// "built without SMB support".
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("credential store error: {0}")]
    Crypto(String),
    #[error("operation timed out")]
    Timeout,
}

impl SourceError {
    pub(crate) fn parse(msg: impl std::fmt::Display) -> Self {
        SourceError::Parse(msg.to_string())
    }
}

impl From<url::ParseError> for SourceError {
    fn from(e: url::ParseError) -> Self {
        SourceError::InvalidUri(e.to_string())
    }
}

impl From<quick_xml::Error> for SourceError {
    fn from(e: quick_xml::Error) -> Self {
        SourceError::Parse(format!("xml: {e}"))
    }
}

impl From<serde_json::Error> for SourceError {
    fn from(e: serde_json::Error) -> Self {
        SourceError::Parse(format!("json: {e}"))
    }
}

impl From<tokio::task::JoinError> for SourceError {
    fn from(e: tokio::task::JoinError) -> Self {
        SourceError::Io(std::io::Error::other(e))
    }
}

impl From<SourceError> for std::io::Error {
    fn from(e: SourceError) -> Self {
        match e {
            SourceError::Io(io) => io,
            SourceError::NotFound(p) => std::io::Error::new(std::io::ErrorKind::NotFound, p),
            SourceError::Timeout => std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out"),
            other => std::io::Error::other(other),
        }
    }
}

pub type Result<T> = std::result::Result<T, SourceError>;
