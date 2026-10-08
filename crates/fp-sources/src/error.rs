//! Error type shared by every source.
//!
//! URLs inside errors are always redacted (no passwords), so errors can be
//! logged and shown in the UI as they are.

use std::io;

/// Everything that can go wrong while browsing or reading a source.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Local or socket I/O failed.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    /// The server answered with an unexpected HTTP status.
    #[error("HTTP {status} from {url}")]
    HttpStatus {
        /// HTTP status code.
        status: u16,
        /// Redacted request URL.
        url: String,
    },
    /// The HTTP request could not be completed (connection, TLS, timeout).
    #[error("request to {url} failed: {message}")]
    Http {
        /// Redacted request URL.
        url: String,
        /// What went wrong.
        message: String,
    },
    /// The server rejected the credentials (HTTP 401/403, SMB logon failure).
    #[error("authentication failed for {0}")]
    Auth(String),
    /// The location does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// A URL or location string could not be understood.
    #[error("invalid location {location}: {reason}")]
    InvalidLocation {
        /// Redacted location.
        location: String,
        /// Why it was rejected.
        reason: String,
    },
    /// A server response (XML, JSON, HTML) could not be parsed.
    #[error("cannot parse {what}: {message}")]
    Parse {
        /// What was being parsed.
        what: String,
        /// Parser message.
        message: String,
    },
    /// The operation is not possible with this source or server.
    #[error("not supported: {0}")]
    Unsupported(String),
    /// The SMB client reported an error.
    #[error("SMB error: {0}")]
    Smb(String),
    /// SMB support is not available in this build.
    #[error("SMB unavailable: {0}")]
    SmbUnavailable(String),
    /// A source configuration is invalid.
    #[error("invalid source configuration: {0}")]
    Config(String),
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub(crate) fn parse(what: impl Into<String>, message: impl ToString) -> Error {
        Error::Parse {
            what: what.into(),
            message: message.to_string(),
        }
    }

    pub(crate) fn invalid(location: &str, reason: impl Into<String>) -> Error {
        Error::InvalidLocation {
            location: crate::urlutil::redact(location),
            reason: reason.into(),
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::parse("JSON", e)
    }
}

impl From<Error> for io::Error {
    fn from(e: Error) -> io::Error {
        match e {
            Error::Io(e) => e,
            Error::NotFound(_) => io::Error::new(io::ErrorKind::NotFound, e.to_string()),
            Error::Auth(_) => io::Error::new(io::ErrorKind::PermissionDenied, e.to_string()),
            Error::Unsupported(_) => io::Error::new(io::ErrorKind::Unsupported, e.to_string()),
            Error::InvalidLocation { .. } => {
                io::Error::new(io::ErrorKind::InvalidInput, e.to_string())
            }
            Error::Parse { .. } => io::Error::new(io::ErrorKind::InvalidData, e.to_string()),
            other => io::Error::other(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_to_io_error_kinds() {
        let e: io::Error = Error::NotFound("x".into()).into();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        let e: io::Error = Error::Unsupported("seek".into()).into();
        assert_eq!(e.kind(), io::ErrorKind::Unsupported);
        let e: io::Error = Error::Auth("u".into()).into();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn invalid_location_is_redacted() {
        let e = Error::invalid("http://bob:hunter2@nas/x", "bad");
        assert!(!e.to_string().contains("hunter2"), "{e}");
    }
}
